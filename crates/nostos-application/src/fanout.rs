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

use crate::ports::{DeliveryDecision, Metrics, ReplicatorStream, SessionStore};

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
///
/// **Reactive-when-connected (default strategy):** `push_interval` sets a
/// minimum cadence between fan-out dispatches in `run`. Default is zero
/// (instant, what the benchmark measures). A managed deploy sets ~1-2s to
/// coalesce bursts server-side — this keeps the four FFI bridges dumb and the
/// policy single-sourced here, per the reactive-default ultrathink decision.
pub struct FanOutService {
    store: Arc<dyn SessionStore>,
    push_interval: std::time::Duration,
    /// Aggregate throughput counters, read by `/metrics`. `None` in unit tests
    /// that assert on `FanOutOutcome` directly (counters would duplicate it).
    metrics: Option<Arc<Metrics>>,
    /// WAL-bloat protection: evict the slowest session when it lags further
    /// than the policy's threshold behind the head of the stream. Default
    /// disabled ([`EvictionPolicy::disabled`]) — see ADR-0016.
    eviction: crate::EvictionPolicy,
}

impl FanOutService {
    #[inline]
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            push_interval: std::time::Duration::ZERO,
            metrics: None,
            eviction: crate::EvictionPolicy::disabled(),
        }
    }

    /// Attach an aggregate metrics handle updated on every fan-out dispatch.
    /// The server constructs one `Arc<Metrics>` and shares it between this
    /// service (writer) and the `/metrics` endpoint (reader).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Enable WAL-bloat protection: evict the slowest session when its acked
    /// LSN lags further than the policy's `max_lag` behind the head of the
    /// stream. Disabled by default (ADR-0016) — a production deploy MUST opt in.
    #[must_use]
    pub fn with_eviction(mut self, policy: crate::EvictionPolicy) -> Self {
        self.eviction = policy;
        self
    }

    /// Set the minimum interval between fan-out dispatches in `run`.
    /// `Duration::ZERO` (the default) means instant delivery — what the
    /// benchmark measures. A reactive-when-connected managed instance sets
    /// this to coalesce bursts server-side.
    #[must_use]
    pub fn with_push_interval(mut self, interval: std::time::Duration) -> Self {
        self.push_interval = interval;
        self
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
        // Aggregate counters for /metrics (lock-free; no-op when no handle).
        if let Some(m) = &self.metrics {
            use std::sync::atomic::Ordering;
            m.matched.fetch_add(outcome.matched, Ordering::Relaxed);
            m.delivered.fetch_add(outcome.delivered, Ordering::Relaxed);
            m.dropped.fetch_add(outcome.dropped, Ordering::Relaxed);
        }
        trace!(?outcome, "fan_out complete");
        outcome
    }

    /// Drive the replicator → fan-out loop to exhaustion. Returns the
    /// aggregated outcome over all events processed.
    ///
    /// `column_extractor` is called once per candidate per event — see
    /// [`Self::fan_out`].
    ///
    /// After each event is fanned out, the loop advances the replicator's
    /// durable-progress cursor to the minimum acked LSN across live sessions
    /// (ADR-0009: ack-driven slot advance). This is what prevents the source's
    /// WAL-retention slot from advancing past data a client never confirmed —
    /// the silent-data-loss-on-resume bug the per-event advance had.
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
            // Ack-driven progress: advance the slot only as far as the slowest
            // live client has confirmed. None = no session has acked → don't
            // advance (WAL retained; no data loss). The replicator no-ops if
            // it has no real slot (FakeReplicator).
            // Ack-driven progress + WAL-bloat protection share the same scan
            // (the slowest client's acked LSN), so compute it once.
            let slowest_acked = self.store.min_acked_lsn().await;
            if let Some(safe) = slowest_acked {
                replicator.advance_progress(safe).await;
            }
            // WAL-bloat protection (ADR-0016): if the slowest client has fallen
            // further than the policy's threshold behind this event (the head of
            // the stream), disconnect it. It reconnects + re-syncs from a fresh
            // checkpoint — trading a controlled replay window for source-DB
            // safety. OFF by default; a production deploy opts in via config.
            if self.eviction.should_evict(event.lsn, slowest_acked) {
                if let Some((id, _)) = self.store.slowest_session().await {
                    tracing::warn!(
                        session = ?id,
                        head = event.lsn.raw(),
                        "evicting slowest session (WAL-bloat protection); client will reconnect + re-sync"
                    );
                    self.store.remove(id).await;
                }
            }
            // Reactive-when-connected cadence: a zero interval (the default,
            // what the benchmark measures) is a no-op; a managed instance sets
            // ~1-2s to coalesce bursts server-side.
            if !self.push_interval.is_zero() {
                tokio::time::sleep(self.push_interval).await;
            }
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
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
            self.events.lock().unwrap().push(event);
            DeliveryDecision::Delivered
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
        async fn try_add_below_cap(
            &self,
            session: SyncSession,
            sink: Arc<dyn EventSink>,
            cap: u64,
        ) -> Result<SessionId, crate::ports::StoreRejection> {
            let mut g = self.by_table.lock().unwrap();
            let live: usize = g.values().map(Vec::len).sum();
            if (live as u64) >= cap {
                return Err(crate::ports::StoreRejection::CapExceeded { cap });
            }
            let id = session.id;
            let table = session.predicate.table.clone();
            g.entry(table).or_default().push(SessionCandidate {
                id,
                predicate: session.predicate,
                sink,
            });
            Ok(id)
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
        async fn min_acked_lsn(&self) -> Option<nostos_domain::Lsn> {
            None
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
        });
        let sink_b = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
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

    /// ADR-0012 slice 1: the boolean tree routes through `fan_out` end-to-end.
    /// An `Or`-predicate session receives events matching either branch; a
    /// `Not`-predicate session excludes events its inner `Eq` would match.
    #[tokio::test]
    async fn boolean_tree_or_and_not_route_through_fanout() {
        let store = make_store();
        let sink_or = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
        });
        let sink_not = Arc::new(RecordingSink {
            events: Arc::new(Mutex::new(vec![])),
        });
        let events_or = sink_or.events.clone();
        let events_not = sink_not.events.clone();

        // Or-branch: status=open OR status=in_progress.
        store
            .add(
                SyncSession::new(
                    Predicate::eq("tasks", "status", ColumnValue::text("open"))
                        .or_eq("status", ColumnValue::text("in_progress")),
                ),
                sink_or,
            )
            .await;
        // Not-branch: NOT status=archived (everything that isn't archived).
        store
            .add(
                SyncSession::new(!Predicate::eq(
                    "tasks",
                    "status",
                    ColumnValue::text("archived"),
                )),
                sink_not,
            )
            .await;

        // Extractor: lift `status` straight out of the payload bytes.
        let extract_status = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            if col == "status" {
                Some(ColumnValue::text(String::from_utf8_lossy(
                    e.payload_bytes(),
                )))
            } else {
                None
            }
        };

        let svc = FanOutService::new(store);
        // Helper to build an event carrying a `status` value as its payload.
        let status_event = |status: &str| {
            ReplicationEvent::new(
                Lsn::new(1),
                RowOp::Insert {
                    table: "tasks".into(),
                    pk: status.into(),
                    payload: Bytes::copy_from_slice(status.as_bytes()),
                },
            )
        };

        // open → Or-branch matches (delivered to sink_or); Not(archived) also
        // matches (delivered to sink_not).
        let o = svc.fan_out(&status_event("open"), extract_status).await;
        assert_eq!(o.matched, 2);
        assert_eq!(o.delivered, 2);
        assert_eq!(events_or.lock().unwrap().len(), 1);
        assert_eq!(events_not.lock().unwrap().len(), 1);

        // archived → Or-branch does NOT match; Not(archived) does NOT match
        // either (the inner Eq matches, Not inverts it). So NEITHER predicate
        // matches: matched=0, nothing delivered, nothing dropped (dropped only
        // counts matched-but-undelivered).
        let o = svc.fan_out(&status_event("archived"), extract_status).await;
        assert_eq!(o.matched, 0);
        assert_eq!(o.delivered, 0);
        assert_eq!(o.dropped, 0);
        // Still just the one event each from the previous fan-out.
        assert_eq!(events_or.lock().unwrap().len(), 1);
        assert_eq!(events_not.lock().unwrap().len(), 1);

        // in_progress → Or-branch matches; Not(archived) matches.
        let o = svc
            .fan_out(&status_event("in_progress"), extract_status)
            .await;
        assert_eq!(o.delivered, 2);
        assert_eq!(events_or.lock().unwrap().len(), 2);
        assert_eq!(events_not.lock().unwrap().len(), 2);
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
