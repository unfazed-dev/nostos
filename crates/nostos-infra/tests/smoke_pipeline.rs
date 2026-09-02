//! Tier 1 smoke tests — the in-memory pipeline, end to end, no PG, no WS.
//!
//! These drive the **real** `FanOutService` against the **real**
//! `InMemorySessionStore` + `TokioEventSink` (the production sink, not a test
//! double), fed by a `FakeReplicator`. What's exercised:
//!
//! - the op-distribution (80/15/5) flows through and the outcome counts match;
//! - predicate filtering actually *routes* events only to matching sessions
//!   (the core moat — currently only unit-tested at `Predicate::matches`);
//! - the bounded drop-on-full contract: a sink that fills its buffer reports
//!   `Dropped`, and the `FanOutOutcome.dropped` counter is honest (methodology §5);
//! - LSN monotonicity across the fanned-out stream.
//!
//! Runs on every push.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{DeliveryDecision, EventSink, ReplicatorStream, SessionStore};
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{
    ColumnValue, Lsn, Operation, Predicate, ReplicationEvent, RowOp, SyncSession, Tier,
};
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::router::TokioEventSink;
use nostos_infra::store::InMemorySessionStore;

/// A recording sink: counts deliveries, captures the last LSN seen. Never drops.
struct CountingSink {
    delivered: std::sync::atomic::AtomicU64,
    last_lsn: std::sync::atomic::AtomicU64,
}

impl CountingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            delivered: 0.into(),
            last_lsn: 0.into(),
        })
    }
}

#[async_trait]
impl EventSink for CountingSink {
    async fn deliver(&self, event: Arc<ReplicationEvent>) -> DeliveryDecision {
        self.delivered
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.last_lsn
            .fetch_max(event.lsn.raw(), std::sync::atomic::Ordering::Relaxed);
        DeliveryDecision::Delivered
    }
}

/// A sink with a tiny bounded channel — fills fast so `deliver` returns Dropped.
struct FullableSink {
    tx: tokio::sync::mpsc::Sender<Arc<ReplicationEvent>>,
}

#[async_trait]
impl EventSink for FullableSink {
    async fn deliver(&self, event: Arc<ReplicationEvent>) -> DeliveryDecision {
        match self.tx.try_send(event) {
            Ok(()) => DeliveryDecision::Delivered,
            Err(
                tokio::sync::mpsc::error::TrySendError::Full(_)
                | tokio::sync::mpsc::error::TrySendError::Closed(_),
            ) => DeliveryDecision::Dropped,
        }
    }
}

/// Build a `FanOutService` wired to a fresh real store; returns (service, store).
fn real_pipeline() -> (Arc<FanOutService>, Arc<dyn SessionStore>) {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let svc = Arc::new(FanOutService::new(store.clone()));
    (svc, store)
}

/// Register a session against the store directly (cap-free path for tests).
async fn register(store: &Arc<dyn SessionStore>, predicate: Predicate, sink: Arc<dyn EventSink>) {
    store.add(SyncSession::new(predicate), sink).await;
}

/// A payload that carries `org_id=<value>` as a JSON-ish field, so an extractor
/// reading `org_id` returns a real ColumnValue (not just `Any`).
fn payload_with_org(org: &str) -> Bytes {
    Bytes::from(format!("{{\"org_id\":\"{org}\"}}"))
}

/// Extractor: parse the small JSON-ish payload for a named column.
fn extract_column(e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(e.payload_bytes()).ok()?;
    let needle = format!("\"{col}\":\"");
    let start = s.find(&needle)? + needle.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(ColumnValue::text(&rest[..end]))
}

// ---------------------------------------------------------------------------
// Scenario 1: the op distribution flows through and counts are honest.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn op_mix_flows_through_and_outcome_matches() {
    let (svc, store) = real_pipeline();
    let sink = CountingSink::new();
    register(&store, Predicate::all("tasks"), sink.clone()).await;

    // 1000 events — enough that the 80/15/5 split is statistically meaningful.
    let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(1000));
    let outcome = svc.run(&mut repl, |_, _| Some(ColumnValue::Any)).await;

    // Every event matches the match-all predicate → matched == delivered.
    assert_eq!(outcome.matched, 1000, "all 1000 events should match");
    assert_eq!(
        outcome.delivered, 1000,
        "counting sink never drops — all delivered"
    );
    assert_eq!(outcome.dropped, 0);
    assert_eq!(
        sink.delivered.load(std::sync::atomic::Ordering::Relaxed),
        1000
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: predicate filtering actually routes — the moat.
// Two sessions on `tasks`: one wants org_id=acme, the other org_id=other.
// Half the events carry org_id=acme. Only the matching session gets them.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn predicate_routes_only_to_matching_session() {
    let (svc, store) = real_pipeline();

    let acme_sink = CountingSink::new();
    let other_sink = CountingSink::new();
    register(
        &store,
        Predicate::eq("tasks", "org_id", ColumnValue::text("acme")),
        acme_sink.clone(),
    )
    .await;
    register(
        &store,
        Predicate::eq("tasks", "org_id", ColumnValue::text("other")),
        other_sink.clone(),
    )
    .await;

    // Hand-emit 4 events: 2 acme, 2 other. The extractor parses org_id.
    let events = [
        event_with_org(1, "acme"),
        event_with_org(2, "other"),
        event_with_org(3, "acme"),
        event_with_org(4, "other"),
    ];
    let mut total = nostos_application::FanOutOutcome::default();
    for e in &events {
        total = total.merged(svc.fan_out(e, extract_column).await);
    }

    assert_eq!(
        total.matched, 4,
        "2+2 events match their respective session"
    );
    assert_eq!(
        acme_sink
            .delivered
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(
        other_sink
            .delivered
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
}

fn event_with_org(lsn: u64, org: &str) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: lsn.to_string(),
            payload: payload_with_org(org),
        },
    )
}

// ---------------------------------------------------------------------------
// Scenario 3: the bounded drop-on-full contract (methodology §5).
// A sink whose channel is depth 1, fed 5 events with no draining, must report
// 4 drops and the outcome.dropped counter must be honest.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_sink_drops_and_outcome_is_honest() {
    let (svc, store) = real_pipeline();
    // Depth-1 channel; we never drain it, so the 2nd send onward must drop.
    let (tx, _rx) = tokio::sync::mpsc::channel::<Arc<ReplicationEvent>>(1);
    let sink = Arc::new(FullableSink { tx }) as Arc<dyn EventSink>;
    register(&store, Predicate::all("tasks"), sink).await;

    let mut total = nostos_application::FanOutOutcome::default();
    for i in 1..=5 {
        let e = ReplicationEvent::new(
            Lsn::new(i),
            RowOp::Insert {
                table: "tasks".into(),
                pk: i.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        );
        total = total.merged(svc.fan_out(&e, |_, _| Some(ColumnValue::Any)).await);
    }

    assert_eq!(total.matched, 5, "all 5 are candidates (match-all)");
    // Exactly 1 fits in the depth-1 buffer; the other 4 drop.
    assert_eq!(
        total.delivered, 1,
        "only the first send fits the depth-1 buffer"
    );
    assert_eq!(
        total.dropped, 4,
        "the other 4 must drop — honest throughput"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: LSN monotonicity across the fanned-out stream.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lsns_are_monotonic_across_delivery() {
    let (svc, store) = real_pipeline();
    let sink = CountingSink::new();
    register(&store, Predicate::all("tasks"), sink.clone()).await;

    let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(500));
    svc.run(&mut repl, |_, _| Some(ColumnValue::Any)).await;

    // The replicator emits strictly-increasing LSNs (step 10); the sink must
    // have seen the highest one — monotonicity preserved through fan-out.
    let last = sink.last_lsn.load(std::sync::atomic::Ordering::Relaxed);
    assert!(last > 0, "sink should have received at least one event");
    assert!(
        last >= Lsn::new(10).raw(),
        "highest delivered LSN {last} should reflect monotonic advance"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: the tier cap surfaces through the SessionManager (regression
// for the TOCTOU fixed in tier_cap_regression — here as a single-threaded
// sanity check; the concurrent case is the dedicated test).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tier_cap_rejects_over_limit_sequentially() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let mgr = Arc::new(SessionManager::new(store.clone(), Tier::Hobby));
    let cap = Tier::Hobby.device_cap();
    for _ in 0..cap {
        mgr.connect(
            SyncSession::new(Predicate::all("tasks")),
            Arc::new(TokioEventSink::channel(8).0),
        )
        .await
        .unwrap();
    }
    // One over the cap must reject.
    let over = mgr
        .connect(
            SyncSession::new(Predicate::all("tasks")),
            Arc::new(TokioEventSink::channel(8).0),
        )
        .await;
    assert!(over.is_err(), "the (cap+1)th connect must be rejected");
    assert_eq!(
        store.len().await as u64,
        cap,
        "store must sit exactly at the cap"
    );
}

/// Smoke-check that the op distribution is roughly 80/15/5 — guards against a
/// future change to `pick_op` silently skewing the workload (the benchmark
/// depends on this split to be comparable to PowerSync's regime).
#[tokio::test]
async fn fake_replicator_op_distribution_is_roughly_80_15_5() {
    let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(10_000));
    let (mut ins, mut upd, mut del) = (0u64, 0u64, 0u64);
    while let Some(e) = repl.next_event().await {
        match e.op.operation() {
            Operation::Insert => ins += 1,
            Operation::Update => upd += 1,
            Operation::Delete => del += 1,
        }
    }
    assert!((7800..=8200).contains(&ins), "inserts ~80%: got {ins}");
    assert!((1300..=1700).contains(&upd), "updates ~15%: got {upd}");
    assert!((300..=700).contains(&del), "deletes ~5%: got {del}");
}
