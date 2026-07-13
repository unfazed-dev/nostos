//! Regression: `PgWriteBack` binds `timestamptz`/`timestamp` columns via a typed
//! `chrono::DateTime<Utc>` (ADR-0019 follow-on; `Fix B`).
//!
//! Before the fix, `json_value_to_sql` mapped any non-UUID string to
//! `SqlValue::Text`, so an ISO8601 `created_at` (exactly what the Flutter demo's
//! `DateTime.now().toUtc().toIso8601String()` emits) was bound as a Rust
//! `String`. tokio-postgres uses extended-query prepared statements: it resolves
//! each parameter's declared type from the server and calls
//! `<String as ToSql>::to_sql(&Type::TIMESTAMPTZ, …)`, which **rejects non-text
//! types client-side** — the write came back `WriteResult{ok:false,
//! error:"backend: error serializing parameter N"}`, dead-lettered after 50
//! attempts, and the UI never reflected the add. The in-tree writeback e2e
//! omitted `created_at` (leaning on `DEFAULT now()`), so this path was
//! untested — the bug shipped.
//!
//! This drives the REAL `PgWriteBack` adapter through the WS socket (the exact
//! path the Flutter demo uses) and asserts the write is accepted AND the row —
//! including `created_at` — lands in Postgres.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_writeback_timestamp -- --nocapture --test-threads=1
//! ```
//!
//! Self-skips when `NOSTOS_E2E_PG` is unset (no real Postgres).

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::{AllowAnonymous, PgWriteBack};

const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

async fn spawn_server(
    slot: &str,
    publication: &str,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), slot, publication).expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg);
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let parsed: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            parsed
                .get(col)
                .and_then(|v| v.as_str())
                .map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    let mut allowlist = HashSet::new();
    allowlist.insert("tasks".to_string());
    let write_back: Arc<dyn nostos_application::ports::WriteBack> =
        Arc::new(PgWriteBack::new(&pg_url(), allowlist.clone()));

    let state = SyncRouterState::new(Arc::clone(&manager), Arc::new(AllowAnonymous::new()))
        .with_buffer(1024)
        .with_write_back(write_back)
        .with_write_tables(allowlist);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
    (addr, shutdown_tx, server, driver)
}

async fn shutdown_server(
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    driver: tokio::task::JoinHandle<()>,
    slot: &str,
) {
    let _ = shutdown.send(());
    driver.abort();
    let _ = server.await;
    let sql = sql_client().await;
    for _ in 0..40 {
        let active: bool = match tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await {
            Ok((c, conn)) => {
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                c.query_one(
                    "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot],
                )
                .await
                .ok()
                .is_some_and(|r| r.get::<_, bool>(0))
            }
            Err(_) => false,
        };
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;
}

/// Upsert a `tasks` row whose payload includes `created_at` as an ISO8601
/// string — exactly what `_nostos.write(..., 'created_at': DateTime.now().toUtc().toIso8601String())`
/// sends — and assert the write is accepted and the row (incl. created_at) lands.
#[tokio::test]
async fn timestamptz_iso8601_string_binds_and_lands() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("repro_created_at_{}", std::process::id());
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, "cairn_pub").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let row_id = uuid::Uuid::new_v4();
    let org_id = uuid::Uuid::new_v4();
    // Mirror Dart's DateTime.now().toUtc().toIso8601String(): ISO8601 with a
    // trailing Z and microsecond precision.
    let created_at_iso = "2026-07-12T14:30:00.123456Z";
    let title = format!("created-at-fix-{}", uuid::Uuid::new_v4());
    let write_frame = format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"upsert\",\
         \"pk\":\"{row_id}\",\
         \"payload\":{{\"id\":\"{row_id}\",\"org_id\":\"{org_id}\",\"title\":\"{title}\",\"created_at\":\"{created_at_iso}\"}},\
         \"client_write_id\":\"fixB\"}}"
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws.send(Message::Text(write_frame)).await.unwrap();

    // Collect the write_result ack.
    let mut ack: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(serde_json::Value::as_str) == Some("write_result") {
                    ack = Some(v);
                    break;
                }
            }
        }
    }

    // Independently verify what landed in PG (title + created_at round-trip).
    let landed: Option<(String, chrono::DateTime<chrono::Utc>)> = sql
        .query_opt(
            "SELECT title::text, created_at FROM tasks WHERE id = $1",
            &[&row_id],
        )
        .await
        .ok()
        .flatten()
        .map(|r| (r.get(0), r.get(1)));

    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;

    // Fix B contract: the bind is ACCEPTED (ok:true) — pre-fix this was
    // ok:false "error serializing parameter 2" and no row landed.
    let ack = ack.expect("expected a write_result ack frame");
    let ok = ack
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    assert!(
        ok,
        "write_result.ok should be true (timestamptz bind), got: {ack:?}"
    );

    let (landed_title, landed_ts) = landed.expect("row should have landed in PG");
    assert_eq!(landed_title, title, "landed title mismatch");
    assert_eq!(
        landed_ts.timestamp_micros(),
        chrono::DateTime::parse_from_rfc3339(created_at_iso)
            .unwrap()
            .timestamp_micros(),
        "created_at should round-trip to the sent instant"
    );
}
