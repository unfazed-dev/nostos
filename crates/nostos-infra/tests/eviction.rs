//! WAL-bloat eviction — integration test for ADR-0016.
//!
//! Proves the eviction policy actually disconnects the slowest session when it
//! crosses the lag threshold, and that under-threshold sessions are untouched.
//! Default-OFF is asserted in the policy unit tests; this asserts the wiring
//! (fanout → store.remove → session gone) over a real FanOutService.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{
    DeliveryDecision, EventSink, ReplicatorStream, SessionStore, StoreRejection,
};
use nostos_application::{EvictionPolicy, FanOutService};
use nostos_domain::{Lsn, Predicate, ReplicationEvent, RowOp, SessionId, SyncSession};
use nostos_infra::store::InMemorySessionStore;

/// A sink whose acked LSN we can pin — so a session looks arbitrarily slow
/// without waiting for real wall-clock lag.
struct PinnedAckSink {
    acked: Lsn,
    delivered: Lsn,
}

#[async_trait]
impl EventSink for PinnedAckSink {
    async fn deliver(&self, _event: ReplicationEvent) -> DeliveryDecision {
        DeliveryDecision::Delivered
    }
    fn last_acked_lsn(&self) -> Option<Lsn> {
        Some(self.acked)
    }
    fn last_delivered_lsn(&self) -> Option<Lsn> {
        Some(self.delivered)
    }
}

/// A replicator that emits exactly one event at a fixed LSN, then stops.
struct OneShotRepl {
    fired: bool,
    lsn: u64,
}

#[async_trait]
impl ReplicatorStream for OneShotRepl {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if self.fired {
            return None;
        }
        self.fired = true;
        Some(ReplicationEvent::new(
            Lsn::new(self.lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: "x".into(),
                payload: Bytes::from_static(b"p"),
            },
        ))
    }
}

/// Register a session that reports a pinned acked LSN of `acked`. Returns its id
/// (via `try_add_below_cap`, which reports the id — `add` does not).
async fn register_slow_session(store: &Arc<dyn SessionStore>, acked: u64) -> SessionId {
    let sink = Arc::new(PinnedAckSink {
        acked: Lsn::new(acked),
        delivered: Lsn::new(acked),
    });
    let session = SyncSession::new(Predicate::all("tasks"));
    match store.try_add_below_cap(session, sink, u64::MAX).await {
        Ok(id) => id,
        Err(StoreRejection::CapExceeded { .. }) => unreachable!("u64::MAX cap never exceeded"),
    }
}

#[tokio::test]
async fn slow_session_is_evicted_when_lag_exceeds_threshold() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());

    // A session acked only up to LSN 100; the head of the stream is 100_000.
    // Gap = 99_900 > 1_000 threshold → evict.
    let slow = register_slow_session(&store, 100).await;
    assert_eq!(store.len().await, 1);

    let fanout = FanOutService::new(Arc::clone(&store)).with_eviction(EvictionPolicy::new(1_000));
    let mut repl = OneShotRepl {
        fired: false,
        lsn: 100_000,
    };
    let extract = |_e: &ReplicationEvent, _col: &str| Some(nostos_domain::ColumnValue::Any);
    fanout.run(&mut repl, extract).await;

    // The slow session was removed by the eviction hook.
    assert_eq!(store.len().await, 0, "the slow session was evicted");
    // And slowest_session would now report None (no sessions left).
    assert!(store.slowest_session().await.is_none());
    let _ = slow; // keep the id alive for clarity
}

#[tokio::test]
async fn session_within_threshold_is_not_evicted() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());

    // A session acked up to 99_500; head is 100_000. Gap = 500 ≤ 1_000 → keep.
    register_slow_session(&store, 99_500).await;
    assert_eq!(store.len().await, 1);

    let fanout = FanOutService::new(Arc::clone(&store)).with_eviction(EvictionPolicy::new(1_000));
    let mut repl = OneShotRepl {
        fired: false,
        lsn: 100_000,
    };
    let extract = |_e: &ReplicationEvent, _col: &str| Some(nostos_domain::ColumnValue::Any);
    fanout.run(&mut repl, extract).await;

    assert_eq!(
        store.len().await,
        1,
        "the within-threshold session was kept"
    );
}

#[tokio::test]
async fn disabled_policy_never_evicts_even_with_huge_lag() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());

    // Pathological lag, but eviction is OFF (the default).
    register_slow_session(&store, 1).await;
    assert_eq!(store.len().await, 1);

    let fanout = FanOutService::new(Arc::clone(&store)); // no with_eviction → disabled
    let mut repl = OneShotRepl {
        fired: false,
        lsn: 1_000_000_000,
    };
    let extract = |_e: &ReplicationEvent, _col: &str| Some(nostos_domain::ColumnValue::Any);
    fanout.run(&mut repl, extract).await;

    assert_eq!(store.len().await, 1, "disabled policy never evicts");
}

#[tokio::test]
async fn only_the_slowest_session_is_evicted() {
    // Two sessions: one slow (gap > threshold), one fast (within). Only the
    // slow one should be removed — the fast one survives.
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    register_slow_session(&store, 100).await; // slow
    register_slow_session(&store, 99_999).await; // fast (gap = 1, within 1_000)
    assert_eq!(store.len().await, 2);

    let fanout = FanOutService::new(Arc::clone(&store)).with_eviction(EvictionPolicy::new(1_000));
    let mut repl = OneShotRepl {
        fired: false,
        lsn: 100_000,
    };
    let extract = |_e: &ReplicationEvent, _col: &str| Some(nostos_domain::ColumnValue::Any);
    fanout.run(&mut repl, extract).await;

    assert_eq!(
        store.len().await,
        1,
        "exactly the slow one was evicted, the fast one kept"
    );
}
