//! D3 — durable client outbox: offline writes survive restarts and flush on
//! reconnect.
//!
//! Proves the three properties the outbox exists for:
//! 1. **Write-while-offline succeeds.** `SyncClient::write` enqueues to the
//!    durable outbox and returns `Ok` even with no server reachable. The caller
//!    never blocks on the network to capture user intent.
//! 2. **The queue is durable.** A fresh `SqliteStorage` handle on the SAME file
//!    sees the enqueued write (it's in `cairn_outbox`, committed to disk). Drop
//!    the whole client process and the row is still there.
//! 3. **The queue flushes on reconnect.** Once the server is up, the client's
//!    connected loop drains `pending()` in order, sends each as a `Write` frame,
//!    and `mark_done`s on `WriteResult{ok:true}`. The written row then flows back
//!    through normal replication and lands in the client's `cairn_data` table —
//!    the round-trip.
//!
//! The kill-restart variant drops the client process entirely between the
//! enqueue and the connect, recreating it from the same SQLite file. The queue
//! survived, so the flush still happens.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::routing::get;
use bytes::Bytes;
use nostos_application::ports::{ReplicatorStream, SessionStore, WriteBack, WriteBackError};
use nostos_application::FanOutService;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{Outbox, PendingWrite, WriteOp};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp, Tier};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::AllowAnonymous;

use common::tempfile_dir;

/// A `WriteBack` that RECORDS each accepted write as a `ReplicationEvent` into
/// a shared channel. The test later drains that channel through the real
/// `FanOutService`, so the written row flows back to subscribers (including the
/// writer) exactly as it would from Postgres logical replication — the honest
/// round-trip path.
///
/// This stands in for `PgWriteBack` (which writes to Postgres and lets logical
/// replication carry the change back out). It lets the test prove the full
/// loop — outbox → Write frame → WriteResult ack → mark_done → row arrives via
/// replication → applied to client SQLite — without a database.
struct RecordingWriteBack {
    /// Captured writes (one ReplicationEvent per accepted upsert/delete), in
    /// arrival order. Drained by the test's fan-out pump.
    captured: tokio::sync::Mutex<Vec<ReplicationEvent>>,
    /// A monotonic LSN source for the synthetic events we emit.
    next_lsn: std::sync::atomic::AtomicU64,
}

impl RecordingWriteBack {
    fn new() -> Self {
        Self {
            captured: tokio::sync::Mutex::new(Vec::new()),
            next_lsn: std::sync::atomic::AtomicU64::new(100),
        }
    }

    /// Take the captured writes out (the test fan-out drains them).
    async fn drain(&self) -> Vec<ReplicationEvent> {
        std::mem::take(&mut *self.captured.lock().await)
    }
}

#[async_trait]
impl WriteBack for RecordingWriteBack {
    async fn upsert(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        // Realistic shape: an Insert carrying the JSON tuple image (the same
        // tuple-image the read path delivers). This is what the client will
        // apply from the round-trip replication frame.
        let lsn = self
            .next_lsn
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::copy_from_slice(payload_json.as_bytes()),
            },
        );
        self.captured.lock().await.push(ev);
        Ok(())
    }

    async fn delete(
        &self,
        table: &str,
        pk: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        let lsn = self
            .next_lsn
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Delete {
                table: table.to_string(),
                pk: pk.to_string(),
                old_payload: None,
            },
        );
        self.captured.lock().await.push(ev);
        Ok(())
    }

    async fn patch(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        // P3 PowerSync PATCH parity: a patch is a column-level UPDATE; record
        // it as an Update carrying the partial tuple image (the columns present
        // in the payload — absent columns are untouched, same as the real
        // PgWriteBack).
        let lsn = self
            .next_lsn
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Update {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::copy_from_slice(payload_json.as_bytes()),
            },
        );
        self.captured.lock().await.push(ev);
        Ok(())
    }
}

/// Spawn the in-process server wired with the recording write-back + the
/// `tasks` allowlist (so client writes pass the allowlist gate and reach the
/// adapter). Returns the bound address, the server handle, the shared session
/// store, and the write-back adapter (so the test can drain captured writes).
async fn spawn_write_server(
    wb: Arc<RecordingWriteBack>,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<dyn SessionStore>,
) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(nostos_application::SessionManager::new(
        Arc::clone(&store),
        Tier::Enterprise,
    ));
    let mut tables = HashSet::new();
    tables.insert("tasks".to_string());
    let state = SyncRouterState::new(manager, Arc::new(AllowAnonymous::new()))
        .with_buffer(64)
        .with_write_back(wb as Arc<dyn WriteBack>)
        .with_write_tables(tables);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle, store)
}

/// The column extractor the FanOutService uses — match-all (the test doesn't
/// filter; the written row should reach the writer unconditionally). Returns
/// `ColumnValue::Any` so the (empty) predicate matches everything.
#[allow(clippy::unnecessary_wraps)]
fn extract_match_all(_e: &ReplicationEvent, _col: &str) -> Option<ColumnValue> {
    Some(ColumnValue::Any)
}

/// A write-back replicator: yields the captured write events ONE each call.
/// Lets the test drive the recorded writes back through the FanOutService as if
/// they had come from Postgres WAL.
struct CapturedStream {
    events: Vec<ReplicationEvent>,
}

#[async_trait]
impl ReplicatorStream for CapturedStream {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if self.events.is_empty() {
            return None;
        }
        Some(self.events.remove(0))
    }
}

fn idle_config(ms: u64) -> SyncClientConfig {
    SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(ms)),
        ..SyncClientConfig::default()
    }
}

/// Read a single row's payload out of the client SQLite (test-only). Returns
/// the opaque bytes stored for `(table, pk)`, or `None` if absent.
fn row_payload(db_path: &str, table: &str, pk: &str) -> Option<Vec<u8>> {
    let storage = SqliteStorage::open(db_path).unwrap();
    let conn = storage.conn_for_test();
    let row: std::result::Result<Vec<u8>, _> = conn.query_row(
        "SELECT payload FROM cairn_data WHERE table_name = ?1 AND pk = ?2",
        rusqlite::params![table, pk],
        |r| r.get(0),
    );
    row.ok()
}

/// An upsert of a single task row.
fn task_upsert(pk: &str) -> PendingWrite {
    PendingWrite {
        table: "tasks".to_string(),
        op: WriteOp::Upsert,
        pk: pk.to_string(),
        payload_json: Some(format!(r#"{{"title":"task-{pk}","done":"false"}}"#)),
    }
}

// ===========================================================================
// Test 1: write-while-offline succeeds + the queue is durable + flushes on
// reconnect + the row round-trips into the client SQLite.
// ===========================================================================
#[tokio::test(flavor = "multi_thread")]
async fn offline_write_survives_and_flushes_on_reconnect() {
    let dir = tempfile_dir();
    let db_path = format!("{dir}/offline.sqlite");

    // ---- Server is DOWN. ----
    // Pick a port nothing is on by spawning + immediately dropping a listener.
    let down_addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let down_url = format!("ws://{down_addr}/sync");

    // A client over the durable file. write() enqueues regardless of the
    // network — it MUST succeed even though nothing is listening.
    let client = SyncClient::new(
        down_url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(50),
    );
    let pending_write = task_upsert("a");
    let write_id = client
        .write(pending_write.clone())
        .await
        .expect("write() enqueues even when offline");
    assert!(write_id > 0, "enqueue returns a monotonic id");

    // ---- DURABILITY: a FRESH SqliteStorage handle on the SAME file sees the
    // enqueued write. The outbox is in `cairn_outbox`, committed to disk — a
    // crash can't strand it. ----
    let fresh = SqliteStorage::open(&db_path).unwrap();
    let pending = fresh.pending().expect("pending reads the outbox");
    assert_eq!(
        pending.len(),
        1,
        "the enqueued write is durable in a fresh handle"
    );
    assert_eq!(pending[0].0, write_id, "id matches the enqueue return");
    assert_eq!(
        pending[0].1, pending_write,
        "the round-tripped PendingWrite matches"
    );

    // ---- Bring the server UP. ----
    let wb = Arc::new(RecordingWriteBack::new());
    let (addr, _server, store) = spawn_write_server(Arc::clone(&wb)).await;
    let url = format!("ws://{addr}/sync");

    // Re-point the client at the live server and let it run one session: it
    // subscribes, flushes the outbox, and applies the round-trip frame.
    let live = SyncClient::new(
        url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(500),
    );
    let task = tokio::spawn(async move { live.run_once().await });
    // Give the flush + ack + round-trip time to land.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The write-back adapter should have captured exactly the one write.
    let captured = wb.drain().await;
    assert_eq!(
        captured.len(),
        1,
        "the server applied exactly one write from the outbox"
    );

    // Drive the captured event back through the fan-out — the round-trip. This
    // is what Postgres logical replication would do after PgWriteBack wrote the
    // row. The client (subscribed to `tasks`) receives and applies it.
    let mut stream = CapturedStream { events: captured };
    let fanout = FanOutService::new(Arc::clone(&store));
    fanout.run(&mut stream, extract_match_all).await;

    let outcome = task.await.unwrap().expect("client session error");

    // ---- The outbox is empty: the write was acked (ok:true) and mark_done'd. ----
    let after = SqliteStorage::open(&db_path).unwrap();
    assert!(
        after.pending().unwrap().is_empty(),
        "outbox drained after a successful flush (frames_received={})",
        outcome.frames_received
    );

    // ---- The row round-tripped into the client's SQLite. ----
    let payload = row_payload(&db_path, "tasks", "a").expect("row round-tripped");
    let payload_str = std::str::from_utf8(&payload).unwrap();
    assert!(
        payload_str.contains("task-a"),
        "round-tripped payload carries the written row: {payload_str}"
    );
}

// ===========================================================================
// Test 2: kill-restart durability. Enqueue, DROP the client process entirely,
// recreate from the same file, connect → the flush still happens (the queue
// survived the drop).
// ===========================================================================
#[tokio::test(flavor = "multi_thread")]
async fn outbox_survives_client_process_drop() {
    let dir = tempfile_dir();
    let db_path = format!("{dir}/kill-restart.sqlite");

    // ---- Enqueue with the server DOWN (no client session can exist). ----
    let down_addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let down_url = format!("ws://{down_addr}/sync");
    {
        let client = SyncClient::new(
            down_url,
            SqliteStorage::open(&db_path).unwrap(),
            idle_config(50),
        );
        let id = client
            .write(task_upsert("b"))
            .await
            .expect("enqueue while offline");
        assert!(id > 0);
        // ---- DROP the client process entirely. ----
        // `client` and its storage handle go out of scope here.
    }

    // The queue survived the drop — a fresh handle reads one pending write.
    let mid = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(mid.pending().unwrap().len(), 1, "queue survived the drop");

    // ---- Bring the server up; recreate the client from the SAME file. ----
    let wb = Arc::new(RecordingWriteBack::new());
    let (addr, _server, store) = spawn_write_server(Arc::clone(&wb)).await;
    let url = format!("ws://{addr}/sync");

    let revived = SyncClient::new(
        url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(500),
    );
    let task = tokio::spawn(async move { revived.run_once().await });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The revived client flushed the surviving write.
    let captured = wb.drain().await;
    assert_eq!(
        captured.len(),
        1,
        "the revived client flushed the surviving write"
    );

    // Round-trip the captured event through the fan-out.
    let mut stream = CapturedStream { events: captured };
    let fanout = FanOutService::new(Arc::clone(&store));
    fanout.run(&mut stream, extract_match_all).await;

    let _outcome = task.await.unwrap().expect("revived client session");

    // Outbox drained + row round-tripped.
    let after = SqliteStorage::open(&db_path).unwrap();
    assert!(
        after.pending().unwrap().is_empty(),
        "outbox drained after the revived client's flush"
    );
    let payload = row_payload(&db_path, "tasks", "b").expect("row round-tripped after restart");
    let s = std::str::from_utf8(&payload).unwrap();
    assert!(
        s.contains("task-b"),
        "round-trip carried the written row: {s}"
    );
}
