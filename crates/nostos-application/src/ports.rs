//! Port traits — the driven-side interfaces the infrastructure layer implements.
//!
//! These are the seams that make Nostos hexagonal: the use-cases talk to these
//! traits, not to concrete adapters. That's what lets the benchmark swap a
//! `FakeReplicator` in for the real `PgReplicator` with zero use-case changes,
//! and lets unit tests run the fan-out loop with no tokio runtime at all.
//!
//! **Async note:** the ports are `async` because the fan-out loop awaits
//! delivery. `async_trait` is used so the same trait works for both sync test
//! doubles and async adapters. The domain layer stays pure (ADR-0001); only
//! this layer sees `async`.

use std::sync::Arc;

use async_trait::async_trait;

use nostos_domain::{Predicate, ReplicationEvent, SessionId, SyncSession};

/// The outcome of attempting to deliver an event to a session.
///
/// The router uses this to maintain drop/latency accounting — an honest
/// throughput number must report drops, not hide them (see
/// `BENCHMARK-METHODOLOGY.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDecision {
    /// The event was accepted by the session's sink.
    Delivered,
    /// The session's bounded buffer was full, so this event was dropped to
    /// protect the router from head-of-line blocking. Counted, not silent.
    Dropped,
}

/// A delivery target for one session — implemented by the infra layer (a tokio
/// channel per WebSocket connection) and by test doubles (a recording sink).
///
/// Implementations decide their own backpressure strategy. The production
/// `TokioEventSink` drops when its bounded channel is full; the test
/// `RecordingSink` never drops (capacity is unlimited).
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Attempt to deliver one event. Non-blocking from the router's POV —
    /// returns promptly with a [`DeliveryDecision`].
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision;

    /// Whether this sink is still accepting events (the underlying connection
    /// is alive). A closed sink is removed by the [`SessionStore`].
    fn is_open(&self) -> bool;
}

/// A live set of sync sessions, indexed for fast predicate evaluation.
///
/// The contract is intentionally minimal: add/remove sessions, and — the hot
/// path — find the candidate sessions whose predicate *might* match an event.
/// `candidates_for` is expected to prune aggressively (by `predicate.table` at
/// minimum) so the router evaluates filters against a small candidate set.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Register a session with its delivery sink. The store indexes it by
    /// `predicate.table`.
    async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>);

    /// Remove a session by id (connection closed / dropped).
    async fn remove(&self, id: SessionId);

    /// Return the sessions whose `predicate.table` matches `event`'s table,
    /// paired with their sinks. The router then runs `Predicate::matches` on
    /// each to decide delivery.
    ///
    /// Implementations should index by table for O(1) pruning. Returning all
    /// sessions on every event is a correctness-preserving but slow fallback.
    async fn candidates_for(&self, event: &ReplicationEvent) -> Vec<SessionCandidate>;

    /// Total number of live sessions (for metrics / dashboards).
    async fn len(&self) -> usize;

    /// Whether there are zero live sessions.
    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// A session + its sink, returned by [`SessionStore::candidates_for`].
///
/// Carrying the predicate alongside lets the router evaluate filters without a
/// second lookup.
#[derive(Clone)]
pub struct SessionCandidate {
    pub id: SessionId,
    pub predicate: Predicate,
    pub sink: Arc<dyn EventSink>,
}

/// Source of replication events — the driven-side port a replicator implements.
///
/// The production `PgReplicator` reads Postgres logical replication (WAL →
/// `pgoutput`); the benchmark `FakeReplicator` generates synthetic events. Both
/// implement this trait, so the fan-out loop is identical.
#[async_trait]
pub trait ReplicatorStream: Send {
    /// Block until the next replication event is available, or return `None`
    /// when the stream is permanently exhausted (clean shutdown).
    async fn next_event(&mut self) -> Option<ReplicationEvent>;
}
