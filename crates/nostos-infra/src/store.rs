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
/// O(1) `len()` and atomic cap enforcement (see the module doc). `by_account`
/// counts live sessions per `principal.account_id` — the presence index the
/// push router reads (ADR-0037 §4).
pub struct InMemorySessionStore {
    by_table: DashMap<String, Arc<Mutex<Vec<StoredSession>>>>,
    live_count: AtomicUsize,
    by_account: DashMap<String, usize>,
}

struct StoredSession {
    id: SessionId,
    predicate: nostos_domain::Predicate,
    /// The session's authenticated identity, retained for the push path
    /// (ADR-0037) — presence + per-account hint routing.
    principal: Option<nostos_domain::Principal>,
    sink: Arc<dyn EventSink>,
}

impl StoredSession {
    /// The presence-index key: the principal's `account_id` when the session
    /// carries a real (non-anonymous) identity. Anonymous principals have an
    /// empty `account_id` and are never indexed — they belong to no account.
    fn account(&self) -> Option<&str> {
        self.principal
            .as_ref()
            .filter(|p| !p.account_id.is_empty())
            .map(|p| p.account_id.as_str())
    }
}

impl InMemorySessionStore {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_table: DashMap::new(),
            live_count: AtomicUsize::new(0),
            by_account: DashMap::new(),
        }
    }

    /// Shared insert tail of `add`/`try_add_below_cap`: table-list push +
    /// presence-index bump, so both registration chutes keep `by_account`
    /// honest identically.
    async fn insert_indexed(&self, stored: StoredSession, table: String) {
        if let Some(account) = stored.account() {
            *self.by_account.entry(account.to_owned()).or_insert(0) += 1;
        }
        let entry = self
            .by_table
            .entry(table)
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())));
        entry.lock().await.push(stored);
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
            principal: session.principal,
            sink,
        };
        self.insert_indexed(stored, table).await;
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
            principal: session.principal,
            sink,
        };
        self.insert_indexed(stored, table).await;
        Ok(id)
    }

    async fn remove(&self, id: SessionId) {
        // A session lives in exactly one table's list; scan all to be safe
        // (cheap — sessions per table is small after pruning). Capture the
        // removed session so the presence index decrements the right account.
        let mut removed: Option<StoredSession> = None;
        for entry in &self.by_table {
            let mut list = entry.value().lock().await;
            let Some(pos) = list.iter().position(|s| s.id == id) else {
                continue;
            };
            removed = Some(list.remove(pos));
            break;
        }
        if let Some(stored) = removed {
            // Every removal path funnels through here — transport disconnect
            // AND WAL-bloat eviction — so an evicted session's zombie socket
            // (sink Arc still alive in the transport) counts as offline
            // (ADR-0037 §4: presence is store membership, not sink liveness).
            if let Some(account) = stored.account() {
                if let Some(mut count) = self.by_account.get_mut(account) {
                    *count = count.saturating_sub(1);
                }
            }
            // Keep the atomic honest. Only decremented when a session was
            // actually removed, so a stray double-remove can't underflow.
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
                principal: s.principal.clone(),
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

    /// Presence from the `by_account` index (ADR-0037 §4). Entries linger at
    /// zero after an account's last session leaves — same shape as
    /// `by_table`'s empty vecs; bounded by distinct accounts, so no
    /// remove-on-zero churn (and its race) is worth it.
    async fn account_online(&self, account_id: &str) -> bool {
        self.by_account.get(account_id).is_some_and(|c| *c > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_application::ports::{DeliveryDecision, EventSink, ReplicatorStream};
    use nostos_application::{EvictionPolicy, FanOutService};
    use nostos_domain::{Lsn, Predicate, Principal, ReplicationEvent, RowOp};

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

    fn auth_session(table: &str, account: &str) -> SyncSession {
        SyncSession::new_authenticated(Predicate::all(table), Principal::new(account, "org-acme"))
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

    // ---- presence index (ADR-0037 §4: presence is store membership) ----

    #[tokio::test]
    async fn presence_tracks_register_and_unregister() {
        let store = InMemorySessionStore::new();
        let s = auth_session("tasks", "u1");
        let id = s.id;
        assert!(!store.account_online("u1").await, "nobody registered yet");

        store.add(s, Arc::new(NoopSink)).await;
        assert!(store.account_online("u1").await);

        // An anonymous session belongs to no account — it must never register
        // presence, not even for the empty account id.
        store
            .add(
                SyncSession::new(Predicate::all("tasks")),
                Arc::new(NoopSink),
            )
            .await;
        assert!(!store.account_online("").await);

        store.remove(id).await;
        assert!(!store.account_online("u1").await, "unregister ⇒ offline");
    }

    #[tokio::test]
    async fn presence_stays_online_until_last_session_leaves() {
        let store = InMemorySessionStore::new();
        // Two devices sharing one account, on different tables.
        let a = auth_session("tasks", "u1");
        let b = auth_session("notes", "u1");
        let (id_a, id_b) = (a.id, b.id);
        store.add(a, Arc::new(NoopSink)).await;
        store.add(b, Arc::new(NoopSink)).await;

        store.remove(id_a).await;
        assert!(
            store.account_online("u1").await,
            "the second device keeps the account online"
        );

        store.remove(id_b).await;
        assert!(!store.account_online("u1").await);
    }

    /// The WAL-bloat eviction chute (application `fanout.rs` `run`) removes
    /// through the same `SessionStore::remove` the transport's disconnect
    /// uses, so the presence index flips offline there too. The sink Arc is
    /// still alive in this test when the assert runs — the "zombie socket" —
    /// and must NOT keep the account online (ADR-0037 §4).
    struct AckedSink {
        acked: Lsn,
    }

    #[async_trait]
    impl EventSink for AckedSink {
        async fn deliver(&self, _e: ReplicationEvent) -> DeliveryDecision {
            DeliveryDecision::Delivered
        }
        fn last_acked_lsn(&self) -> Option<Lsn> {
            Some(self.acked)
        }
    }

    struct OneShotReplicator {
        event: Option<ReplicationEvent>,
    }

    #[async_trait]
    impl ReplicatorStream for OneShotReplicator {
        async fn next_event(&mut self) -> Option<ReplicationEvent> {
            self.event.take()
        }
    }

    #[tokio::test]
    async fn evicted_session_counts_as_offline_even_with_zombie_sink() {
        let store = Arc::new(InMemorySessionStore::new());
        let session = auth_session("tasks", "u1");
        // Keep a handle to the sink past the eviction — the transport still
        // holds the socket object when eviction fires. This is the zombie.
        let zombie_socket: Arc<dyn EventSink> = Arc::new(AckedSink { acked: Lsn::new(1) });
        store.add(session, Arc::clone(&zombie_socket)).await;
        assert!(store.account_online("u1").await);

        // Head 10_000 vs acked 1 → gap 9_999 > 100 ⇒ `run` evicts the slowest
        // (only) session mid-loop.
        let head = ReplicationEvent::new(
            Lsn::new(10_000),
            RowOp::Insert {
                table: "tasks".into(),
                pk: "1".into(),
                payload: Bytes::from_static(b"x"),
            },
        );
        let mut repl = OneShotReplicator { event: Some(head) };
        let svc = FanOutService::new(Arc::clone(&store) as Arc<dyn SessionStore>)
            .with_eviction(EvictionPolicy::new(100));
        let _ = svc.run(&mut repl, |_e, _col| None).await;

        assert_eq!(store.len().await, 0, "eviction removed the session");
        assert!(
            !store.account_online("u1").await,
            "store membership is presence — an evicted session's zombie socket is offline"
        );
    }

    /// `DeliveryDecision::Dropped` is slow-client backpressure, NOT presence
    /// (ADR-0037 §4: "never `Dropped` — a slow-online client must not be
    /// double-signalled"). A registered-but-dropping session stays online.
    struct FullSink;

    #[async_trait]
    impl EventSink for FullSink {
        async fn deliver(&self, _e: ReplicationEvent) -> DeliveryDecision {
            DeliveryDecision::Dropped
        }
    }

    #[tokio::test]
    async fn dropped_but_registered_session_is_still_online() {
        let store = Arc::new(InMemorySessionStore::new());
        let session = auth_session("tasks", "u1");
        let id = session.id;
        store.add(session, Arc::new(FullSink)).await;

        let svc = FanOutService::new(Arc::clone(&store) as Arc<dyn SessionStore>);
        let outcome = svc.fan_out(&event_on("tasks"), |_e, _col| None).await;
        assert_eq!(outcome.matched, 1);
        assert_eq!(outcome.dropped, 1);

        assert!(
            store.account_online("u1").await,
            "`Dropped` is backpressure, not offline-presence"
        );

        store.remove(id).await;
        assert!(!store.account_online("u1").await);
    }
}
