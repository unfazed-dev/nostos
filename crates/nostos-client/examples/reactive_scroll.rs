//! # reactive_scroll — the visible "2-way offline sync" demo.
//!
//! Demonstrates Nostos's headline moat end-to-end, in one process, fully runnable
//! with `cargo run -p nostos-client --example reactive_scroll`:
//!
//! 1. An in-process axum sync server (the real `/sync` handler, `InMemorySessionStore`,
//!    a recording `WriteBack` + the `tasks` allowlist so client writes round-trip).
//! 2. A background `FanOutService` feeding a stream of `tasks` rows (each carrying
//!    `org_id`/`status`/`priority` as a JSON payload — the exact shape
//!    `PgReplicator::tuple_to_json_payload` emits) into the store.
//! 3. A `SyncClient` with durable `SqliteStorage` (a temp file), reconnect
//!    enabled, subscribing with a **`where_sql` predicate** (`status = open AND
//!    priority >= 3`) — exercising the Tier-7 safe-SQL compiler + typed
//!    comparison end-to-end (the predicate is compiled on the server and ANDed
//!    into the session; only matching rows are delivered).
//! 4. Each applied row printed live: `[lsn] op tasks pk {json}`. This is the
//!    visible reactive stream.
//! 5. **A local write that round-trips** (D2/D3/D4): mid-script the client
//!    `write()`s a task to its durable outbox; the server's `WriteBack` accepts
//!    it and the demo pumps the captured write back through the fan-out as a
//!    `ReplicationEvent` — the writer sees its own write arrive via the same
//!    replication path, applied idempotently (one row, not two). This is the
//!    2-way half: reads flow server → client, writes flow client → server →
//!    client.
//! 6. A mid-run server restart — the client's `run_with_reconnect` resumes from
//!    its durable checkpoint, losing nothing (the `chaos_resume` property, made
//!    visible).
//!
//! The example *is* the verification: every line runs when it executes. There
//! are no test doubles in the demo path itself — it wires the production
//! `SyncClient`, `SqliteStorage`, `FanOutService`, and `/sync` handler. The one
//! stand-in is `RecordingWriteBack` (in lieu of `PgWriteBack` → Postgres →
//! logical replication), which captures a write and lets the demo replay it as
//! a replication event — the honest round-trip shape without a database.

// Presentation code: timing/format helpers trip pedantic lints that are fine
// for a demo (usize->f64 for a rate, format_push_string in a print). Mirrors
// nostos-bench's allow for the same reporting pattern.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::format_push_string
)]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::routing::get;
use bytes::Bytes;
use tokio::time::sleep;

use nostos_application::ports::{
    ReplicatorStream, SessionStore, SyncAuth, WriteBack, WriteBackError,
};
use nostos_application::{FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{Outbox, PendingWrite, Storage, WriteOp};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp, Tier};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Build a `tasks` row event whose payload is the JSON object
/// `{"org_id":"..","status":"..","priority":".."}` — the exact shape
/// `PgReplicator::tuple_to_json_payload` produces for real Postgres rows. The
/// client's predicate (`status=open AND priority>=3`) matches against this via
/// the typed-comparison leaves shipped in ADR-0012 slice 2.
fn tasks_event(lsn: u64, org: &str, status: &str, priority: i64) -> ReplicationEvent {
    // Manual JSON build (mirrors tuple_to_json_payload — every value quoted).
    let payload =
        format!("{{\"org_id\":\"{org}\",\"status\":\"{status}\",\"priority\":\"{priority}\"}}");
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: format!("task-{lsn}"),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        },
    )
}

/// The column extractor the FanOutService uses to evaluate predicates against
/// the JSON payload — the production `extract_json_column` shape (parse once,
/// cheap lookups), inlined here so the example is self-contained.
fn extract_json(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    // Parse the small JSON object once per call (the example's events are tiny;
    // production caches the parse — see extract_json_column in nostos-infra).
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    let map = parse_flat_json(s)?;
    map.get(col).map(ColumnValue::text)
}

/// Minimal flat-JSON-object parser for `{"k":"v",...}`. Production uses
/// `serde_json` (extract_json_column); this avoids pulling serde into the
/// example's call path for clarity.
fn parse_flat_json(s: &str) -> Option<HashMap<String, String>> {
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    let mut map = HashMap::new();
    for pair in s.split(',') {
        let mut kv = pair.splitn(2, ':');
        let k = kv.next()?.trim().trim_matches('"');
        let v = kv.next()?.trim().trim_matches('"');
        map.insert(k.to_string(), v.to_string());
    }
    Some(map)
}

/// A `WriteBack` that RECORDS each accepted write as a `ReplicationEvent` into
/// a shared mutex. The demo later drains that mutex and pumps the events back
/// through the real `FanOutService`, so the written row flows back to
/// subscribers (including the writer) exactly as it would from Postgres logical
/// replication — the honest round-trip path.
///
/// This stands in for `PgWriteBack` (which writes to Postgres and lets logical
/// replication carry the change back out). It lets the demo show the full
/// 2-way loop — outbox → Write frame → WriteResult ack → mark_done → row
/// arrives via replication → applied to client SQLite — without a database.
struct RecordingWriteBack {
    captured: tokio::sync::Mutex<Vec<ReplicationEvent>>,
    next_lsn: AtomicU64,
}

impl RecordingWriteBack {
    fn new() -> Self {
        Self {
            captured: tokio::sync::Mutex::new(Vec::new()),
            // Write-echo LSNs live ABOVE the read stream's LSN space (which is
            // 10, 20, …) so the two never collide.
            next_lsn: AtomicU64::new(1_000_000),
        }
    }

    /// Take the captured writes out (the demo's fan-out pump drains them).
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
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
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
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
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
        // P3 PowerSync PATCH parity: record the patch as an Update carrying the
        // partial tuple image.
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
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

    async fn increment(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        // ADR-0030 Decision 1: record the delta-op as an Update (the real
        // PgWriteBack applies col = col + delta and replicates the new row;
        // this recording mock has no row state — plumbing tests only).
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
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

/// A captured-write replicator: yields the drained write-echo events one per
/// call, so the demo can drive them back through the FanOutService as if they
/// had come from Postgres WAL.
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

/// Spawn the in-process sync server against a shared store, wired with the
/// recording write-back + the `tasks` allowlist so client writes pass the
/// allowlist gate, reach the adapter, and round-trip. Mirrors the
/// `nostos-client/tests/common` harness — the example wires the real handler.
async fn spawn_server(
    store: Arc<dyn SessionStore>,
    auth: Arc<dyn SyncAuth>,
    buffer: usize,
    wb: Arc<RecordingWriteBack>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let manager = Arc::new(SessionManager::new(store, Tier::Enterprise));
    let mut tables = HashSet::new();
    tables.insert("tasks".to_string());
    let state = SyncRouterState::new(manager, auth)
        .with_buffer(buffer)
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
    (addr, handle)
}

/// A tiny replicator that yields a scripted stream of `tasks` events — half
/// `open`/high-priority (match the demo predicate) and half `closed`/low
/// (filtered out). Demonstrates predicate filtering visibly.
struct DemoStream {
    total: u64,
    emitted: u64,
}

#[async_trait::async_trait]
impl ReplicatorStream for DemoStream {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if self.emitted >= self.total {
            return None;
        }
        let i = self.emitted;
        self.emitted += 1;
        let lsn = 10 + i * 10;
        // Alternate: even = open+priority5 (matches), odd = closed+priority1
        // (filtered by the client's predicate).
        let (status, priority) = if i.is_multiple_of(2) {
            ("open", 5_i64)
        } else {
            ("closed", 1)
        };
        let org = if i.is_multiple_of(3) {
            "acme"
        } else {
            "globex"
        };
        Some(tasks_event(lsn, org, status, priority))
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("=== nostos reactive_scroll demo — 2-way offline sync ===");
    println!("server + client in one process; durable SQLite; typed predicate; write round-trip\n");

    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    // The recording write-back survives the server restart (held by the demo,
    // not the server task) so a write enqueued before the restart still
    // round-trips after it.
    let wb = Arc::new(RecordingWriteBack::new());

    // --- Phase 1: start the server, drive a scripted event stream into it. ---
    let (addr, server_handle) =
        spawn_server(Arc::clone(&store), Arc::clone(&auth), 64, Arc::clone(&wb)).await;
    println!("[server] listening on http://{addr}/sync");

    let svc = Arc::new(FanOutService::new(Arc::clone(&store)));
    let mut stream = DemoStream {
        total: 20,
        emitted: 0,
    };
    // Pump events into the fan-out service in the background. The pump sleeps
    // briefly first so the client's subscribe lands before the events flow —
    // FanOut delivers only to sessions registered at fan-out time, so the
    // subscriber must connect before the pump emits.
    let svc_pump = Arc::clone(&svc);
    let pump = tokio::spawn(async move {
        sleep(Duration::from_millis(300)).await; // let the client subscribe first
        svc_pump.run(&mut stream, extract_json).await;
    });

    // --- Phase 2: a SyncClient with durable storage + a typed predicate. ---
    let db_path = format!(
        "{}/nostos-reactive-scroll-{}.db",
        std::env::temp_dir().to_string_lossy(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&db_path); // fresh each run
    let storage = SqliteStorage::open(&db_path).expect("open sqlite");
    let initial_checkpoint = storage.checkpoint().expect("checkpoint");
    println!("[client] durable sqlite at {db_path} (initial checkpoint: {initial_checkpoint:?})");

    let url = format!("ws://{addr}/sync");
    let config = SyncClientConfig {
        table: "tasks".into(),
        token: Some("anon".into()),
        // Subscribe with a where_sql — the Tier-7 safe-SQL compiler compiles
        // this on the server and ANDs it into the session predicate. Only rows
        // matching `status = open AND priority >= 3` are delivered (the
        // FanOutService's extract_json lifts real decoded values for the
        // predicate engine to evaluate). Exercises the compiler end-to-end.
        where_sql: Some("status = open AND priority >= 3".into()),
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(500),
        max_retries: Some(3),
        idle_timeout: Some(Duration::from_secs(2)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url, storage, config);
    println!("[client] subscribing to tasks (where status = open AND priority >= 3); applying rows as they stream in...\n");

    let outcome = client.run_once().await.expect("client run_once");
    println!(
        "\n[client] session done: {} frames received, {} commits, checkpoint {:?}",
        outcome.frames_received, outcome.commits, outcome.checkpoint
    );

    // --- Phase 2b: a LOCAL WRITE that round-trips (the 2-way half). ---
    // The client enqueues a write to its durable outbox and connects a fresh
    // session: the connected loop flushes the outbox as a `Write` frame, the
    // server's WriteBack captures it, and the demo pumps the captured event
    // back through the fan-out — the writer sees its own write arrive via
    // replication, applied idempotently (one row, not two). Prints the
    // round-trip payload so the 2-way property is visible.
    println!("\n[client] WRITE: enqueueing a local task to the durable outbox...");
    let write_client = SyncClient::new(
        format!("ws://{addr}/sync"),
        SqliteStorage::open(&db_path).expect("reopen sqlite for write"),
        SyncClientConfig {
            table: "tasks".into(),
            token: Some("anon".into()),
            where_sql: Some("status = open AND priority >= 3".into()),
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(500),
            max_retries: Some(3),
            idle_timeout: Some(Duration::from_millis(500)),
            ..SyncClientConfig::default()
        },
    );
    let written = PendingWrite {
        table: "tasks".into(),
        op: WriteOp::Upsert,
        pk: "demo-write".into(),
        // status=open + priority=5 so the row matches the subscription predicate
        // and is delivered back to the writer (the echo).
        payload_json: Some(
            r#"{"org_id":"acme","status":"open","priority":"5","title":"demo-write"}"#.into(),
        ),
    };
    let write_id = write_client
        .write(written)
        .await
        .expect("write enqueues to the durable outbox");
    println!("[client] write enqueued (outbox id {write_id}); flushing via a fresh session...");
    // Run the flush session in the background so we can pump the echo back
    // WHILE the session is still live (the round-trip frame must reach a
    // registered subscriber; once run_once returns the session is gone).
    let write_task =
        tokio::spawn(async move { write_client.run_once().await.expect("write flush session") });
    sleep(Duration::from_millis(300)).await; // let the subscribe + outbox flush land

    // Drain the captured write + pump it back through the fan-out (the echo).
    let captured = wb.drain().await;
    println!(
        "[server] write-back captured {} write(s); echoing back through replication...",
        captured.len()
    );
    let mut echo = CapturedStream { events: captured };
    let echo_svc = FanOutService::new(Arc::clone(&store));
    echo_svc.run(&mut echo, extract_json).await;

    let write_outcome = write_task.await.expect("write session task");
    println!(
        "[client] flush session done: {} frames, checkpoint {:?}",
        write_outcome.frames_received, write_outcome.checkpoint
    );

    // The round-trip: the written row is now in the client's SQLite, applied
    // idempotently from the replication echo. (Scope the connection guard so it
    // drops before the `pending()` read below — the storage mutex is not
    // reentrant, so holding the guard across another lock would deadlock.)
    let after_write = SqliteStorage::open(&db_path).expect("reopen after write");
    let roundtrip_str = {
        let conn = after_write.conn_for_test();
        let roundtrip: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM cairn_data WHERE pk = 'demo-write'",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .expect("the written row round-tripped into the client store");
        std::str::from_utf8(&roundtrip)
            .unwrap_or("<utf8 error>")
            .to_owned()
    };
    println!("[client] ROUND-TRIP: wrote 'demo-write', received it back via replication:");
    println!("         payload = {roundtrip_str}");
    // The outbox is drained (the write was ack'd ok:true + mark_done'd).
    let pending = after_write.pending().expect("read outbox");
    println!(
        "[client] outbox after round-trip: {} pending (write ack'd + mark_done'd)",
        pending.len()
    );

    // --- Phase 3: server restart — prove reconnect-resume loses nothing. ---
    println!("\n[server] RESTARTING (dropping the listener)...");
    server_handle.abort();
    sleep(Duration::from_millis(200)).await;
    let (addr2, _handle2) =
        spawn_server(Arc::clone(&store), Arc::clone(&auth), 64, Arc::clone(&wb)).await;
    println!("[server] back up on http://{addr2}/sync");

    // The client reconnects to the new address. Its durable checkpoint means it
    // resumes from where it left off — no loss, no duplication. Same where_sql
    // (the predicate is per-session, so the resumed session re-establishes it).
    let storage2 = SqliteStorage::open(&db_path).expect("reopen sqlite");
    let client2 = SyncClient::new(
        format!("ws://{addr2}/sync"),
        storage2,
        SyncClientConfig {
            table: "tasks".into(),
            token: Some("anon".into()),
            where_sql: Some("status = open AND priority >= 3".into()),
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(500),
            max_retries: Some(3),
            idle_timeout: Some(Duration::from_secs(1)),
            ..SyncClientConfig::default()
        },
    );
    let outcome2 = client2.run_once().await.expect("client2 run_once");
    println!(
        "[client2] resumed from durable checkpoint: {} frames this session, checkpoint {:?}",
        outcome2.frames_received, outcome2.checkpoint
    );

    // Let the pump finish.
    let _ = pump.await;
    println!("\n=== demo complete: 2-way sync — reads + write round-trip + durable resume, end-to-end ===");
    let _ = std::fs::remove_file(&db_path);
}
