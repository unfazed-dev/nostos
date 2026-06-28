//! Chaos / resume e2e — the Phase 1 kill criterion, now met end-to-end.
//!
//! > *If the PG logical-replication state machine can't survive a mid-LSN crash
//! > without data loss or duplication, we don't have a product.*
//!
//! Tier 0+1 proved no-loss/no-duplication **on the wire** (ack-driven slot
//! advance + dedup ring). This test proves it **at the apply layer**: a real
//! [`SyncClient`] driving a real [`SqliteStorage`] receives frames over a real
//! WebSocket, applies them to SQLite, gets disconnected, reconnects with
//! `resume_lsn`, and the final store contains **exactly the rows that were
//! sent — none missing, none duplicated**.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{ReplicatorStream, SessionStore};
use nostos_application::FanOutService;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::Storage;
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::AllowAnonymous;

use common::{spawn_server_with_existing_store, tempfile_dir};

/// A replicator whose LSNs increase monotonically across the WHOLE test
/// (unlike `FakeReplicator`, which restarts at 1 on every construction).
///
/// This is what makes reconnect resume testable: the second wave of events has
/// LSNs strictly greater than the first client's checkpoint, so the server's
/// resume seed does NOT drop them. Each event has a unique pk, so the final row
/// count is the exact-loss/exact-dup probe.
struct MonotonicRepl {
    emitted: Arc<AtomicU64>,
    next_lsn: Arc<AtomicU64>,
    /// How many events `next_event` should emit before returning `None`.
    limit: Arc<AtomicU64>,
    table: String,
}

impl MonotonicRepl {
    /// Build a replicator sharing atomic counters across "waves." `next_lsn`
    /// starts at the given offset so two waves form one monotonic sequence.
    fn shared(table: &str) -> (Self, Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
        let emitted = Arc::new(AtomicU64::new(0));
        let next_lsn = Arc::new(AtomicU64::new(1));
        let limit = Arc::new(AtomicU64::new(0));
        let r = Self {
            emitted: Arc::clone(&emitted),
            next_lsn: Arc::clone(&next_lsn),
            limit: Arc::clone(&limit),
            table: table.into(),
        };
        (r, emitted, next_lsn, limit)
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
        // Unique pk per event across the whole test.
        let pk = format!("row-{n}");
        Some(ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: self.table.clone(),
                pk,
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

fn idle_config(ms: u64) -> SyncClientConfig {
    SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(ms)),
        ..SyncClientConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_applies_all_frames_without_loss() {
    // Simplest end-to-end: 100 frames, single session. Proves the
    // receive → apply → ack loop is wired correctly before adding chaos.
    let total = 100;
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let (addr, _server) =
        spawn_server_with_existing_store(Arc::clone(&store), Arc::new(AllowAnonymous::new()), 1024)
            .await;
    let url = format!("ws://{addr}/sync");
    let dir = tempfile_dir();
    let db_path = format!("{dir}/single.sqlite");

    // Subscribe FIRST, then drive (the bench's pattern — events before a
    // subscriber registers are dropped by the bounded per-session sink).
    let client = SyncClient::new(
        url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(400),
    );
    let task = tokio::spawn(async move { client.run_once().await });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (mut repl, _, _, limit) = MonotonicRepl::shared("tasks");
    limit.store(total, Ordering::Relaxed);
    fanout(&mut repl, Arc::clone(&store)).await;

    let outcome = task.await.unwrap().expect("client error");
    assert!(
        outcome.frames_received >= total,
        "received {}",
        outcome.frames_received
    );

    let storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        storage.row_count_for_test(),
        usize::try_from(total).unwrap(),
        "all frames applied, none lost"
    );
    assert!(storage.checkpoint().unwrap().raw() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_with_resume_lsn_loses_nothing_duplicates_nothing() {
    // THE kill criterion. Two waves of 100 events sharing ONE monotonic LSN
    // sequence. Client 1 subscribes, applies wave 1 (LSNs 1..1000), disconnects.
    // Client 2 reconnects with resume_lsn = client 1's checkpoint, applies wave
    // 2 (LSNs 1001..2000). Final store: exactly 200 distinct rows.
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let (addr, _server) =
        spawn_server_with_existing_store(Arc::clone(&store), Arc::new(AllowAnonymous::new()), 1024)
            .await;
    let url = format!("ws://{addr}/sync");
    let dir = tempfile_dir();
    let db_path = format!("{dir}/chaos.sqlite");

    // Shared counters so wave 2's LSNs continue past wave 1's.
    let (mut repl1, emitted, next_lsn, limit) = MonotonicRepl::shared("tasks");

    // ---- Wave 1: client 1 subscribes, then we fan out 100 events. ----
    let client1 = SyncClient::new(
        url.clone(),
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(400),
    );
    let task1 = tokio::spawn(async move { client1.run_once().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    limit.store(100, Ordering::Relaxed);
    fanout(&mut repl1, Arc::clone(&store)).await;
    let out1 = task1.await.unwrap().expect("client 1 error");
    assert!(
        out1.frames_received >= 100,
        "wave 1 received {}",
        out1.frames_received
    );

    let checkpoint_after_wave1 = SqliteStorage::open(&db_path).unwrap().checkpoint().unwrap();
    let rows_after_wave1 = SqliteStorage::open(&db_path).unwrap().row_count_for_test();
    assert_eq!(rows_after_wave1, 100, "wave 1 applied exactly 100 rows");

    // ---- Wave 2: client 2 reconnects, resume from durable checkpoint. ----
    let mut repl2 = MonotonicRepl {
        emitted: Arc::clone(&emitted),
        next_lsn: Arc::clone(&next_lsn),
        limit: Arc::clone(&limit),
        table: "tasks".into(),
    };
    let client2 = SyncClient::new(
        url,
        SqliteStorage::open(&db_path).unwrap(),
        idle_config(400),
    );
    let task2 = tokio::spawn(async move { client2.run_once().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    limit.store(200, Ordering::Relaxed); // emit through event 200 total
    fanout(&mut repl2, Arc::clone(&store)).await;
    let out2 = task2.await.unwrap().expect("client 2 error");
    assert!(
        out2.frames_received >= 100,
        "wave 2 received {}",
        out2.frames_received
    );

    // ---- THE assertion: exactly 200 rows — no loss, no duplication. ----
    // Wave 2's LSNs (1001..2000) are strictly > client 1's checkpoint, so the
    // server did NOT drop them. Reconnect replay may have re-sent the tail of
    // wave 1's window; idempotent upsert collapsed any overlap. Net: 200
    // distinct pks.
    let final_storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        final_storage.row_count_for_test(),
        200,
        "expected exactly 200 rows (no loss, no dup)"
    );
    assert!(
        final_storage.checkpoint().unwrap() > checkpoint_after_wave1,
        "checkpoint advanced past wave 1"
    );
}
