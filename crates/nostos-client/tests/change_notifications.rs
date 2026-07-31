//! `SyncClient::subscribe_changes` — proves the per-commit broadcast added for
//! `nostos_flutter`'s `watch()` actually fires, with the right shape, over a
//! real WebSocket session (not just a unit test of the struct).

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::ReplicatorStream;
use nostos_application::FanOutService;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp};

use common::{spawn_server_with_store, tempfile_dir};

/// Emits exactly two `tasks` inserts, then ends the stream.
struct TwoEvents {
    emitted: u8,
}

#[async_trait]
impl ReplicatorStream for TwoEvents {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if self.emitted >= 2 {
            return None;
        }
        self.emitted += 1;
        let lsn = u64::from(self.emitted) * 10;
        Some(ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: format!("row-{}", self.emitted),
                payload: Bytes::from_static(b"payload"),
            },
        ))
    }
}

#[tokio::test]
async fn subscribe_changes_broadcasts_one_outcome_per_commit() {
    let (addr, _handle, store) = spawn_server_with_store(64).await;

    // `FanOutService` only delivers to sessions registered at fan-out time, so
    // the pump MUST run only after the client has subscribed. The fixed
    // `sleep(200ms)` this replaced raced the client's connect+subscribe and
    // silently dropped events under parallel load (`frames_received < 2`).
    // Latching on `store.len() > 0` synchronizes on the real readiness event —
    // the server registers the session in this shared store when it receives
    // the client's Subscribe frame — so fan-out never fires before a sink
    // exists. (The 5ms is poll cadence, not a correctness timeout: the loop
    // waits for the subscription unconditionally.)
    let pump_store = Arc::clone(&store);
    let svc = Arc::new(FanOutService::new(Arc::clone(&store)));
    let pump_svc = Arc::clone(&svc);
    let pump = tokio::spawn(async move {
        while pump_store.len().await == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut stream = TwoEvents { emitted: 0 };
        let extract = |_e: &ReplicationEvent, _c: &str| Some(ColumnValue::Any);
        pump_svc.run(&mut stream, extract).await;
    });

    let dir = tempfile_dir();
    let db_path = format!("{dir}/change-notifications.sqlite");
    let storage = SqliteStorage::open(&db_path).unwrap();
    let client = SyncClient::new(
        format!("ws://{addr}/sync"),
        storage,
        SyncClientConfig {
            table: "tasks".into(),
            // idle_timeout is the "caught up → return" signal; the only path to
            // a commit is the final flush after it fires.
            idle_timeout: Some(Duration::from_millis(600)),
            // Disable mid-stream quiesce so the two standalone frames can never
            // be split into separate commits by the 50ms quiesce timer firing
            // between them under load (which broadcast rows_applied == 1 twice
            // and made `recv()` observe the first). With quiesce off, both
            // buffer until the single final-flush commit — one broadcast,
            // rows_applied == 2 — matching the doc comment on
            // `SyncClient::subscribe_changes`.
            flush_quiesce: None,
            ..SyncClientConfig::default()
        },
    );

    // Subscribed BEFORE run_once starts, per the method's documented ordering
    // requirement — a receiver created after the first send would miss it.
    let mut changes = client.subscribe_changes();

    let outcome = client.run_once().await.expect("run_once");
    assert_eq!(outcome.frames_received, 2, "both events were received");

    // Two standalone frames, no quiesce, soft cap (256) never reached → both
    // stay buffered until the idle-timeout-triggered final flush, landing in
    // ONE commit — one broadcast, rows_applied == 2, checkpoint matches.
    let notified = changes.recv().await.expect("commit notification");
    assert_eq!(notified.rows_applied, 2);
    assert_eq!(notified.checkpoint, outcome.checkpoint);

    pump.await.unwrap();
}
