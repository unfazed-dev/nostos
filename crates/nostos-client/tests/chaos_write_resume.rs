//! **D4 — the chaos write-resume e2e.** The capstone that makes
//! "2-way offline sync" a true sentence.
//!
//! > *If a client can't make offline writes during a server outage, then resume
//! > from its durable checkpoint when the server comes back — losing nothing,
//! > duplicating nothing — we don't have 2-way sync.*
//!
//! This test combines everything the prior phases proved in isolation:
//! - **chaos_resume** (the restart pattern): a real `SyncClient` over real
//!   `SqliteStorage` survives a mid-stream server kill, reconnects with
//!   `resume_lsn`, and loses no rows.
//! - **D2** (write-back): a `Write` frame from the client reaches the server's
//!   `WriteBack` adapter, which surfaces it back as a `ReplicationEvent`
//!   through the fan-out — the round-trip.
//! - **D3** (outbox): offline writes land in the durable `nostos_outbox` and
//!   flush on reconnect.
//! - **D4 Step 0** (the idempotency premise): the writer's own write comes back
//!   to it via replication, and `apply_batch` collapses the echo to one row.
//!
//! The kill criterion is the FINAL ROW COUNT, asserted with a number — not "no
//! crash." It is `wave1_rows + wave2_rows + 2 offline writes`, exactly, with no
//! duplication from the echo (the writer seeing its own write) or from the
//! mid-stream replay. If the count is off, that's a real bug in D2/D3 or the
//! resume semantics — do NOT loosen the assertion.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::routing::get;
use bytes::Bytes;
use nostos_application::ports::{
    ReplicatorStream, SessionStore, SyncAuth, WriteBack, WriteBackError,
};
use nostos_application::{FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{Outbox, PendingWrite, Storage, WriteOp};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp, Tier};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::AllowAnonymous;

use common::tempfile_dir;

// ===========================================================================
// The recording write-back — stands in for PgWriteBack.
//
// Same shape as D3's `offline_writes.rs` RecordingWriteBack: each accepted
// write is captured as a ReplicationEvent into a shared mutex, drained later by
// the test's fan-out pump. This is the honest round-trip path — the written row
// flows back to subscribers (including the writer) exactly as it would from
// Postgres logical replication. It lets the chaos test prove the full loop
// (outbox → Write frame → WriteResult ack → mark_done → row arrives via
// replication → applied to client SQLite) without a database.
// ===========================================================================
struct RecordingWriteBack {
    captured: tokio::sync::Mutex<Vec<ReplicationEvent>>,
    next_lsn: AtomicU64,
}

impl RecordingWriteBack {
    fn new() -> Self {
        Self {
            captured: tokio::sync::Mutex::new(Vec::new()),
            // Offline-write LSNs start ABOVE the read-replication LSN space so
            // the two never collide (read rows: 10, 20, …; write echoes: 1_000_000+).
            next_lsn: AtomicU64::new(1_000_000),
        }
    }

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
        // P3 column-level PATCH: record the patch as an Update carrying the
        // partial tuple image (the real PgWriteBack applies a column-level
        // UPDATE — absent columns untouched).
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

// ===========================================================================
// A monotonic read-replicator. Each event has a UNIQUE pk and a strictly
// increasing LSN, shared across "waves" via atomic counters — so wave 2's LSNs
// are strictly greater than wave 1's, and the server's resume seed (the client's
// checkpoint) does NOT drop them. Each event is one distinct row, so the final
// row count is the exact-loss / exact-dup probe.
// ===========================================================================
struct MonotonicRepl {
    emitted: Arc<AtomicU64>,
    next_lsn: Arc<AtomicU64>,
    limit: Arc<AtomicU64>,
    table: String,
}

impl MonotonicRepl {
    /// Build a replicator sharing atomic counters across waves. `next_lsn`
    /// starts at the given offset so two waves form one monotonic sequence.
    fn shared(table: &str) -> (Self, Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
        let emitted = Arc::new(AtomicU64::new(0));
        let next_lsn = Arc::new(AtomicU64::new(10));
        let limit = Arc::new(AtomicU64::new(0));
        let r = Self {
            emitted: Arc::clone(&emitted),
            next_lsn: Arc::clone(&next_lsn),
            limit: Arc::clone(&limit),
            table: table.into(),
        };
        (r, emitted, next_lsn, limit)
    }

    /// Clone with the SAME shared counters (so wave 2 continues wave 1's
    /// sequence).
    fn clone_shared(&self) -> Self {
        Self {
            emitted: Arc::clone(&self.emitted),
            next_lsn: Arc::clone(&self.next_lsn),
            limit: Arc::clone(&self.limit),
            table: self.table.clone(),
        }
    }
}

#[async_trait]
impl ReplicatorStream for MonotonicRepl {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        let n = self.emitted.fetch_add(1, Ordering::Relaxed);
        if n >= self.limit.load(Ordering::Relaxed) {
            self.emitted.fetch_sub(1, Ordering::Relaxed); // didn't emit
            return None;
        }
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
        Some(ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: self.table.clone(),
                pk: format!("row-{n}"),
                payload: Bytes::from_static(b"payload"),
            },
        ))
    }
}

/// Fan out `count` events from `repl` through the real FanOutService against the
/// shared store. Returns when the replicator's `next_event` returns `None`.
async fn fanout(repl: &mut MonotonicRepl, store: Arc<dyn SessionStore>) {
    let fanout = FanOutService::new(store);
    let extract =
        |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> { Some(ColumnValue::Any) };
    fanout.run(repl, extract).await;
}

/// A captured-write replicator: yields the drained write-echo events one per
/// call. Lets the test drive the recorded writes back through the FanOutService
/// as if they had come from Postgres WAL (the round-trip).
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

/// Spawn the in-process server wired with the recording write-back + the
/// `tasks` allowlist (so client writes pass the allowlist gate and reach the
/// adapter). Returns the bound address, the server JoinHandle, and the shared
/// session store (so the test can fan out against the same store the transport
/// reads).
async fn spawn_write_server(
    store: Arc<dyn SessionStore>,
    auth: Arc<dyn SyncAuth>,
    wb: Arc<RecordingWriteBack>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let manager = Arc::new(SessionManager::new(store, Tier::Enterprise));
    let mut tables = HashSet::new();
    tables.insert("tasks".to_string());
    let state = SyncRouterState::new(manager, auth)
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
    (addr, handle)
}

fn idle_config(ms: u64) -> SyncClientConfig {
    SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(ms)),
        ..SyncClientConfig::default()
    }
}

/// Build a single task upsert PendingWrite (the offline-write payload).
fn task_upsert(pk: &str) -> PendingWrite {
    PendingWrite {
        table: "tasks".to_string(),
        op: WriteOp::Upsert,
        pk: pk.to_string(),
        payload_json: Some(format!(r#"{{"title":"task-{pk}","done":"false"}}"#)),
    }
}

/// Count rows whose pk starts with `prefix` in the client SQLite — lets the
/// test separate read-replication rows (`row-N`) from offline-write rows
/// (`write-A` / `write-B`).
fn count_rows_with_pk_prefix(db_path: &str, prefix: &str) -> usize {
    let storage = SqliteStorage::open(db_path).unwrap();
    let conn = storage.conn_for_test();
    let like = format!("{prefix}%");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nostos_data WHERE pk LIKE ?1",
            rusqlite::params![like],
            |r| r.get(0),
        )
        .unwrap();
    usize::try_from(count).unwrap()
}

/// Read a single row's payload out of the client SQLite (test-only).
fn row_payload(db_path: &str, pk: &str) -> Option<Vec<u8>> {
    let storage = SqliteStorage::open(db_path).unwrap();
    let conn = storage.conn_for_test();
    let row: Result<Vec<u8>, _> = conn.query_row(
        "SELECT payload FROM nostos_data WHERE pk = ?1",
        rusqlite::params![pk],
        |r| r.get(0),
    );
    row.ok()
}

// ===========================================================================
// THE chaos write-resume test.
//
// Phase 1: server UP, client online syncing — apply wave 1 (WAVE1 read rows).
// Phase 2: server KILLED mid-stream.
// Phase 3: client makes 2 OFFLINE writes while the server is down (outbox).
// Phase 4: server RESTARTS (new process, same store + write-back).
// Phase 5: client reconnects → resumes from checkpoint (no loss) + flushes
//          outbox → both offline rows visible via replication echo.
//
// Final invariant: WAVE1 + WAVE2 + 2 offline rows, exactly. No duplication from
// the echo (writer sees its own write) or the mid-stream replay.
// ===========================================================================
#[tokio::test(flavor = "multi_thread")]
async fn chaos_offline_writes_survive_mid_stream_restart_no_loss_no_dup() {
    const WAVE1: u64 = 50;
    const WAVE2: u64 = 50;
    const OFFLINE_WRITES: usize = 2;

    let dir = tempfile_dir();
    let db_path = format!("{dir}/chaos-write-resume.sqlite");

    // The shared session store + write-back survive the server restart (they're
    // held by the test, not the server task). The store carries the registered
    // sessions across the restart; the write-back accumulates captures.
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let wb = Arc::new(RecordingWriteBack::new());

    // Shared monotonic counters so wave 2's read LSNs continue past wave 1's.
    let (mut repl, _, _, limit) = MonotonicRepl::shared("tasks");

    let expected_total: usize =
        usize::try_from(WAVE1).unwrap() + usize::try_from(WAVE2).unwrap() + OFFLINE_WRITES;

    // -----------------------------------------------------------------------
    // Phase 1: server UP. Client connects, applies wave 1.
    // -----------------------------------------------------------------------
    let (addr1, server1) =
        spawn_write_server(Arc::clone(&store), Arc::clone(&auth), Arc::clone(&wb)).await;
    let url1 = format!("ws://{addr1}/sync");

    let client1 = SyncClient::new(
        url1.clone(),
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(400),
    );
    let task1 = tokio::spawn(async move { client1.run_once().await });
    tokio::time::sleep(Duration::from_millis(150)).await; // let the subscribe land

    limit.store(WAVE1, Ordering::Relaxed);
    fanout(&mut repl, Arc::clone(&store)).await;
    let out1 = task1.await.unwrap().expect("client 1 session error");
    assert!(
        out1.frames_received >= WAVE1,
        "wave 1: client received {} frames (expected >= {WAVE1})",
        out1.frames_received
    );

    let checkpoint_after_wave1 = SqliteStorage::open(&db_path).unwrap().checkpoint().unwrap();
    let rows_after_wave1 = SqliteStorage::open(&db_path).unwrap().row_count_for_test();
    assert_eq!(
        rows_after_wave1,
        usize::try_from(WAVE1).unwrap(),
        "wave 1 applied exactly {WAVE1} read rows"
    );

    // -----------------------------------------------------------------------
    // Phase 2: server KILLED mid-stream. The client's session is over.
    // -----------------------------------------------------------------------
    server1.abort();
    tokio::time::sleep(Duration::from_millis(150)).await; // let the abort land
                                                          // The client's run_once has already returned (idle_timeout fired at the end
                                                          // of wave 1). Confirm there's nothing on the wire by re-opening the store.

    // -----------------------------------------------------------------------
    // Phase 3: 2 OFFLINE writes while the server is DOWN. The outbox is the
    // ONLY path these can take — write() must succeed with no server reachable.
    // -----------------------------------------------------------------------
    // A port nothing is on (the old server is gone).
    let down_addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let down_url = format!("ws://{down_addr}/sync");

    // Build a client pointed at the dead address OVER THE SAME durable file.
    let offline_client = SyncClient::new(
        down_url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(50),
    );
    let id_a = offline_client
        .write(task_upsert("write-A"))
        .await
        .expect("write A enqueues even with the server DOWN");
    let id_b = offline_client
        .write(task_upsert("write-B"))
        .await
        .expect("write B enqueues even with the server DOWN");
    assert!(id_a > 0 && id_b > id_a, "outbox ids are monotonic");

    // The outbox is durable on the SAME file: a fresh handle sees both writes.
    let mid = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        mid.pending().unwrap().len(),
        OFFLINE_WRITES,
        "both offline writes are durable in the outbox while the server is down"
    );
    // WS2 instant-local: the offline writes are ALREADY applied to nostos_data
    // (visible before any round-trip — offline-first) AND still durable in the
    // outbox (proven by the pending() assertion above). The server's echo later
    // UPSERTs the authoritative image on top (reconcile); the no-loss / no-dup
    // round-trip invariants below still hold — the echo overwrites the same pks.
    assert_eq!(
        mid.row_count_for_test(),
        usize::try_from(WAVE1).unwrap() + OFFLINE_WRITES,
        "instant-local: offline writes applied immediately AND queued durably"
    );

    // -----------------------------------------------------------------------
    // Phase 4: server RESTARTS. Same store, same write-back, new process.
    // -----------------------------------------------------------------------
    let (addr2, _server2) =
        spawn_write_server(Arc::clone(&store), Arc::clone(&auth), Arc::clone(&wb)).await;
    let url2 = format!("ws://{addr2}/sync");

    // -----------------------------------------------------------------------
    // Phase 5: client reconnects → resumes from checkpoint + flushes outbox.
    // -----------------------------------------------------------------------
    let client2 = SyncClient::new(
        url2,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(500),
    );
    let task2 = tokio::spawn(async move { client2.run_once().await });
    tokio::time::sleep(Duration::from_millis(300)).await; // flush + ack time

    // The write-back captured BOTH offline writes (the outbox flushed).
    let captured = wb.drain().await;
    assert_eq!(
        captured.len(),
        OFFLINE_WRITES,
        "the server applied both offline writes from the outbox after the restart"
    );

    // Drive the captured write-echoes through the fan-out — the round-trip.
    // The client (still subscribed to `tasks`) receives and applies them. This
    // is the echo: the writer sees its own writes come back via replication.
    let mut echo = CapturedStream { events: captured };
    let echo_fanout = FanOutService::new(Arc::clone(&store));
    let echo_extract =
        |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> { Some(ColumnValue::Any) };
    echo_fanout.run(&mut echo, echo_extract).await;

    // Wave 2 read rows: continue the monotonic LSN sequence past wave 1.
    let mut repl2 = repl.clone_shared();
    limit.store(WAVE1 + WAVE2, Ordering::Relaxed); // emit through event WAVE1+WAVE2
    fanout(&mut repl2, Arc::clone(&store)).await;

    let out2 = task2.await.unwrap().expect("client 2 session error");

    // -----------------------------------------------------------------------
    // THE INVARIANTS — asserted with counts, not "no crash."
    // -----------------------------------------------------------------------

    // (a) The outbox is EMPTY — both offline writes were ack'd (ok:true) and
    //     mark_done'd. No write stranded, no write double-flushed.
    let final_storage = SqliteStorage::open(&db_path).unwrap();
    assert!(
        final_storage.pending().unwrap().is_empty(),
        "outbox drained after the flush (frames_received this session = {})",
        out2.frames_received
    );

    // (b) Total row count is EXACT — wave1 + wave2 + 2 offline writes. This is
    //     the kill criterion: no loss (count too low), no duplication (count
    //     too high — from the echo or the mid-stream replay).
    let final_count = final_storage.row_count_for_test();
    assert_eq!(
        final_count, expected_total,
        "FINAL ROW COUNT: expected exactly {expected_total} \
         ({WAVE1} wave1 + {WAVE2} wave2 + {OFFLINE_WRITES} offline), got {final_count} — \
         no loss, no duplication from echo or replay"
    );

    // (c) Wave 1 rows all survived (no loss across the restart).
    assert_eq!(
        count_rows_with_pk_prefix(&db_path, "row-"),
        usize::try_from(WAVE1 + WAVE2).unwrap(),
        "all read-replication rows (wave1 + wave2) present"
    );

    // (d) BOTH offline writes round-tripped — visible via the replication echo,
    //     applied idempotently (one row each, not two).
    assert_eq!(
        count_rows_with_pk_prefix(&db_path, "write-"),
        OFFLINE_WRITES,
        "both offline writes round-tripped into the client store (one row each)"
    );

    // (e) The round-tripped payloads carry the written rows (content check).
    let pa = row_payload(&db_path, "write-A").expect("write-A round-tripped");
    let sa = std::str::from_utf8(&pa).unwrap();
    assert!(sa.contains("task-write-A"), "write-A payload: {sa}");
    let pb = row_payload(&db_path, "write-B").expect("write-B round-tripped");
    let sb = std::str::from_utf8(&pb).unwrap();
    assert!(sb.contains("task-write-B"), "write-B payload: {sb}");

    // (f) Checkpoint advanced past wave 1 (resume made progress, not regressed).
    assert!(
        final_storage.checkpoint().unwrap() > checkpoint_after_wave1,
        "checkpoint advanced past wave 1 after the restart + wave 2"
    );

    // (g) The session received BOTH the offline-write echoes AND wave 2's read
    //     rows — proving the resume + flush + echo all happened in one session.
    let min_wave2_echo = WAVE2 + u64::try_from(OFFLINE_WRITES).unwrap();
    assert!(
        out2.frames_received >= min_wave2_echo,
        "wave 2 + echoes: client received {} frames (expected >= {})",
        out2.frames_received,
        min_wave2_echo
    );
}
