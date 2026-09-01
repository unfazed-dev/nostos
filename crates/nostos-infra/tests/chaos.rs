//! Tier 2 chaos / concurrency tests — the highest bug-discovery ROI.
//!
//! Runs on a **multi-thread** tokio runtime so the scheduler interleaves the
//! fan-out hot path with connect/disconnect churn — the conditions under which
//! a DashMap-indexed router with bounded drop-on-full sinks is most likely to
//! silently lose events, deadlock, or violate its conservation invariant.
//!
//! Invariants asserted:
//!
//! - **conservation**: under concurrent connect/disconnect/deliver, no panic
//!   and `delivered + dropped == matched` for every event (nothing vanishes);
//! - **selective delivery under load**: events to different tables reach only
//!   the sessions subscribed to that table (DashMap shard correctness);
//! - **slow-client isolation**: one stalled sink drops, but does NOT stall the
//!   others (the head-of-line-blocking guarantee that is the whole point of
//!   drop-on-full backpressure).
//!
//! Runs on every push. No PG, no WS.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{DeliveryDecision, EventSink, SessionStore};
use nostos_application::{FanOutOutcome, FanOutService, SessionManager};
use nostos_domain::{ColumnValue, Lsn, Predicate, ReplicationEvent, RowOp, SyncSession, Tier};
use nostos_infra::router::TokioEventSink;
use nostos_infra::store::InMemorySessionStore;

/// A sink that counts deliveries atomically (sharded per sink, so N of them
/// don't contend on one cache line). Never drops — drop accounting is read from
/// the `FanOutOutcome`, not the sink.
struct TallySink {
    delivered: AtomicU64,
}

impl TallySink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            delivered: 0.into(),
        })
    }
}

#[async_trait]
impl EventSink for TallySink {
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
        // Never drops — used for the conservation + selective tests.
        self.delivered.fetch_add(1, Ordering::Relaxed);
        let _ = event;
        DeliveryDecision::Delivered
    }
}

fn event_on(table: &str, lsn: u64) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: table.into(),
            pk: lsn.to_string(),
            payload: Bytes::from_static(b"x"),
        },
    )
}

fn pipeline() -> (Arc<FanOutService>, Arc<dyn SessionStore>) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let svc = Arc::new(FanOutService::new(store.clone()));
    (svc, store)
}

// ---------------------------------------------------------------------------
// Scenario 1: conservation under concurrent connect/disconnect/deliver.
//
// While events flow to `tasks`, sessions are connecting and disconnecting on
// the same table. After the dust settles: no panic, and for every event the
// router either delivered or dropped it — delivered + dropped == matched. The
// invariant must hold regardless of the interleaving.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conservation_under_churn() {
    let (svc, store) = pipeline();
    let mgr = Arc::new(SessionManager::new(store.clone(), Tier::Enterprise));

    // A pool of sinks that stay alive for the duration.
    let sinks: Vec<Arc<TallySink>> = (0..50).map(|_| TallySink::new()).collect();

    // Pre-register a stable set so fan_out always has candidates.
    for s in &sinks {
        store
            .add(
                SyncSession::new(Predicate::all("tasks")),
                Arc::clone(s) as Arc<dyn EventSink>,
            )
            .await;
    }

    // Churn task: connect + immediately disconnect transient sessions.
    let mgr_c = Arc::clone(&mgr);
    let churn = tokio::spawn(async move {
        for _ in 0..200 {
            let sink = Arc::new(TokioEventSink::channel(8).0);
            let session = SyncSession::new(Predicate::all("tasks"));
            if let Ok(id) = mgr_c.connect(session, sink as Arc<dyn EventSink>).await {
                // Immediately drop — exercises remove() racing fan_out.
                mgr_c.disconnect(id).await;
            }
        }
    });

    // Fan-out task: push events concurrently with the churn.
    let svc_c = Arc::clone(&svc);
    let deliver = tokio::spawn(async move {
        let mut total = FanOutOutcome::default();
        for i in 1..=500u64 {
            total = total.merged(
                svc_c
                    .fan_out(&event_on("tasks", i), |_, _| Some(ColumnValue::Any))
                    .await,
            );
        }
        total
    });

    // Deadlock guard. This test hung for 6 hours in CI (run 33490859076) when
    // the store held DashMap shard guards across `.await` — see
    // InMemorySessionStore::table_lists. A recurrence must fail in a minute
    // with a name on it, not burn the job cap and look like an infra flake.
    let (churn_res, deliver_res) = tokio::time::timeout(
        std::time::Duration::from_mins(1),
        async { (churn.await, deliver.await) },
    )
    .await
    .expect(
        "deadlock: churn + fan_out did not finish in 60s. Prime suspect is a \
         blocking guard (DashMap shard / std Mutex) held across an .await in \
         the session store — see InMemorySessionStore::table_lists.",
    );
    churn_res.unwrap();
    let outcome = deliver_res.unwrap();

    // Conservation: every matched event was either delivered or dropped.
    assert_eq!(
        outcome.delivered + outcome.dropped,
        outcome.matched,
        "conservation violated: delivered({}) + dropped({}) != matched({})",
        outcome.delivered,
        outcome.dropped,
        outcome.matched
    );
    // Sanity: we pushed 500 events against a stable set of 50 → matched ≥ 500.
    assert!(
        outcome.matched >= 500,
        "expected ≥500 matches, got {}",
        outcome.matched
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: selective delivery under concurrent multi-table load.
//
// Two tables, each with its own subscriber set. Events stream to BOTH tables
// concurrently. After completion, each sink must have received ONLY its own
// table's events — the DashMap table index must not cross-contaminate shards.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selective_delivery_under_multitable_load() {
    let (svc, store) = pipeline();

    let tasks_sink = TallySink::new();
    let users_sink = TallySink::new();
    store
        .add(
            SyncSession::new(Predicate::all("tasks")),
            Arc::clone(&tasks_sink) as Arc<dyn EventSink>,
        )
        .await;
    store
        .add(
            SyncSession::new(Predicate::all("users")),
            Arc::clone(&users_sink) as Arc<dyn EventSink>,
        )
        .await;

    let n = 400u64;
    let svc_t = Arc::clone(&svc);
    let svc_u = Arc::clone(&svc);
    let (t, u) = tokio::join!(
        async move {
            let mut o = FanOutOutcome::default();
            for i in 1..=n {
                o = o.merged(
                    svc_t
                        .fan_out(&event_on("tasks", i), |_, _| Some(ColumnValue::Any))
                        .await,
                );
            }
            o
        },
        async move {
            let mut o = FanOutOutcome::default();
            for i in 1..=n {
                o = o.merged(
                    svc_u
                        .fan_out(&event_on("users", i), |_, _| Some(ColumnValue::Any))
                        .await,
                );
            }
            o
        }
    );

    // Each sink got exactly its own table's count — no cross-contamination.
    assert_eq!(t.delivered, n, "tasks sink should get all {n} tasks events");
    assert_eq!(u.delivered, n, "users sink should get all {n} users events");
    assert_eq!(
        tasks_sink.delivered.load(Ordering::Relaxed),
        n,
        "tasks sink leaked nothing FROM users"
    );
    assert_eq!(
        users_sink.delivered.load(Ordering::Relaxed),
        n,
        "users sink leaked nothing FROM tasks"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: slow-client isolation (the head-of-line-blocking guarantee).
//
// One sink with a depth-1 channel that is NEVER drained (the stalled client).
// 49 healthy sinks that always drain. We fan out a burst. The stalled sink
// must drop (and report it), but the 49 healthy sinks must ALL receive every
// event — one bad client must not block the fan-out.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client_drops_without_blocking_others() {
    let (svc, store) = pipeline();

    // 49 healthy sinks (infinite-room tally sinks).
    let healthy: Vec<Arc<TallySink>> = (0..49).map(|_| TallySink::new()).collect();
    for s in &healthy {
        store
            .add(
                SyncSession::new(Predicate::all("tasks")),
                Arc::clone(s) as Arc<dyn EventSink>,
            )
            .await;
    }

    // 1 stalled sink: depth-1 channel, receiver held but never drained.
    let (stalled_tx, _stalled_rx) = tokio::sync::mpsc::channel::<ReplicationEvent>(1);
    let stalled = Arc::new(StalledSink { tx: stalled_tx }) as Arc<dyn EventSink>;
    store
        .add(SyncSession::new(Predicate::all("tasks")), stalled)
        .await;

    let burst = 50u64;
    let mut total = FanOutOutcome::default();
    for i in 1..=burst {
        total = total.merged(
            svc.fan_out(&event_on("tasks", i), |_, _| Some(ColumnValue::Any))
                .await,
        );
    }

    // Every healthy sink received all 50 events — the stalled one did not block them.
    for (i, s) in healthy.iter().enumerate() {
        assert_eq!(
            s.delivered.load(Ordering::Relaxed),
            burst,
            "healthy sink #{i} was blocked by the stalled client (got {} of {burst})",
            s.delivered.load(Ordering::Relaxed)
        );
    }
    // The stalled sink dropped at least burst-1 (only 1 fits in the buffer).
    assert!(
        total.dropped >= burst - 1,
        "stalled sink should have dropped ≥{} events, outcome.dropped = {}",
        burst - 1,
        total.dropped
    );
}

/// A sink backed by a bounded channel whose receiver is held but never drained
/// — models a slow / stalled WebSocket client.
struct StalledSink {
    tx: tokio::sync::mpsc::Sender<ReplicationEvent>,
}

#[async_trait]
impl EventSink for StalledSink {
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
        match self.tx.try_send(event) {
            Ok(()) => DeliveryDecision::Delivered,
            Err(
                tokio::sync::mpsc::error::TrySendError::Full(_)
                | tokio::sync::mpsc::error::TrySendError::Closed(_),
            ) => DeliveryDecision::Dropped,
        }
    }
}
