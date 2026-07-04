//! End-to-end write-back test (D2, ADR-0013): a client writes a row over the
//! sync socket, the `PgWriteBack` adapter applies it to the source Postgres,
//! and the resulting change flows back out through logical replication to
//! every subscriber — including the writer itself (where the idempotent apply
//! is a no-op, so the row appears exactly once).
//!
//! This is the trust-boundary test: it proves the identifier validation +
//! allowlist + parameterized values hold against a real database, AND that the
//! write → replicate → fan-out loop is closed (a write is confirmed to the
//! writer and delivered to subscribers).
//!
//! ## Running
//!
//! Requires a live Postgres with logical replication (the repo's `make pg-up`)
//! and the `tasks` table (created by `docker/pg-init/01-sources.sql`). Skipped
//! unless `NOSTOS_E2E_PG=1` is set, so it never breaks PG-less CI:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_writeback -- --nocapture --test-threads=1
//! ```

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
use nostos_infra::PgWriteBack;

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

/// Connect a control-plane SQL client (tokio-postgres) for setup/teardown.
async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Spin up the in-process axum sync server with BOTH a PgReplicator driver
/// (read path) AND a PgWriteBack adapter (write path) wired to the same source
/// Postgres. The `tasks` table is allowlisted for writes.
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

    // Replicator driver: PgReplicator → FanOutService, real column extraction.
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

    // Write-back adapter: PgWriteBack against the same source, `tasks` allowlisted.
    let mut allowlist = HashSet::new();
    allowlist.insert("tasks".to_string());
    let write_back: Arc<dyn nostos_application::ports::WriteBack> =
        Arc::new(PgWriteBack::new(&pg_url(), allowlist.clone()));

    let state = SyncRouterState::new(
        Arc::clone(&manager),
        Arc::new(nostos_infra::AllowAnonymous::new()),
    )
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

/// Cleanly shut down a spawned server + its replicator driver, and wait for the
/// replication slot to be released by PG before returning. (Mirrors
/// e2e_pg_replication.rs — a second connection to the same slot hangs until
/// the first connection's lease is dropped.)
async fn shutdown_server(
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    driver: tokio::task::JoinHandle<()>,
    slot: &str,
) {
    let _ = shutdown.send(());
    driver.abort();
    let _ = server.await;
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
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Subscribe a client to `tasks` and collect frames (decoded as JSON) until
/// `timeout` elapses. Returns the parsed frames. Reused shape from the
/// contract tests but inline (so this test binary is self-contained).
async fn subscribe_and_collect(addr: SocketAddr, timeout: Duration) -> Vec<serde_json::Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                got.push(v);
            }
        }
    }
    got
}

/// The core D2 claim: client A writes a row over WS → `WriteResult ok` → the
/// row arrives via replication to client B (a subscriber) AND to A (where the
/// idempotent apply is a no-op — assert the row appears exactly once to A).
#[tokio::test]
async fn client_write_round_trips_through_replication() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_wb_{}", std::process::id());
    let publication = "cairn_pub";
    let sql = sql_client().await;
    // Clean slate: drop a leftover slot + the rows we might have written.
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    // Give the replicator a moment to open the connection + create the slot.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Two subscribers: A (the writer) and B (an observer). Both subscribe to
    // `tasks`. Collect their frames concurrently.
    let collect_a = tokio::spawn(subscribe_and_collect(addr, Duration::from_secs(8)));
    let collect_b = tokio::spawn(subscribe_and_collect(addr, Duration::from_secs(8)));
    // Give both clients time to connect + register before the write.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Client A writes a row over the WS. Use fresh UUIDs for id + org_id (the
    // tasks table requires both NOT NULL). The payload is column → value, the
    // same tuple-image shape the read path delivers.
    let row_id = uuid::Uuid::new_v4();
    let org_id = uuid::Uuid::new_v4();
    let title = format!("wb-row-{}", uuid::Uuid::new_v4());
    let write_frame = format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"upsert\",\
         \"pk\":\"{row_id}\",\
         \"payload\":{{\"id\":\"{row_id}\",\"org_id\":\"{org_id}\",\"title\":\"{title}\"}},\
         \"client_write_id\":\"wb1\"}}"
    );
    // A sends the write on ITS socket. We open a dedicated write socket that
    // subscribes first (write-before-subscribe is rejected), then writes.
    let write_ack = tokio::spawn({
        let frame = write_frame.clone();
        async move {
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
                .await
                .expect("ws connect (writer)");
            ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
                .await
                .unwrap();
            // Drain the subscribe window.
            let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
            ws.send(Message::Text(frame)).await.unwrap();
            // Read back the WriteResult ack.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                if let Ok(Some(Ok(Message::Binary(b)))) =
                    tokio::time::timeout(Duration::from_millis(200), ws.next()).await
                {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                        if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                            return Some(v);
                        }
                    }
                }
            }
            None
        }
    });

    let ack = write_ack.await.expect("writer task panicked");
    let frames_a = collect_a.await.unwrap();
    let frames_b = collect_b.await.unwrap();
    shutdown_server(shutdown, server, driver, &slot).await;
    // Clean up the row we wrote (so re-runs don't accumulate).
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    // 1. The write must have been acked ok.
    let ack = ack.expect("expected a WriteResult frame");
    assert_eq!(
        ack["type"], "write_result",
        "the ack must be a write_result frame"
    );
    assert_eq!(
        ack["ok"], true,
        "write must succeed against the real PG (got error: {:?})",
        ack["error"]
    );
    assert_eq!(ack["client_write_id"], "wb1");

    // 2. The row arrives via replication to client B (the observer).
    let b_saw_row = frames_b.iter().any(|f| frame_contains_title(f, &title));
    assert!(
        b_saw_row,
        "client B should receive the written row via replication; got {} frames",
        frames_b.len()
    );

    // 3. The row arrives to client A (the writer) too — and exactly once (the
    //    idempotent apply on A is a no-op for A's own write, but the row is
    //    still delivered once via replication because A subscribed).
    let a_matches: Vec<&serde_json::Value> = frames_a
        .iter()
        .filter(|f| frame_contains_title(f, &title))
        .collect();
    assert_eq!(
        a_matches.len(),
        1,
        "client A should receive its own written row EXACTLY once (idempotent apply); \
         got {} matches across {} frames",
        a_matches.len(),
        frames_a.len()
    );
}

/// Does a wire frame's decoded payload contain the given title string?
/// (The frame payload is hex-encoded JSON; decode then substring-match.)
fn frame_contains_title(frame: &serde_json::Value, title: &str) -> bool {
    let hex = frame.get("payload").and_then(|v| v.as_str()).unwrap_or("");
    if hex.is_empty() {
        return false;
    }
    let bytes = common::decode_payload_hex(hex);
    String::from_utf8_lossy(&bytes).contains(title)
}

/// Idempotent delete: a delete of an absent row is success. (Guards the v1
/// contract that "missing row is success" — so a redelivered delete doesn't
/// surface an error to the client.)
#[tokio::test]
async fn delete_of_missing_row_is_success() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_wb_del_{}", std::process::id());
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // A fresh UUID that does NOT exist in tasks.
    let ghost = uuid::Uuid::new_v4();
    let delete_frame = format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"delete\",\
         \"pk\":\"{ghost}\",\"client_write_id\":\"d1\"}}"
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws.send(Message::Text(delete_frame)).await.unwrap();

    let mut ack: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                    ack = Some(v);
                    break;
                }
            }
        }
    }
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let ack = ack.expect("expected a WriteResult frame");
    assert_eq!(
        ack["ok"], true,
        "delete of a missing row must be ok (idempotent)"
    );
}

/// Injection attempt: a column name that tries to break out of the identifier
/// quote must be rejected as InvalidPayload — the SQL is NEVER built. This is
/// the trust-boundary proof against a real database.
#[tokio::test]
async fn injection_column_name_is_rejected() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_wb_inj_{}", std::process::id());
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // A column name with an embedded double-quote + SQL comment — the classic
    // identifier-tampering attempt. The regex must reject it.
    let malicious = format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"upsert\",\
         \"pk\":\"{}\",\
         \"payload\":{{\"title\\\"; --\":\"x\"}},\
         \"client_write_id\":\"inj1\"}}",
        uuid::Uuid::new_v4()
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws.send(Message::Text(malicious)).await.unwrap();

    let mut ack: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                    ack = Some(v);
                    break;
                }
            }
        }
    }
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let ack = ack.expect("expected a WriteResult frame");
    assert_eq!(
        ack["ok"], false,
        "the injection column name must be rejected"
    );
    let err = ack["error"].as_str().expect("error string present");
    assert!(
        err.contains("invalid payload"),
        "error must mention 'invalid payload', got: {err}"
    );
}
