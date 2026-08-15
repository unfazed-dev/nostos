//! `disconnect()`/`resume()` e2e (ADR-0037 task 5.1) — the push-notification
//! wake criterion:
//!
//! > *A backgrounded app gets poked, calls `resume()`, and the delta past the
//! > durable checkpoint applies — no data loss, no duplication, and the local
//! > store is never wiped.*
//!
//! Mirrors `chaos_resume.rs` (same `MonotonicRepl` + in-process-server shape):
//! ONE `SyncClient` on ONE durable store runs `run_with_reconnect`, applies
//! wave 1, is `disconnect()`-ed (the run task must exit on its own — the gate,
//! not an abort), sits out wave 2's fan-out entirely disconnected, then
//! `resume()`s and applies wave 2's delta from the checkpoint. The final store
//! contains exactly the union of both waves.

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

/// A replicator whose LSNs increase monotonically across the WHOLE test and
/// whose emission limit is a live atomic — `chaos_resume.rs`'s `MonotonicRepl`,
/// copied verbatim so both files keep the identical no-loss/no-dup probe
/// (unique pk per event ⇒ final row count is the exact probe).
struct MonotonicRepl {
    emitted: Arc<AtomicU64>,
    next_lsn: Arc<AtomicU64>,
    /// How many events `next_event` should emit before returning `None`.
    limit: Arc<AtomicU64>,
    table: String,
}

impl MonotonicRepl {
    /// Build a replicator sharing atomic counters across "waves" — wave 2's
    /// LSNs continue past wave 1's, so the resume seed does not drop them.
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

/// Fan out events from `repl` through the real FanOutService against the
/// shared store. Returns when the replicator's `next_event` returns `None`.
async fn fanout(repl: &mut MonotonicRepl, store: Arc<dyn SessionStore>) {
    let fanout = FanOutService::new(store);
    let extract =
        |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> { Some(ColumnValue::Any) };
    fanout.run(repl, extract).await;
}

/// Poll the client's durable store (through the client, not a second SQLite
/// connection) until `want` rows are applied, or panic with the deadline.
async fn wait_for_rows(client: &SyncClient<SqliteStorage>, want: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n = client
            .with_storage(SqliteStorage::row_count_for_test)
            .await
            .expect("row count via with_storage");
        if n >= want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {want} rows; store has {n}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_then_resume_applies_delta_from_checkpoint_without_loss() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let (addr, _server) =
        spawn_server_with_existing_store(Arc::clone(&store), Arc::new(AllowAnonymous::new()), 1024)
            .await;
    let url = format!("ws://{addr}/sync");
    let dir = tempfile_dir();
    let db_path = format!("{dir}/disconnect_resume.sqlite");

    // NO idle_timeout: the loop must stay live until disconnect() ends it —
    // that self-termination (not an abort, not idle) is what this test proves.
    let config = SyncClientConfig {
        idle_timeout: None,
        ..SyncClientConfig::default()
    };
    let client = Arc::new(SyncClient::new(
        url,
        SqliteStorage::open(&db_path).unwrap(),
        config,
    ));

    // ---- Session 1: subscribe first, then wave 1 (the bench's pattern). ----
    let run1 = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.run_with_reconnect().await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (mut repl, emitted, next_lsn, limit) = MonotonicRepl::shared("tasks");
    limit.store(100, Ordering::Relaxed);
    fanout(&mut repl, Arc::clone(&store)).await;
    wait_for_rows(&client, 100).await;
    let checkpoint_after_wave1 = client.checkpoint().await.unwrap();
    assert!(
        checkpoint_after_wave1 > Lsn::ZERO,
        "wave 1 advanced the checkpoint"
    );

    // ---- disconnect(): the run task must wind down ON ITS OWN (the gate, not
    // an abort) — and the durable store must survive untouched. ----
    client.disconnect();
    let out1 = tokio::time::timeout(Duration::from_secs(5), run1)
        .await
        .expect("run_with_reconnect exited within 5s of disconnect()")
        .expect("run task join")
        .expect("run_with_reconnect returned Ok");
    assert!(
        out1.frames_received >= 100,
        "wave 1 received {}",
        out1.frames_received
    );
    assert_eq!(out1.checkpoint, checkpoint_after_wave1);

    // Non-destructive: rows + checkpoint intact (contrast clear_local_state).
    let rows_after_disconnect = client
        .with_storage(SqliteStorage::row_count_for_test)
        .await
        .unwrap();
    assert_eq!(
        rows_after_disconnect, 100,
        "disconnect() must NOT wipe rows"
    );
    assert_eq!(
        client.checkpoint().await.unwrap(),
        checkpoint_after_wave1,
        "disconnect() must NOT reset the checkpoint"
    );

    // ---- Disconnected gap: while the client sleeps, bump the replicator's
    // limit so wave 2's LSNs are strictly past the checkpoint. No fan-out yet
    // (events before a subscriber registers are dropped by the per-session
    // sink — subscribe-first is load-bearing, as in chaos_resume). ----

    // ---- Session 2: resume() reopens the loop; the Subscribe carries
    // resume_lsn = the durable checkpoint, so only wave 2's delta flows. ----
    client.resume();
    let run2 = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.run_with_reconnect().await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut repl2 = MonotonicRepl {
        emitted: Arc::clone(&emitted),
        next_lsn: Arc::clone(&next_lsn),
        limit: Arc::clone(&limit),
        table: "tasks".into(),
    };
    limit.store(200, Ordering::Relaxed); // events 100..199, LSNs past wave 1
    fanout(&mut repl2, Arc::clone(&store)).await;
    wait_for_rows(&client, 200).await;

    // End session 2 the same way, then assert the full no-loss/no-dup probe.
    client.disconnect();
    let out2 = tokio::time::timeout(Duration::from_secs(5), run2)
        .await
        .expect("resumed run loop exited within 5s of disconnect()")
        .expect("run task join")
        .expect("run_with_reconnect returned Ok");

    let final_storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        final_storage.row_count_for_test(),
        200,
        "expected exactly 200 rows after resume (no loss, no dup)"
    );
    assert_eq!(
        final_storage.checkpoint().unwrap(),
        out2.checkpoint,
        "durable checkpoint matches the resumed session's outcome"
    );
    assert!(
        final_storage.checkpoint().unwrap() > checkpoint_after_wave1,
        "checkpoint advanced past wave 1 after resume"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_once_while_disconnected_is_a_clean_noop() {
    // The gate holds across sessions: after disconnect() and BEFORE resume(),
    // a spawned run loop must return immediately (a clean empty session at the
    // current checkpoint) instead of opening a socket — a backgrounded app's
    // stray spawn must not silently resync.
    let dir = tempfile_dir();
    let db_path = format!("{dir}/noop.sqlite");
    let client = SyncClient::new(
        "ws://127.0.0.1:1/sync".to_owned(), // nothing listens — connecting would error
        SqliteStorage::open(&db_path).unwrap(),
        SyncClientConfig {
            max_retries: Some(1), // the resumed attempt returns its error, doesn't loop
            ..SyncClientConfig::default()
        },
    );
    client.disconnect();
    let outcome = tokio::time::timeout(Duration::from_secs(1), client.run_with_reconnect())
        .await
        .expect("returns immediately while disconnected")
        .expect("clean Ok, not a connect error");
    assert_eq!(outcome.frames_received, 0);
    assert_eq!(outcome.commits, 0);
    assert_eq!(outcome.checkpoint, Lsn::ZERO);

    // resume() reopens: the same client may run again (this attempt fails to
    // connect — nothing listens — proving the gate, not the URL, was the
    // block). One retry then the connect error surfaces.
    client.resume();
    let resumed = tokio::time::timeout(Duration::from_secs(5), client.run_with_reconnect()).await;
    let resumed = resumed.expect("resume() let the loop run again (no timeout)");
    assert!(
        resumed.is_err(),
        "the post-resume attempt actually dialed the dead URL (gate was the only block)"
    );
}
