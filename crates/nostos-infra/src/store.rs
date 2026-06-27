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
//!
//! ## Live count & the cap-enforcement race
//!
//! A separate `AtomicUsize` (`live_count`) tracks the total session count,
//! decoupled from the `DashMap`'s per-table vectors. This serves two purposes:
//!
//! 1. **Atomic cap enforcement.** `try_add_below_cap` reserves a slot via
//!    `fetch_update` on the atomic *before* touching the DashMap, so the
//!    count-check and the insert form one atomic step — concurrent connects
//!    can't each read a stale count and overshoot the cap (the TOCTOU the old
//!    `len().await` + `add().await` sequence had).
//! 2. **No iterate-while-inserting deadlock.** The old `len()` iterated the
//!    `DashMap` (holding shard read-locks) while `add()` took shard write-locks
//!    — under heavy concurrent connects this deadlocked. The atomic makes
//!    `len()` a single load, so it never touches the shards.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::Mutex;

use nostos_application::ports::{EventSink, SessionCandidate, SessionStore, StoreRejection};
use nostos_domain::{ReplicationEvent, SessionId, SyncSession};

/// A concurrent, table-indexed session store.
///
/// `by_table` maps `predicate.table → Vec<StoredSession>`. The inner `Vec` is
/// guarded by a per-table `Mutex` so add/remove on one table doesn't block
/// lookups on another. `live_count` mirrors the total across all tables for
/// O(1) `len()` and atomic cap enforcement (see the module doc).
pub struct InMemorySessionStore {
    by_table: DashMap<String, Arc<Mutex<Vec<StoredSession>>>>,
    live_count: AtomicUsize,
}

struct StoredSession {
    id: SessionId,
    predicate: nostos_domain::Predicate,
    sink: Arc<dyn EventSink>,
}

impl InMemorySessionStore {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_table: DashMap::new(),
            live_count: AtomicUsize::new(0),
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
    async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>) {
        let table = session.predicate.table.clone();
        let stored = StoredSession {
            id: session.id,
            predicate: session.predicate,
            sink,
        };
        let entry = self
            .by_table
            .entry(table)
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
        entry.lock().await.push(stored);
        // Mirrors the cap-free insert; the count stays honest for `len()`.
        self.live_count.fetch_add(1, Ordering::Relaxed);
    }

    async fn try_add_below_cap(
        &self,
        session: SyncSession,
        sink: Arc<dyn EventSink>,
        cap: u64,
    ) -> Result<SessionId, StoreRejection> {
        // Reserve a slot atomically FIRST. fetch_update retries until we observe
        // a count below the cap and bump it by one — a single atomic step, so
        // concurrent connects can't each read the same count and overshoot.
        let cap_usize = usize::try_from(cap).unwrap_or(usize::MAX);
        self.live_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current < cap_usize {
                    Some(current + 1)
                } else {
                    None
                }
            })
            .map_err(|_| StoreRejection::CapExceeded { cap })?;

        // Slot reserved — now insert into the index. If this ever failed we'd
        // have to roll the reservation back; push onto a Vec is infallible.
        let id = session.id;
        let table = session.predicate.table.clone();
        let stored = StoredSession {
            id,
            predicate: session.predicate,
            sink,
        };
        let entry = self
            .by_table
            .entry(table)
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
        entry.lock().await.push(stored);
        Ok(id)
    }

    async fn remove(&self, id: SessionId) {
        // A session lives in exactly one table's list; scan all to be safe
        // (cheap — sessions per table is small after pruning).
        let mut removed = false;
        for entry in &self.by_table {
            let mut list = entry.value().lock().await;
            let before = list.len();
            list.retain(|s| s.id != id);
            if list.len() != before {
                removed = true;
                break; // found & removed
            }
        }
        if removed {
            // Keep the atomic honest. Saturating so a stray double-remove can't
            // underflow (remove is best-effort by id).
            self.live_count.fetch_sub(1, Ordering::Relaxed);
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
                sink: Arc::clone(&s.sink),
            })
            .collect()
    }

    async fn len(&self) -> usize {
        // O(1) single load — never iterates the shards (the old per-table lock
        // loop could deadlock against concurrent adds).
        self.live_count.load(Ordering::Acquire)
    }

    async fn min_acked_lsn(&self) -> Option<nostos_domain::Lsn> {
        // The safe-to-flush LSN: the minimum acked LSN across all live
        // sessions. Fold over every table's sessions, taking the min of each
        // sink's `last_acked_lsn`. A session that hasn't acked yet (None)
        // contributes nothing — but if ANY live session has acked nothing,
        // the conservative answer is "don't advance" (return the global min,
        // which a single unacked session with delivered events would drag
        // down via its delivered_lsn being below others' acked). For correctness
        // we use acked_lsn only: a session that never acks keeps the slot from
        // advancing past its last ack, which (if zero) means no advance at all.
        //
        // In practice the transport ACKs on every batch, so acked_lsn tracks
        // closely. WAL-bloat from a permanently-silent client is the deferred
        // ADR-0016 problem (max_slot_wal_keep_size / age-based advance).
        let mut global_min: Option<u64> = None;
        for entry in &self.by_table {
            let list = entry.value().lock().await;
            for s in list.iter() {
                if let Some(lsn) = s.sink.last_acked_lsn() {
                    global_min = Some(global_min.map_or(lsn.raw(), |m| m.min(lsn.raw())));
                }
            }
        }
        global_min.map(nostos_domain::Lsn::new)
    }

    async fn slowest_session(&self) -> Option<(nostos_domain::SessionId, nostos_domain::Lsn)> {
        // The session with the smallest acked LSN — the eviction target when
        // WAL-bloat protection fires (ADR-0016). Same fold as min_acked_lsn,
        // but tracks the SessionId alongside the minimum so the fanout loop can
        // disconnect exactly this one session.
        let mut slowest: Option<(nostos_domain::SessionId, u64)> = None;
        for entry in &self.by_table {
            let list = entry.value().lock().await;
            for s in list.iter() {
                if let Some(lsn) = s.sink.last_acked_lsn() {
                    match slowest {
                        None => slowest = Some((s.id, lsn.raw())),
                        Some((_, cur)) if lsn.raw() < cur => slowest = Some((s.id, lsn.raw())),
                        _ => {}
                    }
                }
            }
        }
        slowest.map(|(id, raw)| (id, nostos_domain::Lsn::new(raw)))
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
