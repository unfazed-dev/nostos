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

use nostos_application::ports::SyncAuth;
use nostos_application::ports::WriteBack;
use nostos_application::ports::WriteBackError;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, Principal, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::{AllowAnonymous, PgWriteBack};

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
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
/// Postgres. The `tasks` table is allowlisted for writes. Anonymous auth, no
/// tenant column — the pre-ADR-0018 shape used by the original D2 tests.
async fn spawn_server(
    slot: &str,
    publication: &str,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_server_with(slot, publication, Arc::new(AllowAnonymous::new()), None).await
}

/// Like [`spawn_server`] but with an explicit `SyncAuth` + tenant column
/// (ADR-0018) — used by the tenant-enforcement tests below.
async fn spawn_server_with(
    slot: &str,
    publication: &str,
    auth: Arc<dyn SyncAuth>,
    tenant_column: Option<&str>,
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

    let mut state = SyncRouterState::new(Arc::clone(&manager), auth)
        .with_buffer(1024)
        .with_write_back(write_back)
        .with_write_tables(allowlist);
    if let Some(col) = tenant_column {
        state = state.with_tenant_column(col);
    }
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

/// A `SyncAuth` test-double: the token IS the tenant name, and mints a
/// principal whose `tenant_id` is a UUID string bound at construction. The
/// `tasks.org_id` column is `UUID NOT NULL` (`docker/pg-init/01-sources.sql`),
/// so the tenant value must be a real UUID for the stamped/guarded SQL to
/// bind correctly (ADR-0013's typed-binding addendum) — a bare tenant name
/// like `"acme"` would fail as "column org_id is of type uuid but parameter
/// is of type text".
struct TwoTenantAuth {
    tenants: std::collections::HashMap<String, Principal>,
}

impl TwoTenantAuth {
    fn new(acme_org: &str, other_org: &str) -> Self {
        let mut tenants = std::collections::HashMap::new();
        tenants.insert(
            "acme".to_string(),
            Principal::new("acme-user", acme_org.to_string()),
        );
        tenants.insert(
            "other".to_string(),
            Principal::new("other-user", other_org.to_string()),
        );
        Self { tenants }
    }
}

#[async_trait::async_trait]
impl SyncAuth for TwoTenantAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        self.tenants.get(token).cloned()
    }
}

/// Connect with `?token=`, subscribe to `tasks`, send `write_frame`, and
/// return the first `write_result` frame (or `None` on timeout).
async fn write_and_await_ack(
    addr: SocketAddr,
    token: &str,
    write_frame: String,
) -> Option<serde_json::Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync?token={token}"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws.send(Message::Text(write_frame)).await.unwrap();
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

/// A bare upsert frame: `payload` is `{"id":..,"org_id":..,"title":..}`.
fn upsert_frame(id: uuid::Uuid, org_id: &str, title: &str, client_write_id: &str) -> String {
    format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"upsert\",\
         \"pk\":\"{id}\",\
         \"payload\":{{\"id\":\"{id}\",\"org_id\":\"{org_id}\",\"title\":\"{title}\"}},\
         \"client_write_id\":\"{client_write_id}\"}}"
    )
}

fn delete_frame(id: uuid::Uuid, client_write_id: &str) -> String {
    format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"delete\",\
         \"pk\":\"{id}\",\"client_write_id\":\"{client_write_id}\"}}"
    )
}

/// Query the `org_id`/`title` of a row directly (bypassing the sync socket) —
/// used to assert what actually landed in Postgres, independent of what the
/// `WriteResult` frame claimed.
async fn fetch_row(sql: &tokio_postgres::Client, id: uuid::Uuid) -> Option<(uuid::Uuid, String)> {
    sql.query_opt("SELECT org_id, title FROM tasks WHERE id = $1", &[&id])
        .await
        .expect("query tasks")
        .map(|row| (row.get(0), row.get(1)))
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
                .is_ok_and(|r| r.get::<_, bool>(0))
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
    if nostos_infra::env::var(E2E_FLAG).is_err() {
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
    common::frame_payload_contains(frame, title)
}

/// Idempotent delete: a delete of an absent row is success. (Guards the v1
/// contract that "missing row is success" — so a redelivered delete doesn't
/// surface an error to the client.)
#[tokio::test]
async fn delete_of_missing_row_is_success() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
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
    if nostos_infra::env::var(E2E_FLAG).is_err() {
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

// ===========================================================================
// ADR-0018: write-path tenant enforcement. Two principals (acme / other),
// `NOSTOS_TENANT_COLUMN=org_id`, against the real `tasks` table.
// ===========================================================================

/// A fresh, unique replication slot name per tenant-enforcement test — these
/// tests share the same test binary so slot names must not collide.
fn tenant_test_slot(name: &str) -> String {
    format!("e2e_wb_tenant_{name}_{}", std::process::id())
}

/// A new insert stamps the tenant column to the CALLER's tenant, even when
/// the client's own payload claims a different tenant (ADR-0018's
/// force-stamp: the client's value is never trusted).
#[tokio::test]
async fn cross_tenant_insert_is_stamped_to_callers_tenant() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("insert");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // acme upserts a BRAND NEW row, but its own payload claims org_id=other —
    // an attempted tenant-claim on a fresh insert.
    let row_id = uuid::Uuid::new_v4();
    let frame = upsert_frame(row_id, &other_org, "stamped-row", "stamp1");
    let ack = write_and_await_ack(addr, "acme", frame).await;

    let stored = fetch_row(&sql, row_id).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let ack = ack.expect("expected a WriteResult frame");
    assert_eq!(
        ack["ok"], true,
        "a fresh insert always succeeds regardless of the claimed tenant \
         (it's force-stamped, not rejected): {:?}",
        ack["error"]
    );
    let (stored_org, stored_title) = stored.expect("row must exist after insert");
    assert_eq!(
        stored_org.to_string(),
        acme_org,
        "the stored org_id must be the CALLER's tenant (acme), not the \
         client-claimed one (other) — the server force-stamps it"
    );
    assert_eq!(stored_title, "stamped-row");
}

/// An upsert whose pk already exists under a DIFFERENT tenant is rejected
/// (`Forbidden`), and the existing row is left untouched — the write must
/// not silently change the row's tenant ownership.
#[tokio::test]
async fn cross_tenant_upsert_conflict_is_rejected() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("conflict");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // acme creates a row.
    let row_id = uuid::Uuid::new_v4();
    let create = upsert_frame(row_id, &acme_org, "acme-owned", "create1");
    let create_ack = write_and_await_ack(addr, "acme", create)
        .await
        .expect("create ack");
    assert_eq!(
        create_ack["ok"], true,
        "acme's own-tenant create must succeed"
    );

    // other attempts to upsert the SAME pk — a cross-tenant conflict attempt.
    let attack = upsert_frame(row_id, &other_org, "hijacked", "attack1");
    let attack_ack = write_and_await_ack(addr, "other", attack).await;

    let stored = fetch_row(&sql, row_id).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let attack_ack = attack_ack.expect("expected a WriteResult frame");
    assert_eq!(
        attack_ack["ok"], false,
        "a cross-tenant upsert conflict must be rejected, not silently applied"
    );
    let err = attack_ack["error"].as_str().expect("error string present");
    assert!(
        err.contains("forbidden"),
        "error must be a Forbidden rejection, got: {err}"
    );
    let (stored_org, stored_title) = stored.expect("row must still exist");
    assert_eq!(
        stored_org.to_string(),
        acme_org,
        "ownership must NOT have transferred to the attacking tenant"
    );
    assert_eq!(
        stored_title, "acme-owned",
        "the row's content must be UNCHANGED by the rejected write"
    );
}

/// A delete of a pk that exists under a DIFFERENT tenant is rejected
/// (`Forbidden`) and the row is left in place — distinct from deleting a pk
/// that never existed at all, which stays idempotent-success (see
/// `delete_of_missing_row_is_success` and
/// `cross_tenant_delete_of_absent_row_is_idempotent_success` below).
#[tokio::test]
async fn cross_tenant_delete_is_rejected_row_survives() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("delete");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let row_id = uuid::Uuid::new_v4();
    let create = upsert_frame(row_id, &acme_org, "acme-owned", "create2");
    let create_ack = write_and_await_ack(addr, "acme", create)
        .await
        .expect("create ack");
    assert_eq!(create_ack["ok"], true);

    // other attempts to delete acme's row.
    let del = delete_frame(row_id, "del1");
    let del_ack = write_and_await_ack(addr, "other", del).await;

    let stored = fetch_row(&sql, row_id).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let del_ack = del_ack.expect("expected a WriteResult frame");
    assert_eq!(
        del_ack["ok"], false,
        "a cross-tenant delete must be rejected, not silently no-op'd"
    );
    let err = del_ack["error"].as_str().expect("error string present");
    assert!(
        err.contains("forbidden"),
        "error must be a Forbidden rejection, got: {err}"
    );
    assert!(stored.is_some(), "the row must survive the rejected delete");
}

/// A delete of a pk that genuinely does not exist stays idempotent-success
/// EVEN under tenant scoping — the CTE-based guard (write_back.rs) must
/// distinguish "belongs to someone else" (Forbidden, above) from "never
/// existed" (success) via a single round trip, without an extra query.
#[tokio::test]
async fn cross_tenant_delete_of_absent_row_is_idempotent_success() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("delabsent");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let ghost = uuid::Uuid::new_v4(); // never created by anyone
    let del = delete_frame(ghost, "del-ghost");
    let del_ack = write_and_await_ack(addr, "acme", del).await;

    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let del_ack = del_ack.expect("expected a WriteResult frame");
    assert_eq!(
        del_ack["ok"], true,
        "deleting a pk that never existed must stay idempotent-success even \
         with tenant scoping active: {:?}",
        del_ack["error"]
    );
}

/// The straightforward case: a principal's own-tenant upsert then delete both
/// flow normally (no rejection, no stamping surprises for values already
/// matching the caller's tenant).
#[tokio::test]
async fn own_tenant_writes_flow_normally() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("owntenant");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let row_id = uuid::Uuid::new_v4();
    let create = upsert_frame(row_id, &acme_org, "v1", "own1");
    let create_ack = write_and_await_ack(addr, "acme", create)
        .await
        .expect("create ack");
    assert_eq!(create_ack["ok"], true);

    // Update the same row (own tenant) — must succeed and change the title.
    let update = upsert_frame(row_id, &acme_org, "v2", "own2");
    let update_ack = write_and_await_ack(addr, "acme", update)
        .await
        .expect("update ack");
    assert_eq!(update_ack["ok"], true, "own-tenant update must succeed");
    let after_update = fetch_row(&sql, row_id).await;

    // Delete the same row (own tenant) — must succeed and remove it.
    let del = delete_frame(row_id, "own3");
    let del_ack = write_and_await_ack(addr, "acme", del)
        .await
        .expect("delete ack");
    let after_delete = fetch_row(&sql, row_id).await;

    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (_, updated_title) = after_update.expect("row exists after own-tenant update");
    assert_eq!(updated_title, "v2");
    assert_eq!(del_ack["ok"], true, "own-tenant delete must succeed");
    assert!(
        after_delete.is_none(),
        "the row must be gone after the own-tenant delete"
    );
}

// ===========================================================================
// P3 — PATCH (column-level UPDATE). A patch updates
// only the columns present in its payload of an EXISTING row; never inserts;
// idempotent on an absent row; cross-tenant patch is Forbidden (ADR-0018).
// ===========================================================================

/// Fetch `(title, completed)` for a row — used by the patch tests to verify
/// that a patch updates ONLY the columns present in its payload (the other
/// columns are untouched).
async fn fetch_row_title_completed(
    sql: &tokio_postgres::Client,
    id: uuid::Uuid,
) -> Option<(String, bool)> {
    sql.query_opt("SELECT title, completed FROM tasks WHERE id = $1", &[&id])
        .await
        .expect("query tasks")
        .map(|row| (row.get(0), row.get(1)))
}

/// Build a patch write frame whose payload carries ONLY `title` — the
/// canonical partial-column case. Mirrors `upsert_frame`'s shape but with
/// `op:"patch"` and a single column.
fn patch_frame(id: uuid::Uuid, title: &str, client_write_id: &str) -> String {
    format!(
        "{{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"patch\",\
         \"pk\":\"{id}\",\
         \"payload\":{{\"title\":\"{title}\"}},\
         \"client_write_id\":\"{client_write_id}\"}}"
    )
}

/// A patch updates ONLY the columns present in its payload; other columns are
/// untouched (P3 column-level PATCH).
#[tokio::test]
async fn patch_updates_only_specified_columns() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_wb_patch_{}", std::process::id());
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create a row with title="original" via upsert. `org_id` is NOT NULL on
    // the tasks schema, so the upsert payload must carry one even though this
    // test runs without tenant enforcement.
    let row_id = uuid::Uuid::new_v4();
    let org = uuid::Uuid::new_v4();
    let create = upsert_frame(row_id, &org.to_string(), "original", "p1");
    let create_ack = write_and_await_ack(addr, "anon", create)
        .await
        .expect("create ack");
    assert_eq!(create_ack["ok"], true, "create must succeed");

    // Patch ONLY the title. `completed` (default FALSE) is absent from the
    // patch payload — it must be left untouched.
    let patch = patch_frame(row_id, "patched", "p2");
    let patch_ack = write_and_await_ack(addr, "anon", patch)
        .await
        .expect("patch ack");
    let after = fetch_row_title_completed(&sql, row_id).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    assert_eq!(
        patch_ack["ok"], true,
        "patch of an existing row must succeed"
    );
    let (title, completed) = after.expect("row must still exist after patch");
    assert_eq!(title, "patched", "patch must update the title column");
    assert!(
        !completed,
        "patch must NOT touch columns absent from its payload"
    );
}

/// A patch of a row that does not exist is success (idempotent) — mirrors
/// `delete_of_missing_row_is_success`. A redelivered patch after the row is
/// gone must not surface an error to the client.
#[tokio::test]
async fn patch_on_absent_row_is_ok() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_wb_patch_absent_{}", std::process::id());
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // A fresh UUID that does NOT exist in tasks.
    let ghost = uuid::Uuid::new_v4();
    let patch = patch_frame(ghost, "ghost", "pa1");
    let ack = write_and_await_ack(addr, "anon", patch).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let ack = ack.expect("expected a WriteResult frame");
    assert_eq!(
        ack["ok"], true,
        "patch of an absent row must be ok (idempotent)"
    );
}

/// A patch of a pk that exists under a DIFFERENT tenant is rejected
/// (`Forbidden`) and the row is left unchanged — the patch must not silently
/// mutate another tenant's row (ADR-0018).
#[tokio::test]
async fn cross_tenant_patch_is_rejected_row_unchanged() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = tenant_test_slot("patch");
    let publication = "cairn_pub";
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let auth = Arc::new(TwoTenantAuth::new(&acme_org, &other_org));
    let (addr, shutdown, server, driver) =
        spawn_server_with(&slot, publication, auth, Some("org_id")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // acme creates a row.
    let row_id = uuid::Uuid::new_v4();
    let create = upsert_frame(row_id, &acme_org, "acme-owned", "cp1");
    let create_ack = write_and_await_ack(addr, "acme", create)
        .await
        .expect("create ack");
    assert_eq!(
        create_ack["ok"], true,
        "acme's own-tenant create must succeed"
    );

    // other attempts to patch acme's row. The server force-stamps org_id to
    // other_org (the attacker's own tenant), so the tenant-guarded WHERE
    // can never match acme's row — a cross-tenant probe, rejected as Forbidden.
    let attack = patch_frame(row_id, "hijacked", "cp2");
    let attack_ack = write_and_await_ack(addr, "other", attack).await;

    let stored = fetch_row(&sql, row_id).await;
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .execute("DELETE FROM tasks WHERE id = $1", &[&row_id])
        .await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let attack_ack = attack_ack.expect("expected a WriteResult frame");
    assert_eq!(
        attack_ack["ok"], false,
        "a cross-tenant patch must be rejected, not silently applied"
    );
    let err = attack_ack["error"].as_str().expect("error string present");
    assert!(
        err.contains("forbidden"),
        "error must be a Forbidden rejection, got: {err}"
    );
    let (stored_org, stored_title) = stored.expect("row must still exist");
    assert_eq!(
        stored_org.to_string(),
        acme_org,
        "ownership must NOT have changed"
    );
    assert_eq!(
        stored_title, "acme-owned",
        "the row's content must be UNCHANGED by the rejected patch"
    );
}

/// ADR-0030 Decision 1: `PgWriteBack::increment` emits
/// `UPDATE ... SET col = col + $delta WHERE id = $pk`, so Postgres serializes
/// concurrent increments — no client read-modify-write, no lost update. Direct
/// adapter test (no server/replication machinery): seeds a row, applies two
/// +1 deltas, asserts the serialized total; plus the idempotent-absent +
/// payload-validation branches.
#[tokio::test]
async fn increment_serializes_concurrent_deltas_server_side() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(
            "DROP TABLE IF EXISTS nostosincr; \
             CREATE TABLE nostosincr (id text PRIMARY KEY, n bigint NOT NULL DEFAULT 0);",
        )
        .await;

    let mut allowlist = HashSet::new();
    allowlist.insert("nostosincr".to_string());
    let wb = PgWriteBack::new(&pg_url(), allowlist);

    // Seed n=0, then two +1 increments → Postgres serializes to n=2.
    wb.upsert("nostosincr", "a", r#"{"n":0}"#, None)
        .await
        .expect("seed upsert");
    wb.increment("nostosincr", "a", r#"{"field":"n","delta":1}"#, None)
        .await
        .expect("first increment");
    wb.increment("nostosincr", "a", r#"{"field":"n","delta":1}"#, None)
        .await
        .expect("second increment");

    let n: i64 = sql
        .query_one("SELECT n FROM nostosincr WHERE id = 'a'", &[])
        .await
        .expect("seeded row exists")
        .get(0);
    assert_eq!(
        n, 2,
        "two +1 increments serialize to +2 (no lost update — the whole point)"
    );

    // Increment of an absent row is idempotent success (0 rows affected),
    // mirroring patch-of-missing / delete-of-missing.
    wb.increment("nostosincr", "ghost", r#"{"field":"n","delta":5}"#, None)
        .await
        .expect("increment of absent row is idempotent success");

    // Payload validation: missing delta rejected; the pk column is not
    // incrementable (would corrupt row identity).
    assert!(
        wb.increment("nostosincr", "a", r#"{"field":"n"}"#, None)
            .await
            .is_err(),
        "missing delta must be InvalidPayload"
    );
    assert!(
        wb.increment("nostosincr", "a", r#"{"field":"id","delta":1}"#, None)
            .await
            .is_err(),
        "incrementing the pk column must be rejected"
    );

    let _ = sql.batch_execute("DROP TABLE nostosincr;").await;
}

/// ADR-0030 slice 3: `PgWriteBack` must MERGE (not clobber) when applying a
/// flushed OR-set upsert to a configured table — else a client's add loses
/// other clients' elements server-side. Direct adapter test (no replication):
/// two clients each add a distinct element to the same shared row; assert both
/// survive the second write (a clobber would leave only the second).
#[tokio::test]
async fn or_set_writeback_merges_concurrent_client_adds_server_side() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(
            "DROP TABLE IF EXISTS nostosorset; \
             CREATE TABLE nostosorset (id text PRIMARY KEY, members jsonb);",
        )
        .await;

    let mut allowlist = HashSet::new();
    allowlist.insert("nostosorset".to_string());
    let mut or_set_columns = std::collections::HashMap::new();
    or_set_columns.insert("nostosorset".to_string(), "members".to_string());
    let wb = PgWriteBack::new(&pg_url(), allowlist).with_or_set_columns(or_set_columns);

    // Two clients each add a distinct element to the shared community row.
    let alice = nostos_domain::OrSetPayload {
        elements: vec![nostos_domain::OrSetElement {
            v: "alice".to_string(),
            h: nostos_domain::Hlc::mint(None, 1),
            d: None,
        }],
    };
    let bob = nostos_domain::OrSetPayload {
        elements: vec![nostos_domain::OrSetElement {
            v: "bob".to_string(),
            h: nostos_domain::Hlc::mint(None, 2),
            d: None,
        }],
    };
    wb.upsert(
        "nostosorset",
        "community-1",
        &serde_json::to_string(&alice).expect("serialize alice"),
        None,
    )
    .await
    .expect("alice add");
    wb.upsert(
        "nostosorset",
        "community-1",
        &serde_json::to_string(&bob).expect("serialize bob"),
        None,
    )
    .await
    .expect("bob add");

    // A merge converges to {alice, bob}; a clobber would leave only {bob}.
    let id = "community-1".to_string();
    let row = sql
        .query_one("SELECT members::text FROM nostosorset WHERE id = $1", &[&id])
        .await
        .expect("community row exists after both adds");
    let members_text: String = row
        .get::<_, Option<String>>(0)
        .expect("members column populated");
    let present =
        nostos_domain::present_elements(members_text.as_bytes()).expect("parse merged element set");
    assert!(
        present.contains(&"alice".to_string()),
        "alice was clobbered by bob's write: {present:?}"
    );
    assert!(
        present.contains(&"bob".to_string()),
        "bob missing after his own write: {present:?}"
    );

    let _ = sql.batch_execute("DROP TABLE nostosorset;").await;
}

/// ADR-0018 × ADR-0030: tenant-scoped OR-set merge. Two replicas of the SAME
/// tenant converge (both elements survive, row stamped with the principal's
/// tenant); a DIFFERENT tenant's merge on the same pk is `Forbidden` and
/// leaves the row untouched — no cross-tenant read, write, or ownership
/// change. Direct adapter test (no replication), mirroring
/// `or_set_writeback_merges_concurrent_client_adds_server_side`.
#[tokio::test]
async fn or_set_writeback_tenant_scoped_merge_converges_and_isolates() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(
            "DROP TABLE IF EXISTS nostosorsettenant; \
             CREATE TABLE nostosorsettenant (id text PRIMARY KEY, tenant_id text NOT NULL, members jsonb);",
        )
        .await;

    let mut allowlist = HashSet::new();
    allowlist.insert("nostosorsettenant".to_string());
    let mut or_set_columns = std::collections::HashMap::new();
    or_set_columns.insert("nostosorsettenant".to_string(), "members".to_string());
    let wb = PgWriteBack::new(&pg_url(), allowlist).with_or_set_columns(or_set_columns);

    let acme = nostos_domain::TenantScope::new("tenant_id", "acme");
    let other = nostos_domain::TenantScope::new("tenant_id", "other");
    let element = |v: &str, tick: u64| {
        serde_json::to_string(&nostos_domain::OrSetPayload {
            elements: vec![nostos_domain::OrSetElement {
                v: v.to_string(),
                h: nostos_domain::Hlc::mint(None, tick),
                d: None,
            }],
        })
        .expect("serialize element")
    };

    // Two replicas of tenant acme each add a distinct element to one row.
    wb.upsert(
        "nostosorsettenant",
        "shared-1",
        &element("alice", 1),
        Some(acme),
    )
    .await
    .expect("alice add");
    wb.upsert(
        "nostosorsettenant",
        "shared-1",
        &element("bob", 2),
        Some(acme),
    )
    .await
    .expect("bob add");

    // Converged AND stamped: the row belongs to acme and holds both elements.
    let id = "shared-1".to_string();
    let row = sql
        .query_one(
            "SELECT tenant_id, members::text FROM nostosorsettenant WHERE id = $1",
            &[&id],
        )
        .await
        .expect("shared row exists after both adds");
    let tenant_id: String = row.get(0);
    assert_eq!(
        tenant_id, "acme",
        "merge insert must stamp the principal's tenant"
    );
    let members_text: String = row.get::<_, Option<String>>(1).expect("members populated");
    let present =
        nostos_domain::present_elements(members_text.as_bytes()).expect("parse merged set");
    assert!(
        present.contains(&"alice".to_string()),
        "alice clobbered: {present:?}"
    );
    assert!(
        present.contains(&"bob".to_string()),
        "bob missing: {present:?}"
    );

    // A DIFFERENT tenant merging the same pk: Forbidden, and acme's state is
    // byte-identical afterwards (no read-fold, no clobber, no ownership move).
    let err = wb
        .upsert(
            "nostosorsettenant",
            "shared-1",
            &element("carol", 3),
            Some(other),
        )
        .await;
    assert!(
        matches!(err, Err(WriteBackError::Forbidden(_))),
        "cross-tenant merge must be Forbidden, got {err:?}"
    );
    let after: String = sql
        .query_one(
            "SELECT members::text FROM nostosorsettenant WHERE id = $1",
            &[&id],
        )
        .await
        .expect("row still there")
        .get::<_, Option<String>>(0)
        .expect("members still populated");
    assert_eq!(
        after, members_text,
        "cross-tenant merge must not touch the row"
    );

    // Isolation cuts both ways: other's merge on a FRESH pk stamps other.
    wb.upsert(
        "nostosorsettenant",
        "other-1",
        &element("carol", 4),
        Some(other),
    )
    .await
    .expect("carol add on her own row");
    let other_tenant: String = sql
        .query_one(
            "SELECT tenant_id FROM nostosorsettenant WHERE id = 'other-1'",
            &[],
        )
        .await
        .expect("other row exists")
        .get(0);
    assert_eq!(other_tenant, "other");

    let _ = sql.batch_execute("DROP TABLE nostosorsettenant;").await;
}

/// ADR-0018 × ADR-0030 addendum: tenant-scoped PN-counter merge. Same-tenant
/// replicas SUM (per-replica elementwise max, then p−n across entries); a
/// cross-tenant merge on the same pk is Forbidden. The tenant values are
/// UUID-SHAPED strings on a TEXT column on purpose — the prepare +
/// coerce_params path must undo the uuid shape-guess (the kit's canonical
/// user ids are v5 UUIDs on text tenant columns).
#[tokio::test]
async fn counter_writeback_tenant_scoped_merge_sums_and_isolates() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(
            "DROP TABLE IF EXISTS nostoscountertenant; \
             CREATE TABLE nostoscountertenant (id text PRIMARY KEY, tenant_id text NOT NULL, value jsonb);",
        )
        .await;

    let mut allowlist = HashSet::new();
    allowlist.insert("nostoscountertenant".to_string());
    let mut counter_columns = std::collections::HashMap::new();
    counter_columns.insert("nostoscountertenant".to_string(), "value".to_string());
    let wb = PgWriteBack::new(&pg_url(), allowlist).with_counter_columns(counter_columns);

    let acme = nostos_domain::TenantScope::new("tenant_id", "11111111-1111-1111-1111-111111111111");
    let other = nostos_domain::TenantScope::new("tenant_id", "22222222-2222-2222-2222-222222222222");
    let counts = |r: &str, p: u64, n: u64| {
        serde_json::to_string(&nostos_domain::PnCounterPayload {
            entries: vec![nostos_domain::PnEntry {
                r: r.to_string(),
                p,
                n,
            }],
        })
        .expect("serialize counts")
    };

    // Two replicas of the same tenant: +5 and −2 (p=3,n=2 nets +1) → total 6.
    wb.upsert(
        "nostoscountertenant",
        "counter-1",
        &counts("r1", 5, 0),
        Some(acme),
    )
    .await
    .expect("r1 flush");
    wb.upsert(
        "nostoscountertenant",
        "counter-1",
        &counts("r2", 3, 2),
        Some(acme),
    )
    .await
    .expect("r2 flush");

    let id = "counter-1".to_string();
    let row = sql
        .query_one(
            "SELECT tenant_id, value::text FROM nostoscountertenant WHERE id = $1",
            &[&id],
        )
        .await
        .expect("counter row exists");
    let tenant_id: String = row.get(0);
    assert_eq!(tenant_id, acme.value, "merge insert must stamp the tenant");
    let value_text: String = row.get::<_, Option<String>>(1).expect("value populated");
    let total = nostos_domain::counter_value(value_text.as_bytes()).expect("parse counter");
    assert_eq!(total, 6, "5 + (3−2) across replicas must sum, got {total}");

    // Cross-tenant merge on the same pk: Forbidden, state untouched.
    let err = wb
        .upsert(
            "nostoscountertenant",
            "counter-1",
            &counts("r9", 100, 0),
            Some(other),
        )
        .await;
    assert!(
        matches!(err, Err(WriteBackError::Forbidden(_))),
        "cross-tenant counter merge must be Forbidden, got {err:?}"
    );
    let after: String = sql
        .query_one(
            "SELECT value::text FROM nostoscountertenant WHERE id = $1",
            &[&id],
        )
        .await
        .expect("row still there")
        .get::<_, Option<String>>(0)
        .expect("value still populated");
    assert_eq!(
        after, value_text,
        "cross-tenant merge must not touch the row"
    );

    let _ = sql.batch_execute("DROP TABLE nostoscountertenant;").await;
}
