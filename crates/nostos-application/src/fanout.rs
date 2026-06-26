//! `FanOutService` — the hot loop that is Nostos's throughput moat.
//!
//! Pipeline:
//! ```text
//!   ReplicatorStream.next_event()
//!        │
//!        ▼
//!   SessionStore.candidates_for(event)   ← O(1) by Predicate.table index
//!        │   returns Vec<SessionCandidate>
//!        ▼
//!   for candidate in candidates:
//!       if candidate.predicate.matches(extract_columns(event)):
//!           candidate.sink.deliver(event).await   ← bounded; slow client → Drop
//!        │
//!        ▼
//!   return FanOutOutcome { delivered, dropped, matched }
//! ```
//!
//! Complexity is **O(changed rows × matching sessions)**, not O(all sessions) —
//! the table index prunes the candidate set before filter evaluation. This is
//! what scales past PowerSync's static-bucket model (ADR-0003).

use std::sync::Arc;

use tracing::trace;

use nostos_domain::{ColumnValue, ReplicationEvent};

use crate::ports::{DeliveryDecision, ReplicatorStream, SessionStore};

/// The result of fanning one event out to all matching sessions.
///
/// Returned per-event so the caller (the server driver, or the benchmark) can
/// aggregate honest throughput numbers: how many sessions matched, how many of
/// those actually received the event vs. were dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanOutOutcome {
    /// Sessions whose predicate matched this event (candidate-count after
    /// filter evaluation).
    pub matched: u64,
    /// Events actually accepted by a sink.
    pub delivered: u64,
    /// Events dropped because a sink's bounded buffer was full.
    pub dropped: u64,
}

impl FanOutOutcome {
    /// Combine two outcomes by summing their counters.
    #[inline]
    #[must_use]
    pub const fn merged(self, other: Self) -> Self {
        Self {
            matched: self.matched.saturating_add(other.matched),
            delivered: self.delivered.saturating_add(other.delivered),
            dropped: self.dropped.saturating_add(other.dropped),
        }
    }
}

/// The fan-out engine. Holds references to the replicator (event source) and
/// the session store (delivery targets) behind the application's port traits.
///
/// Constructed once at server startup and driven by [`FanOutService::run`],
/// which loops until the replicator stream is exhausted.
pub struct FanOutService {
    store: Arc<dyn SessionStore>,
}

impl FanOutService {
    #[inline]
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    /// Fan a single event out to all matching sessions. This is the unit the
    /// benchmark counts as "one op" — and the unit PowerSync's 2-4k ops/sec
    /// ceiling refers to (one row change processed through the router).
    ///
    /// `column_extractor` lifts column values out of the event's payload so the
    /// domain-layer [`Predicate`] can be evaluated. The extractor is supplied
    /// by the caller (the wire codec in infra knows the payload encoding); the
    /// application layer stays decoupled from any specific tuple format.
    ///
    /// Deliveries to matching sinks are dispatched **concurrently** via a
    /// `JoinSet` — each `EventSink::deliver` is non-blocking (`try_send` on a
    /// bounded channel), so fanning out to 10,000 sessions spreads across the
    /// tokio runtime instead of serializing on one task. This is what lets the
    /// router scale past the 1-to-N sequential wall.
    pub async fn fan_out<F>(&self, event: &ReplicationEvent, column_extractor: F) -> FanOutOutcome
    where
        F: Fn(&ReplicationEvent, &str) -> Option<ColumnValue>,
    {
        // Pre-filter candidates by predicate, then dispatch deliveries
        // concurrently. `deliver` is non-blocking (`try_send` on a bounded
        // channel), so fanning out to 10,000 sessions spreads across the tokio
        // runtime instead of serializing on one task — this is what lets the
        // router scale past the 1-to-N sequential wall.
        let matched: Vec<_> = self
            .store
            .candidates_for(event)
            .await
            .into_iter()
            .filter(|c| c.predicate.matches(|col| column_extractor(event, col)))
            .collect();
        let matched_count = matched.len() as u64;

        let mut set = tokio::task::JoinSet::new();
        for c in matched {
            let ev = event.clone();
            set.spawn(async move { c.sink.deliver(ev).await });
        }
        let mut delivered = 0u64;
        let mut dropped = 0u64;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(DeliveryDecision::Delivered) => delivered += 1,
                // A drop OR a panicked task both count as "not delivered" → drop.
                Ok(DeliveryDecision::Dropped) | Err(_) => dropped += 1,
            }
        }
        let outcome = FanOutOutcome {
            matched: matched_count,
            delivered,
            dropped,
        };
        trace!(?outcome, "fan_out complete");
        outcome
    }

    /// Drive the replicator → fan-out loop to exhaustion. Returns the
    /// aggregated outcome over all events processed.
    ///
    /// `column_extractor` is called once per candidate per event — see
    /// [`Self::fan_out`].
    pub async fn run<F>(
        &self,
        replicator: &mut dyn ReplicatorStream,
        column_extractor: F,
    ) -> FanOutOutcome
    where
        F: Fn(&ReplicationEvent, &str) -> Option<ColumnValue>,
    {
        let mut total = FanOutOutcome::default();
        while let Some(event) = replicator.next_event().await {
            total = total.merged(self.fan_out(&event, &column_extractor).await);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{EventSink, SessionCandidate, SessionStore};
    use async_trait::async_trait;
    use bytes::Bytes;
    use nostos_domain::{Lsn, Predicate, RowOp, SessionId, SyncSession};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ---- test doubles ----

    /// A sink that records every delivered event and never drops.
    struct RecordingSink {
        events: Arc<Mutex<Vec<ReplicationEvent>>>,
        open: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
            self.events.lock().unwrap().push(event);
            DeliveryDecision::Delivered
        }
        fn is_open(&self) -> bool {
            *self.open.lock().unwrap()
        }
    }

    /// An in-memory store keyed by table — the simplest correct SessionStore.
    struct TableStore {
        by_table: Mutex<HashMap<String, Vec<SessionCandidate>>>,
    }

    #[async_trait]
    impl SessionStore for TableStore {
        async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>) {
            let table = session.predicate.table.clone();
            let cand = SessionCandidate {
                id: session.id,
                predicate: session.predicate,
                sink,
            };
            self.by_table
                .lock()
                .unwrap()
                .entry(table)
                .or_default()
                .push(cand);
        }
        async fn remove(&self, id: SessionId) {
            let mut g = self.by_table.lock().unwrap();
            for sessions in g.values_mut() {
                sessions.retain(|c| c.id != id);
            }
        }
        async fn candidates_for(&self, event: &ReplicationEvent) -> Vec<SessionCandidate> {
            self.by_table
                .lock()
                .unwrap()
                .get(event.table())
                .cloned()
                .unwrap_or_default()
        }
        async fn len(&self) -> usize {
            self.by_table.lock().unwrap().values().map(Vec::len).sum()
        }
    }

    fn make_store() -> Arc<TableStore> {
        Arc::new(TableStore {
            by_table: Mutex::new(HashMap::new()),
        })
    }

    // A trivial extractor: decode the payload as "org_id=<value>" for testing.
    fn extract_org(_e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
        if col == "org_id" {
            Some(ColumnValue::text("acme"))
        } else {
            None
        }
    }

    fn insert_event(table: &str) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(1),
            RowOp::Insert {
                table: table.into(),
                pk: "1".into(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    #[tokio::test]
    async fn matches_go_to_matching_sessions_only() {
        let store = make_store();
        // Two sessions on "tasks": one scoped to org_id=acme, one match-all.
        let sink_a = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
            open: Arc::new(Mutex::new(true)),
        });
        let sink_b = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
            open: Arc::new(Mutex::new(true)),
        });
        let events_a = sink_a.events.clone();
        let events_b = sink_b.events.clone();

        store
            .add(
                SyncSession::new(Predicate::eq("tasks", "org_id", ColumnValue::text("acme"))),
                sink_a,
            )
            .await;
        store
            .add(SyncSession::new(Predicate::all("tasks")), sink_b)
            .await;

        let svc = FanOutService::new(store);
        let outcome = svc.fan_out(&insert_event("tasks"), extract_org).await;

        // Both sessions are candidates (same table). Predicate filters:
        //   sink_a predicate (org_id=acme) matches → delivered.
        //   sink_b predicate (match-all) matches → delivered.
        assert_eq!(outcome.matched, 2);
        assert_eq!(outcome.delivered, 2);
        assert_eq!(outcome.dropped, 0);
        assert_eq!(events_a.lock().unwrap().len(), 1);
        assert_eq!(events_b.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn non_matching_table_is_pruned() {
        let store = make_store();
        let sink = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
            open: Arc::new(Mutex::new(true)),
        });
        let events = sink.events.clone();
        store
            .add(SyncSession::new(Predicate::all("tasks")), sink)
            .await;

        let svc = FanOutService::new(store);
        // Event on a different table → no candidates.
        let outcome = svc.fan_out(&insert_event("users"), extract_org).await;
        assert_eq!(outcome.matched, 0);
        assert_eq!(outcome.delivered, 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn outcome_merges_accumulate() {
        let a = FanOutOutcome {
            matched: 5,
            delivered: 4,
            dropped: 1,
        };
        let b = FanOutOutcome {
            matched: 3,
            delivered: 3,
            dropped: 0,
        };
        let m = a.merged(b);
        assert_eq!(
            m,
            FanOutOutcome {
                matched: 8,
                delivered: 7,
                dropped: 1
            }
        );
    }
}
