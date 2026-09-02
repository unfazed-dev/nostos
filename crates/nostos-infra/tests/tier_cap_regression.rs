//! Regression: the concurrent-device cap must hold under concurrent connects.
//!
//! `SessionManager::connect` historically did `store.len().await` (check) then
//! `store.add().await` (act) across two awaits with no atomicity. With the real
//! `InMemorySessionStore` those acquire *different* locks (len iterates each
//! per-table mutex one at a time; add locks only the target table), so N
//! concurrent connects can each read the same count and all overshoot the cap.
//!
//! This test spawns 2× the Hobby cap (200) connects on a multi-thread runtime
//! so the scheduler interleaves them, then asserts the live count never exceeds
//! the cap. On a buggy store it overshoots; on a fixed (atomic check-and-insert)
//! store it fills exactly to the cap and the rest reject with `DeviceCapReached`.
//!
//! Runs on every push — no PG, no WS, just the in-memory store + use-case.

// Tier caps are small known constants (Hobby = 100); the truncation cast to
// usize is safe in practice and clippy's 32-bit-pointer concern doesn't apply.
#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use async_trait::async_trait;
use nostos_application::ports::{DeliveryDecision, EventSink};
use nostos_application::SessionManager;
use nostos_domain::{Predicate, ReplicationEvent, SyncSession, Tier};
use nostos_infra::store::InMemorySessionStore;

/// A sink that accepts everything and never blocks — the cheapest valid sink.
struct NoopSink;
#[async_trait]
impl EventSink for NoopSink {
    async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
        DeliveryDecision::Delivered
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cap_holds_under_concurrent_connects() {
    let store_dyn: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let mgr = Arc::new(SessionManager::new(store_dyn.clone(), Tier::Hobby));

    let cap = Tier::Hobby.device_cap();
    let attempts = cap as usize * 2; // 200 concurrent connects against a 100 cap

    // Fire them all from independent tasks so the runtime interleaves them.
    let sink = Arc::new(NoopSink) as Arc<dyn EventSink>;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..attempts {
        let session = SyncSession::new(Predicate::all("tasks"));
        let sink_c = Arc::clone(&sink);
        let mgr_c = Arc::clone(&mgr);
        set.spawn(async move { mgr_c.connect(session, sink_c).await.is_ok() });
    }

    let mut ok = 0usize;
    while let Some(res) = set.join_next().await {
        if res.unwrap() {
            ok += 1;
        }
    }

    let live = store_dyn.len().await;
    assert!(
        live as u64 <= cap,
        "TOCTOU: cap {cap} exceeded — store has {live} sessions ({ok} connects succeeded of {attempts} attempted)"
    );
    assert_eq!(
        live as u64, cap,
        "expected the store filled exactly to the cap {cap}, got {live}"
    );
}
