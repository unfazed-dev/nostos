//! In-memory `SessionStore` — the production-grade adapter for Week 1.
//!
//! Indexes sessions by `predicate.table` for O(1) candidate pruning. Backed by
//! a [`DashMap`] (sharded concurrent map) so add/remove/lookup scale across
//! the fan-out tasks without a single global lock.
//!
//! Why not a `Mutex<HashMap>`? The fan-out hot path calls `candidates_for` for
//! *every* event; under thousands of concurrent sessions a single mutex would
//! serialize the router. DashMap shards by key, so lookups on different tables
//! don't contend.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::Mutex;

use nostos_application::ports::{SessionCandidate, SessionStore};
use nostos_domain::{ReplicationEvent, SessionId, SyncSession};

use crate::router::SessionSinkHandle;

/// A concurrent, table-indexed session store.
///
/// `by_table` maps `predicate.table → Vec<(SessionId, SessionSinkHandle)>`.
/// The inner `Vec` is guarded by a per-table `Mutex` so add/remove on one
/// table doesn't block lookups on another.
pub struct InMemorySessionStore {
    by_table: DashMap<String, Arc<Mutex<Vec<StoredSession>>>>,
}

struct StoredSession {
    id: SessionId,
    predicate: nostos_domain::Predicate,
    handle: SessionSinkHandle,
}

impl InMemorySessionStore {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_table: DashMap::new(),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn add(&self, session: SyncSession, sink: Arc<dyn nostos_application::ports::EventSink>) {
        let table = session.predicate.table.clone();
        let stored = StoredSession {
            id: session.id,
            predicate: session.predicate,
            handle: SessionSinkHandle::new(sink),
        };
        let entry = self
            .by_table
            .entry(table)
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
        entry.lock().await.push(stored);
    }

    async fn remove(&self, id: SessionId) {
        // A session lives in exactly one table's list; scan all to be safe
        // (cheap — sessions per table is small after pruning).
        for entry in &self.by_table {
            let mut list = entry.value().lock().await;
            let before = list.len();
            list.retain(|s| s.id != id);
            if list.len() != before {
                break; // found & removed
            }
        }
    }

    async fn candidates_for(&self, event: &ReplicationEvent) -> Vec<SessionCandidate> {
        let Some(list_arc) = self
            .by_table
            .get(event.table())
            .map(|r| Arc::clone(r.value()))
        else {
            return Vec::new();
        };
        let list = list_arc.lock().await;
        list.iter()
            .map(|s| SessionCandidate {
                id: s.id,
                predicate: s.predicate.clone(),
                sink: s.handle.sink(),
            })
            .collect()
    }

    async fn len(&self) -> usize {
        let mut total = 0usize;
        for entry in &self.by_table {
            total += entry.value().lock().await.len();
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_application::ports::{DeliveryDecision, EventSink};
    use nostos_domain::{Lsn, Predicate, ReplicationEvent, RowOp};

    struct NoopSink;
    #[async_trait]
    impl EventSink for NoopSink {
        async fn deliver(&self, _e: ReplicationEvent) -> DeliveryDecision {
            DeliveryDecision::Delivered
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    fn event_on(table: &str) -> ReplicationEvent {
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
    async fn add_lookup_remove() {
        let store = InMemorySessionStore::new();
        let s = SyncSession::new(Predicate::all("tasks"));
        let id = s.id;
        store.add(s, Arc::new(NoopSink)).await;

        assert_eq!(store.len().await, 1);
        let cands = store.candidates_for(&event_on("tasks")).await;
        assert_eq!(cands.len(), 1);
        // Different table → no candidates.
        assert!(store.candidates_for(&event_on("users")).await.is_empty());

        store.remove(id).await;
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn many_sessions_same_table() {
        let store = InMemorySessionStore::new();
        for _ in 0..1000 {
            store
                .add(
                    SyncSession::new(Predicate::all("tasks")),
                    Arc::new(NoopSink),
                )
                .await;
        }
        assert_eq!(store.len().await, 1000);
        let cands = store.candidates_for(&event_on("tasks")).await;
        assert_eq!(cands.len(), 1000);
    }
}
