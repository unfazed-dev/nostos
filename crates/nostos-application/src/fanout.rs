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
//!   return FanOutOutcome { delivered, dropped, faulted, matched }
//! ```
//!
//! Complexity is **O(changed rows × matching sessions)**, not O(all sessions) —
//! the table index prunes the candidate set before filter evaluation. This is
//! what scales past PowerSync's static-bucket model (ADR-0003).

use std::sync::Arc;

use tracing::{trace, warn};

use nostos_domain::{ColumnValue, ReplicationEvent};

use crate::ports::{
    DeliveryDecision, Metrics, PushHint, PushNotifier, PushTables, PushTemplate, ReplicatorStream,
    SessionStore,
};

/// Bounded depth of the push-hint channel (ADR-0037 §4, plan 1.3). Hints are
/// tiny routing tuples consumed by a background drain task (the coalescer,
/// plan 2.4); full ⇒ drop-and-count — never block the fan-out loop.
const PUSH_HINT_CAPACITY: usize = 1024;

/// Below this many matched sessions the walk stays on the caller's task.
/// Spawning costs ~1 µs per chunk and a delivery costs ~0.5 µs (measured with
/// `nostos-fanout-walk`, 2026-09-21), so splitting a small matched set is pure
/// overhead. Only the big fan-outs — the ones the scale ladder cares about —
/// pay for parallelism.
const PARALLEL_FANOUT_MIN: usize = 8_192;

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
    /// Delivery tasks that faulted (panicked or were cancelled) — a server-side
    /// problem, NOT slow-client backpressure. Kept distinct from `dropped` so a
    /// task panic is never mis-attributed as a client drop in the "0% drops"
    /// moat figure. `delivered + dropped + faulted <= matched`.
    pub faulted: u64,
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
            faulted: self.faulted.saturating_add(other.faulted),
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
    /// than the policy's threshold behind the head of the stream. Library
    /// default disabled ([`EvictionPolicy::disabled`]); nostos-server enables it
    /// at 1 GiB — see ADR-0016 / ADR-0043.
    eviction: crate::EvictionPolicy,
    /// Persisted op-log writer (ADR-0025 slice 2). `None` by default — the
    /// benchmark and fake-mode deploys run without one (no behavior change).
    /// A `pg` deploy wires a `PgOpLogWriter` to enable in-window reconnect
    /// replay. See [`crate::ports::OpLogWriter`] for the non-blocking contract.
    op_log: Option<Arc<dyn crate::ports::OpLogWriter>>,
    /// Coalesce the per-event ack-progress scan: recompute the slowest acked
    /// LSN every `ack_progress_every` events instead of every event. `1` (the
    /// default) = every event = the exact ADR-0009 cadence (unchanged). `>1`
    /// trades a bounded slot-advance lag for an N× cut of the O(sessions)
    /// `min_acked_lsn` scan — the documented 10k bottleneck (`min_acked_lsn` +
    /// `slowest_session` fold over every session per event). Safe because acks
    /// are monotonic: a cached min is ≤ the true min, so `advance_progress`
    /// stays conservative (at most `ack_progress_every` events of extra WAL
    /// retention). See [`Self::with_ack_progress_every`].
    ack_progress_every: u32,
    /// Push doorbell (ADR-0037 §4, plan 1.3): the sender half of a bounded
    /// channel fed after the matched-set drain — one [`PushHint`] per matched
    /// offline account. `None` by default: push stays entirely off (the bench
    /// baseline and fake-mode deploys pay nothing). See
    /// [`Self::with_push_notifier`].
    push: Option<tokio::sync::mpsc::Sender<PushHint>>,
    /// Per-table push config (ADR-0037 §1 amendment, plan 2.4): tables that
    /// additionally emit a tenant-wide hint for fully-offline accounts, plus
    /// the tenant column used to target it. Constructor-injected — the
    /// application layer never reads env. Default (empty) = tenant-wide
    /// hints off; the per-account path runs unchanged.
    push_tables: PushTables,
    /// How many tasks split the per-event delivery walk. `1` = the sequential
    /// walk (what shipped before 2026-09-21). Default: `available_parallelism`.
    /// Only consulted above [`PARALLEL_FANOUT_MIN`] matched sessions. See
    /// [`Self::with_fanout_workers`].
    fanout_workers: usize,
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
            op_log: None,
            ack_progress_every: 1,
            push: None,
            push_tables: PushTables::default(),
            fanout_workers: std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get),
        }
    }

    /// Override the delivery-walk width. `1` restores the sequential walk —
    /// which is what the bench passes to get a before number out of the same
    /// binary. Values < 1 clamp to 1.
    #[must_use]
    pub fn with_fanout_workers(mut self, workers: usize) -> Self {
        self.fanout_workers = workers.max(1);
        self
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

    /// Attach a persisted op-log writer (ADR-0025 slice 2). When set, every
    /// fanned-out event is also appended to the durable op-log so a
    /// reconnecting client can replay missed ops in-window. Opt-in: the bench
    /// and fake-mode deploys omit it (no behavior change).
    #[must_use]
    pub fn with_op_log(mut self, writer: Arc<dyn crate::ports::OpLogWriter>) -> Self {
        self.op_log = Some(writer);
        self
    }

    /// Coalesce the per-event ack-progress scan: recompute the slowest acked
    /// LSN (which drives `advance_progress` + WAL-bloat eviction) every `every`
    /// events instead of every event. `1` (the default) is the exact ADR-0009
    /// per-event cadence — pass `>1` to cut the O(sessions) `min_acked_lsn` scan
    /// by that factor, at the cost of at most `every` events of extra WAL
    /// retention (safe: acks are monotonic, so a cached min never overshoots
    /// the true safe-to-flush LSN). Values < 1 clamp to 1.
    ///
    /// This is the lever for the 10k-client stretch goal: at 10k sessions the
    /// per-event full-store fold is the dominant cost (see the bench's own
    /// `min_acked_lsn` note); coalescing to ~16–64 events drops it proportionally.
    #[must_use]
    pub fn with_ack_progress_every(mut self, every: u32) -> Self {
        self.ack_progress_every = every.max(1);
        self
    }

    /// Enable the push doorbell (ADR-0037 §4, plan 1.3). After every
    /// matched-set drain, [`Self::fan_out`] enqueues one [`PushHint`] per
    /// matched OFFLINE account into a bounded channel — the
    /// [`crate::ports::OpLogWriter`] non-blocking contract: `try_send`,
    /// drop-on-full, counted in [`Metrics`] (`push_enqueued`/`push_dropped`),
    /// never blocking or doing rail I/O on the fan-out path.
    ///
    /// The spawned drain task forwards each hint to `notifier` — with the
    /// coalescer (`PushRouter`, plan 2.4) that is debounce + presence
    /// re-check + rail send; with [`crate::ports::NoopNotifier`] (the
    /// composition-root default when no rails are configured) it simply
    /// consumes and discards.
    ///
    /// Must be called inside a tokio runtime (it spawns the drain task). The
    /// task ends when the service (and its sender) drops.
    #[must_use]
    pub fn with_push_notifier(mut self, notifier: Arc<dyn PushNotifier>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PushHint>(PUSH_HINT_CAPACITY);
        tokio::spawn(async move {
            while let Some(hint) = rx.recv().await {
                notifier.notify(hint).await;
            }
        });
        self.push = Some(tx);
        self
    }

    /// Inject the per-table push config (ADR-0037 §1 amendment, plan 2.4):
    /// tables listed here additionally emit ONE tenant-wide hint
    /// (`account_id` empty) per event — even when no session matched — which
    /// the coalescer expands to the tenant's registered tokens whose accounts
    /// are offline at send time. The config is parsed by the composition
    /// root (`NOSTOS_PUSH_TABLES`); this layer never reads env.
    ///
    /// Hot-loop contract: an event whose table is NOT in the config costs
    /// exactly one map lookup/compare here and nothing else.
    #[must_use]
    pub fn with_push_tables(mut self, tables: PushTables) -> Self {
        self.push_tables = tables;
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
    /// Deliveries to matching sinks run as a **sequential loop on the fan-out
    /// task**. Each `EventSink::deliver` is non-blocking (`try_send` on a
    /// bounded channel, ~100ns), so there is nothing to parallelise; the
    /// previous design spawned one tokio task per matched session per event
    /// (10k spawns + 10k joins per event at 10k clients) and was measured
    /// 2026-09-02 at 5–50 events/s with 0 sheds — the spawn storm starved the
    /// 20k writer/reader tasks sharing the runtime. Panics inside a sink are
    /// isolated with `catch_unwind` and counted as `faulted`, preserving the
    /// old JoinSet contract.
    pub async fn fan_out<F>(&self, event: &ReplicationEvent, column_extractor: F) -> FanOutOutcome
    where
        F: Fn(&ReplicationEvent, &str) -> Option<ColumnValue>,
    {
        let stage_start = self.metrics.as_ref().map(|_| std::time::Instant::now());
        let matched: Vec<_> = self
            .store
            .candidates_for(event)
            .await
            .into_iter()
            .filter(|c| c.predicate.matches(|col| column_extractor(event, col)))
            .collect();
        if let (Some(m), Some(t0)) = (&self.metrics, stage_start) {
            m.stage_match_nanos.fetch_add(
                u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let matched_count = matched.len() as u64;

        // Push candidate accounts (ADR-0037 §1): one entry per matched
        // authenticated, non-anonymous session's account, deduped per event.
        // Collected before `matched` is moved into the delivery tasks; the
        // enqueue itself runs after the drain. Entirely skipped — no
        // iteration, no allocation — when push is not wired (bench baseline /
        // fake-mode deploys). The two small String clones per DISTINCT account
        // are the whole cost and are push-gated.
        let mut push_accounts: Vec<(String, String)> = Vec::new();
        if self.push.is_some() {
            for c in &matched {
                if let Some(p) = c.principal.as_ref() {
                    if !p.is_anonymous() && !push_accounts.iter().any(|(a, _)| *a == p.account_id) {
                        push_accounts.push((p.account_id.clone(), p.tenant_id.clone()));
                    }
                }
            }
        }

        // One allocation per event; every session gets a refcount bump, not a
        // clone of the two `String`s + `Bytes` in `RowOp`.
        let shared = Arc::new(event.clone());
        // Wide fan-outs split the walk across tasks; small ones stay here
        // (spawning would cost more than it saves — see PARALLEL_FANOUT_MIN).
        // Every task is JOINED before this returns, so a sink still receives
        // events in LSN order: event N+1's walk cannot start until N's is
        // fully drained. Only the order sessions are visited WITHIN one event
        // changes, and that was never a guarantee.
        let walk_start = self.metrics.as_ref().map(|_| std::time::Instant::now());
        let (delivered, dropped, faulted) =
            if matched.len() >= PARALLEL_FANOUT_MIN && self.fanout_workers > 1 {
                let workers = self.fanout_workers.min(matched.len());
                let chunk = matched.len().div_ceil(workers);
                let mut rest = matched;
                let mut handles = Vec::with_capacity(workers);
                while !rest.is_empty() {
                    let tail = rest.split_off(chunk.min(rest.len()));
                    let part = std::mem::replace(&mut rest, tail);
                    let ev = Arc::clone(&shared);
                    handles.push(tokio::spawn(async move { deliver_chunk(part, ev).await }));
                }
                let mut totals = (0u64, 0u64, 0u64);
                for h in handles {
                    // A JoinError means the task was cancelled (runtime
                    // shutdown) — its deliveries are simply uncounted, which
                    // the `delivered + dropped + faulted <= matched` contract
                    // already allows. Panics never reach here: `deliver_chunk`
                    // catches them per-delivery and counts them as faulted.
                    let (d, dr, f) = h.await.unwrap_or((0, 0, 0));
                    totals = (totals.0 + d, totals.1 + dr, totals.2 + f);
                }
                totals
            } else {
                deliver_chunk(matched, Arc::clone(&shared)).await
            };
        // ADR-0037 §4 (plan 1.3) — push doorbell enqueue, strictly off the hot
        // loop's critical path. Non-blocking contract copied from
        // `OpLogWriter`: try_send into a bounded channel, drop-on-full with a
        // counter, no rail I/O here. Online accounts are suppressed at
        // enqueue time — store membership is presence; a `Dropped`-but-live
        // session is still online (its socket is draining; pushing it would
        // double-signal a client that is catching up).
        //
        // The enqueue-time suppression still has a race window (an account can
        // CONNECT between enqueue and send) — the coalescer (`PushRouter`,
        // plan 2.4) closes it by re-checking `account_online` at SEND time, so
        // at worst a hint is absorbed and then discarded.
        let mut push_enqueued = 0u64;
        let mut push_dropped = 0u64;
        if let Some(tx) = &self.push {
            // ADR-0037 §1 amendment — the ONE per-event config lookup. A miss
            // on a non-configured table is this block's entire cost.
            let template = self.push_tables.get(event.table());
            // Visible-configured tables carry the tuple bytes for in-process
            // `{col}` interpolation at send time; silent doorbells stay
            // content-free (ADR-0037 §2).
            let payload = match template {
                Some(PushTemplate::Visible { .. }) => Some(event.payload_bytes().to_vec()),
                _ => None,
            };
            for (account, tenant) in &push_accounts {
                // ponytail (L4): presence is keyed by bare account id — an
                // account id colliding across tenants suppresses the other
                // tenant's doorbell too. Over-suppression ONLY (a missed
                // push loses nothing; the durable LSN checkpoint is the
                // correctness mechanism), so the ceiling is cosmetic.
                // Upgrade = re-key `account_online` to (tenant, account)
                // here and in the push router's send-time re-check.
                if self.store.account_online(account).await {
                    continue;
                }
                let hint = PushHint {
                    table: event.table().to_owned(),
                    tenant_id: tenant.clone(),
                    account_id: account.clone(),
                    lsn: event.lsn,
                    payload: payload.clone(),
                };
                match tx.try_send(hint) {
                    Ok(()) => push_enqueued += 1,
                    // Full channel (or consumer gone): the doorbell is
                    // best-effort — a missed push loses nothing, the client's
                    // durable LSN checkpoint is the correctness mechanism.
                    // Counted, never blocking.
                    Err(_) => push_dropped += 1,
                }
            }
            // ADR-0037 §1 amendment — fully-offline accounts: a tenant-wide
            // hint (`account_id` empty) for push-configured tables, emitted
            // even when NO session matched (the killed-app case the matched
            // set cannot doorbell). The coalescer expands it to the tenant's
            // registered tokens whose accounts are offline at send time —
            // offline accounts cannot be predicate-filtered, so
            // over-notification is possible and harmless for silent
            // doorbells; visible tables are a conscious operator opt-in.
            if template.is_some() {
                // Tenant targeting: the row's OWN tenant column when
                // configured (read via the caller's extractor — works with
                // zero matched sessions), else the matched sessions' distinct
                // tenants. ponytail: without a tenant column the event
                // carries no tenant to read, so a fully-offline tenant with
                // no matched session gets no hint; upgrade = require the
                // column for tenant-wide hints (or a per-tenant registry
                // scan) when a deploy shows that gap matters.
                let mut tenants: Vec<String> = Vec::new();
                if let Some(col) = &self.push_tables.tenant_column {
                    // INVARIANT: push's tenant column must be the SAME column
                    // the write path force-stamps (write_back.rs's
                    // `stamp_tenant_column`, ADR-0018) — the hint targets the
                    // tenant the WRITER authenticated as, never a
                    // client-attested value. A deploy pointing
                    // NOSTOS_TENANT_COLUMN here at a non-stamped column makes
                    // tenant-wide hints follow untrusted row data.
                    if let Some(ColumnValue::Text(t)) = column_extractor(event, col) {
                        tenants.push(t);
                    }
                }
                if tenants.is_empty() {
                    for (_, tenant) in &push_accounts {
                        if !tenants.iter().any(|t| t == tenant) {
                            tenants.push(tenant.clone());
                        }
                    }
                }
                for tenant in tenants {
                    let hint = PushHint {
                        table: event.table().to_owned(),
                        tenant_id: tenant,
                        account_id: String::new(),
                        lsn: event.lsn,
                        payload: payload.clone(),
                    };
                    match tx.try_send(hint) {
                        Ok(()) => push_enqueued += 1,
                        Err(_) => push_dropped += 1,
                    }
                }
            }
        }
        if let (Some(m), Some(t0)) = (&self.metrics, walk_start) {
            m.stage_deliver_nanos.fetch_add(
                u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let outcome = FanOutOutcome {
            matched: matched_count,
            delivered,
            dropped,
            faulted,
        };
        // Aggregate counters for /metrics (lock-free; no-op when no handle).
        if let Some(m) = &self.metrics {
            use std::sync::atomic::Ordering;
            m.matched.fetch_add(outcome.matched, Ordering::Relaxed);
            m.delivered.fetch_add(outcome.delivered, Ordering::Relaxed);
            m.dropped.fetch_add(outcome.dropped, Ordering::Relaxed);
            m.faulted.fetch_add(outcome.faulted, Ordering::Relaxed);
            if push_enqueued != 0 || push_dropped != 0 {
                m.push_enqueued.fetch_add(push_enqueued, Ordering::Relaxed);
                m.push_dropped.fetch_add(push_dropped, Ordering::Relaxed);
            }
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
        // Coalesced ack-progress: the slowest acked LSN drives both
        // `advance_progress` and WAL-bloat eviction, and they share one scan.
        // Recompute it every `ack_progress_every` events (1 = every event, the
        // exact ADR-0009 cadence). Between recomputes we reuse the last value
        // — safe because acks are monotonic, so a cached min ≤ true min and the
        // slot never advances past unconfirmed data (at most N events of extra
        // WAL retention). `since` starts at the threshold so the FIRST event
        // primes the cache rather than advancing on a stale `None`.
        let every = self.ack_progress_every;
        let mut since = every;
        let mut slowest_acked: Option<nostos_domain::Lsn> = None;
        while let Some(event) = replicator.next_event().await {
            // Op-log (ADR-0025 slice 2): record the event durably for in-window
            // reconnect replay. Non-blocking (the impl enqueues to a bounded
            // buffer; a background task flushes). Fire-and-forget — recorded
            // regardless of whether live fan-out later drops it.
            if let Some(w) = &self.op_log {
                w.append(&event).await;
            }
            total = total.merged(self.fan_out(&event, &column_extractor).await);
            // Ack-driven progress: advance the slot only as far as the slowest
            // live client has confirmed. Coalesced — see the `since`/`every`
            // comment above. None = no session has acked (or not yet recomputed
            // this window) → don't advance (WAL retained; no data loss). The
            // replicator no-ops if it has no real slot (FakeReplicator).
            since = since.saturating_add(1);
            if since >= every {
                since = 0;
                let t0 = self.metrics.as_ref().map(|_| std::time::Instant::now());
                slowest_acked = self.store.min_acked_lsn().await;
                if let (Some(m), Some(t0)) = (&self.metrics, t0) {
                    use std::sync::atomic::Ordering;
                    m.stage_ack_scan_nanos.fetch_add(
                        u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                }
            }
            if let Some(m) = &self.metrics {
                m.stage_events
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(safe) = slowest_acked {
                replicator.advance_progress(safe).await;
            }
            // WAL-bloat protection (ADR-0016): if the slowest client has fallen
            // further than the policy's threshold behind this event (the head of
            // the stream), disconnect it. It reconnects + re-syncs from a fresh
            // checkpoint — trading a controlled replay window for source-DB
            // safety. This removes the *session* only — it never drops the
            // replication slot (ADR-0043).
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

/// Deliver `chunk` sequentially, returning `(delivered, dropped, faulted)`.
///
/// Panics inside a sink are isolated with `catch_unwind` and counted as
/// `faulted`, preserving the JoinSet contract the per-session-task design had.
async fn deliver_chunk(
    chunk: Vec<crate::ports::SessionCandidate>,
    event: Arc<ReplicationEvent>,
) -> (u64, u64, u64) {
    let mut delivered = 0u64;
    let mut dropped = 0u64;
    let mut faulted = 0u64;
    for c in chunk {
        use futures_util::FutureExt as _;
        let res = std::panic::AssertUnwindSafe(c.sink.deliver(Arc::clone(&event)))
            .catch_unwind()
            .await;
        match res {
            Ok(DeliveryDecision::Delivered) => delivered += 1,
            // Slow-client backpressure: the sink's bounded buffer was full.
            Ok(DeliveryDecision::Dropped) => dropped += 1,
            // A delivery panicked. This is a server-side problem, NOT
            // slow-client backpressure — count it separately from `dropped` so
            // a task panic is never mis-attributed as a client drop in the
            // "0% drops" moat figure, and log it (a panic here should be
            // visible, not silent).
            Err(payload) => {
                faulted += 1;
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                warn!(
                    error = %msg,
                    "delivery panicked; counted as faulted, not dropped"
                );
            }
        }
    }
    (delivered, dropped, faulted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{EventSink, SessionCandidate, SessionStore};
    use async_trait::async_trait;
    use bytes::Bytes;
    use nostos_domain::{Lsn, Predicate, Principal, RowOp, SessionId, SyncSession};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    // ---- test doubles ----

    /// A sink that records every delivered event and never drops.
    struct RecordingSink {
        events: Arc<Mutex<Vec<ReplicationEvent>>>,
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn deliver(&self, event: Arc<ReplicationEvent>) -> DeliveryDecision {
            self.events.lock().unwrap().push((*event).clone());
            DeliveryDecision::Delivered
        }
    }

    /// An in-memory store keyed by table — the simplest correct SessionStore.
    struct TableStore {
        by_table: Mutex<HashMap<String, Vec<SessionCandidate>>>,
        /// Accounts the push path must treat as ONLINE (suppress). Absent ⇒
        /// offline ⇒ push — the port's default failure direction.
        online: Mutex<HashSet<String>>,
    }

    impl TableStore {
        fn set_online(&self, account: &str) {
            self.online.lock().unwrap().insert(account.to_string());
        }
    }

    #[async_trait]
    impl SessionStore for TableStore {
        async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>) {
            let table = session.predicate.table.clone();
            let cand = SessionCandidate {
                id: session.id,
                predicate: session.predicate,
                principal: session.principal,
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
            _per_principal_cap: u64,
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
                principal: session.principal,
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
        async fn account_online(&self, account_id: &str) -> bool {
            self.online.lock().unwrap().contains(account_id)
        }
    }

    fn make_store() -> Arc<TableStore> {
        Arc::new(TableStore {
            by_table: Mutex::new(HashMap::new()),
            online: Mutex::new(HashSet::new()),
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

    /// The parallel walk must be indistinguishable from the sequential one:
    /// every matched session gets the event EXACTLY once, the counts agree,
    /// and a panicking sink among thousands is still `faulted`, not `dropped`.
    /// Sized just over `PARALLEL_FANOUT_MIN` so the chunked path is the one
    /// under test (below it, `fan_out` stays sequential by design).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_walk_matches_the_sequential_walk() {
        const N: usize = PARALLEL_FANOUT_MIN + 7; // +7 ⇒ a ragged last chunk

        for workers in [1usize, 4] {
            let store = make_store();
            let counters: Vec<Arc<std::sync::atomic::AtomicU64>> = (0..N)
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0)))
                .collect();
            for c in &counters {
                store
                    .add(
                        SyncSession::new(Predicate::all("tasks")),
                        Arc::new(CountingSink(Arc::clone(c))),
                    )
                    .await;
            }
            // One panicking sink in the crowd — it must land in `faulted`.
            store
                .add(
                    SyncSession::new(Predicate::all("tasks")),
                    Arc::new(PanickingSink),
                )
                .await;

            let svc = FanOutService::new(store).with_fanout_workers(workers);
            let outcome = svc.fan_out(&insert_event("tasks"), extract_org).await;

            assert_eq!(outcome.matched, N as u64 + 1, "workers={workers}");
            assert_eq!(outcome.delivered, N as u64, "workers={workers}");
            assert_eq!(outcome.faulted, 1, "workers={workers}");
            assert_eq!(outcome.dropped, 0, "workers={workers}");
            // Exactly once each — no chunk skipped, no session visited twice.
            assert!(
                counters
                    .iter()
                    .all(|c| c.load(std::sync::atomic::Ordering::Relaxed) == 1),
                "every session delivered exactly once (workers={workers})"
            );
        }
    }

    /// Per-session counter — cheaper than `RecordingSink` at 8k sessions.
    struct CountingSink(Arc<std::sync::atomic::AtomicU64>);

    #[async_trait]
    impl EventSink for CountingSink {
        async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            DeliveryDecision::Delivered
        }
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

    /// A sink whose `deliver` panics — models a faulting delivery task (the
    /// `Err(JoinError)` arm of the fan-out join loop).
    struct PanickingSink;

    #[async_trait]
    impl EventSink for PanickingSink {
        async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
            panic!("simulated delivery fault");
        }
    }

    #[tokio::test]
    async fn faulting_delivery_task_is_counted_as_faulted_not_dropped() {
        let store = make_store();
        store
            .add(
                SyncSession::new(Predicate::all("tasks")),
                Arc::new(PanickingSink),
            )
            .await;

        let svc = FanOutService::new(store);
        let outcome = svc.fan_out(&insert_event("tasks"), extract_org).await;

        // The panicking delivery task surfaces as a JoinError → `faulted`, NOT
        // `dropped` (it is a server fault, not slow-client backpressure).
        assert_eq!(outcome.matched, 1);
        assert_eq!(outcome.delivered, 0);
        assert_eq!(outcome.dropped, 0);
        assert_eq!(outcome.faulted, 1);
    }

    #[tokio::test]
    async fn outcome_merges_accumulate() {
        let a = FanOutOutcome {
            matched: 5,
            delivered: 4,
            dropped: 1,
            faulted: 2,
        };
        let b = FanOutOutcome {
            matched: 3,
            delivered: 3,
            dropped: 0,
            faulted: 1,
        };
        let m = a.merged(b);
        assert_eq!(
            m,
            FanOutOutcome {
                matched: 8,
                delivered: 7,
                dropped: 1,
                faulted: 3
            }
        );
    }

    // ---- push doorbell enqueue (ADR-0037 §4, plan 1.3) ----

    /// Records every hint it is asked to send — the test double for
    /// [`PushNotifier`].
    #[derive(Default)]
    struct RecordingNotifier {
        hints: Mutex<Vec<PushHint>>,
    }

    #[async_trait]
    impl PushNotifier for RecordingNotifier {
        async fn notify(&self, hint: PushHint) {
            self.hints.lock().unwrap().push(hint);
        }
    }

    /// A rail whose send never completes — pins the channel's consumer so the
    /// bounded buffer provably fills.
    struct StalledNotifier {
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl PushNotifier for StalledNotifier {
        async fn notify(&self, _hint: PushHint) {
            self.gate.notified().await;
        }
    }

    /// The `with_push_notifier` drain task forwards asynchronously — poll
    /// (with a generous deadline) until `f` holds, then let the caller's
    /// asserts fail with real values if it never did.
    async fn soon(mut f: impl FnMut() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    fn authenticated_session(table: &str, account: &str) -> SyncSession {
        SyncSession::new_authenticated(
            Predicate::all(table),
            Principal::new(account, "tenant-acme"),
        )
    }

    #[tokio::test]
    async fn push_hint_enqueued_for_offline_matched_account() {
        let store = make_store();
        // Two sessions of ONE account (multi-device) + one anonymous session:
        // the burst must collapse to a single hint, and the anonymous session
        // must produce none (no account to doorbell).
        store
            .add(
                authenticated_session("tasks", "u1"),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;
        store
            .add(
                authenticated_session("tasks", "u1"),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;
        store
            .add(
                SyncSession::new(Predicate::all("tasks")),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;

        let recorder = Arc::new(RecordingNotifier::default());
        let metrics = Arc::new(Metrics::new());
        let svc = FanOutService::new(store)
            .with_metrics(Arc::clone(&metrics))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        let ev = ReplicationEvent::new(
            Lsn::new(42),
            RowOp::Insert {
                table: "tasks".into(),
                pk: "1".into(),
                payload: Bytes::from_static(b"x"),
            },
        );
        let outcome = svc.fan_out(&ev, extract_org).await;
        assert_eq!(outcome.delivered, 3);

        soon(|| recorder.hints.lock().unwrap().len() == 1).await;
        let hints = recorder.hints.lock().unwrap().clone();
        assert_eq!(hints.len(), 1, "one hint per account, not per session");
        assert_eq!(hints[0].table, "tasks");
        assert_eq!(hints[0].account_id, "u1");
        assert_eq!(hints[0].tenant_id, "tenant-acme");
        assert_eq!(hints[0].lsn, Lsn::new(42));
        let snap = metrics.snapshot();
        assert_eq!(snap.push_enqueued, 1);
        assert_eq!(snap.push_dropped, 0);
    }

    #[tokio::test]
    async fn online_account_is_not_enqueued() {
        let store = make_store();
        store
            .add(
                authenticated_session("tasks", "u1"),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;
        store.set_online("u1");

        let recorder = Arc::new(RecordingNotifier::default());
        let metrics = Arc::new(Metrics::new());
        let svc = FanOutService::new(store)
            .with_metrics(Arc::clone(&metrics))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        let outcome = svc.fan_out(&insert_event("tasks"), extract_org).await;
        assert_eq!(outcome.delivered, 1);

        // Nothing was enqueued, so nothing can arrive; give the drain task a
        // grace window before asserting emptiness.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(recorder.hints.lock().unwrap().is_empty());
        let snap = metrics.snapshot();
        assert_eq!(snap.push_enqueued, 0);
        assert_eq!(snap.push_dropped, 0);
    }

    #[tokio::test]
    async fn full_push_channel_drops_and_counts_without_stalling_fanout() {
        let store = make_store();
        // Offline (default) ⇒ a hint per event; the stalled consumer pins the
        // channel so it fills after PUSH_HINT_CAPACITY (+1 in-flight) hints.
        store
            .add(
                authenticated_session("tasks", "u1"),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;

        let metrics = Arc::new(Metrics::new());
        let svc = FanOutService::new(store)
            .with_metrics(Arc::clone(&metrics))
            .with_push_notifier(Arc::new(StalledNotifier {
                gate: Arc::new(tokio::sync::Notify::new()),
            }));

        let n = (PUSH_HINT_CAPACITY + 64) as u64;
        let mut total = FanOutOutcome::default();
        for _ in 0..n {
            total = total.merged(svc.fan_out(&insert_event("tasks"), extract_org).await);
        }

        // Every event was still delivered — enqueue drops never touch the
        // fan-out path (the non-blocking contract).
        assert_eq!(total.matched, n);
        assert_eq!(total.delivered, n);
        let snap = metrics.snapshot();
        // Each try_send either landed or was dropped — the counts partition
        // the hints exactly, regardless of drain timing.
        assert_eq!(snap.push_enqueued + snap.push_dropped, n);
        assert!(
            snap.push_dropped > 0,
            "channel (capacity {PUSH_HINT_CAPACITY}) must have filled"
        );
    }

    // ---- tenant-wide hints (ADR-0037 §1 amendment, plan 2.4) ----

    fn push_tables_cfg(
        tenant_column: Option<&str>,
        tables: Vec<(&str, crate::ports::PushTemplate)>,
    ) -> crate::ports::PushTables {
        crate::ports::PushTables {
            tenant_column: tenant_column.map(str::to_string),
            tables: tables
                .into_iter()
                .map(|(t, tpl)| (t.to_string(), tpl))
                .collect(),
        }
    }

    /// The killed-app case: zero matched sessions, but the table is
    /// push-configured and the event's own tenant column names the tenant —
    /// exactly one tenant-wide hint (`account_id` empty) must fire.
    #[tokio::test]
    async fn tenant_wide_hint_emitted_for_configured_table_with_no_sessions() {
        let store = make_store();
        let recorder = Arc::new(RecordingNotifier::default());
        let metrics = Arc::new(Metrics::new());
        let svc = FanOutService::new(store)
            .with_metrics(Arc::clone(&metrics))
            .with_push_tables(push_tables_cfg(
                Some("org_id"),
                vec![("tasks", crate::ports::PushTemplate::Silent)],
            ))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        // extract_org yields Text("acme") for org_id — the payload-tenant path.
        let outcome = svc.fan_out(&insert_event("tasks"), extract_org).await;
        assert_eq!(outcome.matched, 0, "fixture: nobody is subscribed");

        soon(|| recorder.hints.lock().unwrap().len() == 1).await;
        let hints = recorder.hints.lock().unwrap().clone();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].table, "tasks");
        assert_eq!(hints[0].tenant_id, "acme");
        assert!(hints[0].account_id.is_empty(), "tenant-wide marker");
        assert!(
            hints[0].payload.is_none(),
            "silent template carries no row data"
        );
        assert_eq!(metrics.snapshot().push_enqueued, 1);
    }

    /// A non-configured table must not emit a tenant-wide hint — one lookup,
    /// nothing else.
    #[tokio::test]
    async fn unconfigured_table_emits_no_tenant_hint() {
        let store = make_store();
        let recorder = Arc::new(RecordingNotifier::default());
        let svc = FanOutService::new(store)
            .with_push_tables(push_tables_cfg(
                Some("org_id"),
                vec![("tasks", crate::ports::PushTemplate::Silent)],
            ))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        let _ = svc.fan_out(&insert_event("notes"), extract_org).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(recorder.hints.lock().unwrap().is_empty());
    }

    /// Visible-configured tables attach the event's tuple bytes to the hint
    /// (in-process interpolation input — ADR-0037 §2's visible opt-in).
    #[tokio::test]
    async fn visible_table_hints_carry_payload() {
        let store = make_store();
        let recorder = Arc::new(RecordingNotifier::default());
        let svc = FanOutService::new(store)
            .with_push_tables(push_tables_cfg(
                Some("org_id"),
                vec![(
                    "tasks",
                    crate::ports::PushTemplate::Visible {
                        title: "Changed".into(),
                        body: "{label} updated".into(),
                        category: None,
                    },
                )],
            ))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        let _ = svc.fan_out(&insert_event("tasks"), extract_org).await;
        soon(|| recorder.hints.lock().unwrap().len() == 1).await;
        let hints = recorder.hints.lock().unwrap().clone();
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].payload.is_some(),
            "a visible-configured table must carry the tuple bytes"
        );
    }

    /// No tenant column configured ⇒ fall back to the matched sessions'
    /// tenants: the tenant-wide hint still fires alongside the per-account
    /// hint (the coalescer debounces both into one send per account).
    #[tokio::test]
    async fn tenant_hint_falls_back_to_matched_tenants_without_tenant_column() {
        let store = make_store();
        store
            .add(
                authenticated_session("tasks", "u1"),
                Arc::new(RecordingSink {
                    events: Arc::new(Mutex::new(vec![])),
                }),
            )
            .await;
        let recorder = Arc::new(RecordingNotifier::default());
        let svc = FanOutService::new(store)
            .with_push_tables(push_tables_cfg(
                None,
                vec![("tasks", crate::ports::PushTemplate::Silent)],
            ))
            .with_push_notifier(Arc::clone(&recorder) as Arc<dyn PushNotifier>);

        let _ = svc.fan_out(&insert_event("tasks"), extract_org).await;
        soon(|| recorder.hints.lock().unwrap().len() == 2).await;
        let hints = recorder.hints.lock().unwrap().clone();
        assert_eq!(hints.len(), 2, "one per-account + one tenant-wide");
        assert!(hints.iter().any(|h| h.account_id == "u1"));
        assert!(hints
            .iter()
            .any(|h| h.account_id.is_empty() && h.tenant_id == "tenant-acme"));
    }

    /// Coalescing the ack-progress scan must never advance the slot past the
    /// true safe-to-flush LSN. `with_ack_progress_every(N)` reuses a cached
    /// minimum between recomputes, and the whole safety argument is that acks
    /// are monotonic so a cached min is <= the true min. This pins that: the
    /// slot lags, and it lags CONSERVATIVELY — never once above the truth.
    ///
    /// Also pins the cost model the 100k measurement turns on: the O(sessions)
    /// scan runs once per N events, not once per event
    /// (`benches/results/RESULTS.md`, 2026-09-21 — 2.23x at 100k sessions).
    #[tokio::test]
    async fn coalesced_ack_progress_lags_but_never_overshoots() {
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Its `min_acked_lsn` rises by 1 each event, mimicking clients that
        /// ack every event, and counts how often the fold actually ran.
        struct AckingStore {
            true_min: Arc<AtomicU64>,
            scans: Arc<AtomicU64>,
        }
        #[async_trait]
        impl SessionStore for AckingStore {
            async fn add(&self, _s: SyncSession, _k: Arc<dyn EventSink>) {}
            async fn try_add_below_cap(
                &self,
                s: SyncSession,
                _k: Arc<dyn EventSink>,
                _cap: u64,
                _pcap: u64,
            ) -> Result<SessionId, crate::ports::StoreRejection> {
                Ok(s.id)
            }
            async fn remove(&self, _id: SessionId) {}
            async fn candidates_for(&self, _e: &ReplicationEvent) -> Vec<SessionCandidate> {
                Vec::new()
            }
            async fn len(&self) -> usize {
                0
            }
            async fn min_acked_lsn(&self) -> Option<Lsn> {
                self.scans.fetch_add(1, Ordering::Relaxed);
                Some(Lsn::new(self.true_min.load(Ordering::Relaxed)))
            }
        }

        /// Emits `n` events and records every LSN the loop tries to flush,
        /// paired with the true min at that instant.
        struct RecordingReplicator {
            left: u64,
            lsn: u64,
            true_min: Arc<AtomicU64>,
            seen: Arc<Mutex<Vec<(u64, u64)>>>,
        }
        #[async_trait]
        impl ReplicatorStream for RecordingReplicator {
            async fn next_event(&mut self) -> Option<ReplicationEvent> {
                if self.left == 0 {
                    return None;
                }
                self.left -= 1;
                self.lsn += 1;
                // The client acks this event before the loop asks for the min.
                self.true_min.store(self.lsn, Ordering::Relaxed);
                Some(ReplicationEvent::new(
                    Lsn::new(self.lsn),
                    RowOp::Insert {
                        table: "tasks".into(),
                        pk: self.lsn.to_string(),
                        payload: Bytes::from_static(b"{}"),
                    },
                ))
            }
            async fn advance_progress(&mut self, lsn: Lsn) {
                self.seen
                    .lock()
                    .unwrap()
                    .push((lsn.raw(), self.true_min.load(Ordering::Relaxed)));
            }
        }

        const EVENTS: u64 = 64;
        const EVERY: u32 = 8;

        let true_min = Arc::new(AtomicU64::new(0));
        let scans = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(AckingStore {
            true_min: Arc::clone(&true_min),
            scans: Arc::clone(&scans),
        });
        let mut repl = RecordingReplicator {
            left: EVENTS,
            lsn: 0,
            true_min: Arc::clone(&true_min),
            seen: Arc::clone(&seen),
        };

        let svc = FanOutService::new(store as Arc<dyn SessionStore>).with_ack_progress_every(EVERY);
        svc.run(&mut repl, extract_org).await;

        // The scan is coalesced, not skipped.
        assert_eq!(
            scans.load(Ordering::Relaxed),
            EVENTS / u64::from(EVERY),
            "the O(sessions) fold must run once per {EVERY} events"
        );

        // The safety property: every flushed LSN was <= the truth at the time.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len() as u64, EVENTS, "every event attempts a flush");
        for (flushed, truth) in seen.iter() {
            assert!(
                flushed <= truth,
                "advanced the slot to {flushed} while the slowest client had only acked {truth}"
            );
        }
        // And it really does lag — otherwise this test would pass trivially
        // against a per-event scan and prove nothing about coalescing.
        assert!(
            seen.iter().any(|(flushed, truth)| flushed < truth),
            "coalescing must actually reuse a stale min somewhere"
        );
    }
}
