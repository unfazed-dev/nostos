//! Debounce coalescer (ADR-0038 §2, plan task 1.6) — the daemon's send
//! pipeline core. Ceilings + admission gate per the 2026-08-17 security
//! audit closeout (plan task 4.1, finding 2).
//!
//! Semantics mirror the embedded PushRouter (nostos-infra push/router.rs):
//! a bounded tokio mpsc channel (capacity [QUEUE_CAPACITY], fed by
//! try_send off the request path — full channel = 503, never a blocked
//! handler) drains into a per-(tenant, token) debounce map. The FIRST
//! queued send for a key fixes the flush deadline (now + debounce);
//! later sends within the window REPLACE the pending payload and join the
//! losers list — a steady stream must not push the flush out forever
//! (debounce-vs-throttle). At deadline the winning payload dispatches
//! through the rail seam with the collapse key (rail-native supersede).
//!
//! RECEIPT SEMANTICS (pinned with the brief): every accepted push_id yields
//! exactly one receipt — the winner carries the rail outcome (Fatal's
//! diagnostic as detail); each coalesced-away push_id shares the winning
//! outcome with detail "coalesced:<winning push_id>" and echoes ITS OWN
//! request metadata (the push-LSN correlation channel).
//!
//! CEILINGS (audit finding 2): the pending map admits at most
//! [CoalescerLimits::pending_keys_max] distinct keys — a NEW key beyond
//! that is refused at the route (429) via the [PendingGate] shared between
//! handler and task; the losers list per key is capped at
//! [CoalescerLimits::losers_max] — the oldest loser beyond the cap is
//! evicted from the list at absorption time and receipted with the rest at
//! flush (sharing the window's real outcome — the daemon never fabricates
//! an outcome it has not observed), so every push_id still yields exactly
//! one receipt.
//!
//! TRANSIENT RETRY (the former v1 ponytail, closed 2026-08-27 per the arxa
//! integration plan B2): a transient rail outcome (429/5xx/network) re-
//! debounces the whole window up to RetryPolicy::max_attempts total sends —
//! exactly the embedded router's shape (attempts counter on the pending
//! entry + scheduled re-flush). The receipt stays the source of truth:
//! receipts are written only at the terminal attempt, so every push_id
//! still yields exactly one. Shutdown drains terminal (no retry at
//! process exit); an exhausted transient receipts outcome=transient with a
//! retries-exhausted detail. A doorbell lost past the budget is mitigated
//! by the durable LSN checkpoint reconciling on the next sync — no data
//! loss, just a missed wake-up.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostos_infra::push::{PushPayload, RailOutcome};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::rail::{default_collapse_key, Rails};
use crate::store::{NewReceipt, Outcome, Store};

/// Bounded channel capacity between the send route and the coalescer task.
/// Full channel => the handler 503s; the request path never blocks.
pub const QUEUE_CAPACITY: usize = 1024;

/// Ceilings on the coalescer's in-memory state (audit finding 2, plan task
/// 4.1). Injectable so tests run with tiny values; production defaults in
/// [Default].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalescerLimits {
    /// Max distinct (tenant, token) keys with an open debounce window. A
    /// send for a NEW key while the map is full is refused at the route
    /// with 429 (sends joining an already-open key always pass).
    pub pending_keys_max: usize,
    /// Max coalesced-away jobs retained per key. Beyond the cap the oldest
    /// loser is evicted to the flush-time receipt batch.
    pub losers_max: usize,
}

/// Transient-outcome retry budget (B2 of the arxa integration plan; the
/// embedded router MAX_ATTEMPTS=2 / RETRY_DELAY=500ms shape, env-exposed
/// here per the daemon knob convention). `max_attempts` is TOTAL sends
/// (1 initial + max_attempts-1 deferred); the delay is the re-debounce
/// wait before each deferred send.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total sends per window (1 initial + deferred retries). 1 disables
    /// retry (v1 behavior). 0 clamps to 1 at use sites — a send always
    /// happens at least once.
    pub max_attempts: u8,
    /// Wait before each deferred retry.
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            delay: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    /// The effective total-send budget, floored at 1 (a configured 0 means
    /// "no retries", never "no send").
    #[must_use]
    pub fn effective_max_attempts(&self) -> u8 {
        self.max_attempts.max(1)
    }
}

impl Default for CoalescerLimits {
    /// ponytail: 10k open keys / 64 losers per key are daemon-shape guesses
    /// (10k targets inside one 2s window, 65 pushes to one token inside one
    /// window), not measurements — the audit pinned them as safe ceilings.
    /// Upgrade path: derive both from observed window occupancy once
    /// operators run real fleets; the knobs are env-exposed
    /// (NOSTOS_PUSHD_PENDING_KEYS_MAX / NOSTOS_PUSHD_LOSERS_MAX) precisely so
    /// tuning needs no code change.
    fn default() -> Self {
        Self {
            pending_keys_max: 10_000,
            losers_max: 64,
        }
    }
}

/// Admission gate on open debounce windows, shared between the send route
/// (admit) and the coalescer task (release on flush). Lock discipline:
/// sync Mutex, no await while held.
///
/// Race posture: admission can under-count around a concurrent flush (a
/// key re-admitted just before its release leaves the set briefly without
/// a window) — that over-admits, the safe direction; it can never
/// over-count, because every admit either finds the key present or inserts
/// it, and every release corresponds to a real prior insert.
pub struct PendingGate {
    max_open: usize,
    open: HashSet<(String, String)>,
}

impl PendingGate {
    fn new(max_open: usize) -> Self {
        Self {
            max_open,
            open: HashSet::new(),
        }
    }

    /// May a send for `key` enter the coalescer? `true` when the key
    /// already has an open window or the map has room for one more.
    pub fn admit(&mut self, key: &(String, String)) -> bool {
        if self.open.contains(key) {
            return true;
        }
        if self.open.len() >= self.max_open {
            return false;
        }
        self.open.insert(key.clone());
        true
    }

    /// Mark `key`'s window closed (flush) — or undo an admission whose
    /// channel send failed. Idempotent.
    pub fn release(&mut self, key: &(String, String)) {
        self.open.remove(key);
    }

    fn clear(&mut self) {
        self.open.clear();
    }
}

/// Shared handle to the [PendingGate].
pub type SharedPendingGate = Arc<Mutex<PendingGate>>;

/// One debounced (tenant, token) target.
struct Pending {
    /// Fixed by the FIRST send in the window; later sends never move it —
    /// EXCEPT the retry re-debounce, which sets it to now + delay (the
    /// embedded router shape).
    deadline: Instant,
    /// Sends attempted so far (transient-retry budget, B2).
    attempts: u8,
    /// Latest payload wins; the send that actually goes out.
    winner: SendJob,
    /// Earlier sends coalesced away — each still gets its receipt. Capped
    /// at [CoalescerLimits::losers_max].
    losers: VecDeque<SendJob>,
    /// Losers evicted past the cap during this window — receipted at flush
    /// exactly like losers (same outcome, coalesced detail), preserving the
    /// one-receipt-per-push_id invariant without fabricating outcomes.
    evicted: Vec<SendJob>,
}

/// One accepted send, as handed to the coalescer.
#[derive(Debug, Clone)]
pub struct SendJob {
    pub tenant_id: String,
    pub token: String,
    pub platform: crate::store::Platform,
    /// uuid v4 (pin 0.4) — one per accepted request, echoed on its receipt.
    pub push_id: String,
    /// Latest-wins payload (silent doorbell or interpolated visible).
    pub payload: PushPayload,
    /// Caller override; None => [default_collapse_key] per (tenant, token).
    pub collapse_key: Option<String>,
    /// Echoed into this job's receipt (push-LSN correlation channel).
    pub metadata: Option<serde_json::Value>,
}

/// Handle back to the coalescer's inbox + admission gate.
#[derive(Clone)]
pub struct Coalescer {
    /// Bounded — try_send only; a full queue is a 503 at the route.
    pub tx: mpsc::Sender<SendJob>,
    /// The pending-key ceiling's gate — the route admits before try_send
    /// and the task releases on flush.
    pub gate: SharedPendingGate,
}

/// Spawn the coalescer task (plan task 1.6; ceilings per task 4.1).
/// Dropping every clone of the returned sender ends the task after a
/// final drain-flush.
#[must_use]
pub fn spawn_coalescer(
    store: Arc<dyn Store>,
    rails: Rails,
    debounce: Duration,
    limits: CoalescerLimits,
    retry: RetryPolicy,
) -> Coalescer {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let gate: SharedPendingGate = Arc::new(Mutex::new(PendingGate::new(limits.pending_keys_max)));
    tokio::spawn(coalesce(
        rx,
        store,
        rails,
        debounce,
        limits,
        retry,
        Arc::clone(&gate),
    ));
    Coalescer { tx, gate }
}

/// The debounce loop — router.rs's shape: recv arm absorbs, min-deadline
/// sleep arm flushes what is due, channel close does a final full drain.
async fn coalesce(
    mut rx: mpsc::Receiver<SendJob>,
    store: Arc<dyn Store>,
    rails: Rails,
    debounce: Duration,
    limits: CoalescerLimits,
    retry: RetryPolicy,
    gate: SharedPendingGate,
) {
    let mut pending: HashMap<(String, String), Pending> = HashMap::new();
    loop {
        // Recomputed every iteration: min over pending deadlines (the map is
        // bounded by targets active in one window — coalescer-scoped, never
        // request-scoped, and now ceiling-enforced via the gate).
        let next = pending.values().map(|p| p.deadline).min();
        tokio::select! {
            maybe = rx.recv() => {
                if let Some(job) = maybe {
                    absorb(job, &mut pending, debounce, limits);
                } else {
                    // Shutdown drain: dispatch TERMINAL (allow_retry=false) —
                    // a re-debounce would have nowhere left to wait; the
                    // transient outcome receipts as-is and the LSN
                    // checkpoint reconciles the data later.
                    let drained: Vec<((String, String), Pending)> = pending.drain().collect();
                    for (key, p) in drained {
                        dispatch_one(p, &store, &rails, &retry, false).await;
                        gate.lock().expect("pending gate").release(&key);
                    }
                    gate.lock().expect("pending gate").clear();
                    return;
                }
            }
            () = sleep_until(next) => {
                flush_due(&mut pending, &store, &rails, &retry, &gate).await;
            }
        }
    }
}

/// None parks forever (only the recv arm can fire); Some(d) sleeps to the
/// earliest pending deadline.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
        None => std::future::pending().await,
    }
}

/// Absorb one job: existing window -> the previous winner demotes to the
/// losers list and the new job becomes the winner (payload REPLACE, deadline
/// untouched; the losers list is capped at `limits.losers_max` — the
/// oldest beyond the cap is evicted into the flush-time receipt batch);
/// no window -> open one with deadline now + debounce.
fn absorb(
    job: SendJob,
    pending: &mut HashMap<(String, String), Pending>,
    debounce: Duration,
    limits: CoalescerLimits,
) {
    let key = (job.tenant_id.clone(), job.token.clone());
    match pending.get_mut(&key) {
        Some(p) => {
            let demoted = std::mem::replace(&mut p.winner, job);
            if p.losers.len() >= limits.losers_max {
                // Ceiling eviction (audit finding 2): the list stays capped
                // NOW; the evicted job's receipt is written at flush with
                // the window's real outcome (never a fabricated one) — the
                // every-push_id-one-receipt invariant is preserved.
                if let Some(oldest) = p.losers.pop_front() {
                    p.evicted.push(oldest);
                }
            }
            p.losers.push_back(demoted);
        }
        None => {
            pending.insert(
                key,
                Pending {
                    deadline: Instant::now() + debounce,
                    attempts: 0,
                    winner: job,
                    losers: VecDeque::new(),
                    evicted: Vec::new(),
                },
            );
        }
    }
}

/// Flush every entry whose deadline has passed (due-only — later windows
/// keep their fixed deadlines, the same filter as router.rs's flush) and
/// release each key's admission-gate slot.
async fn flush_due(
    pending: &mut HashMap<(String, String), Pending>,
    store: &Arc<dyn Store>,
    rails: &Rails,
    retry: &RetryPolicy,
    gate: &SharedPendingGate,
) {
    let now = Instant::now();
    let due: Vec<(String, String)> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for key in due {
        if let Some(p) = pending.remove(&key) {
            match dispatch_one(p, store, rails, retry, true).await {
                // Transient + budget left: re-debounce the window (deadline
                // now + delay, attempts bumped inside dispatch_one) and KEEP
                // the gate slot — the window is still open. A NEW send for
                // the same key in the gap lands in absorb against the
                // re-inserted window (latest-wins payload rides the retry).
                Some(p2) => {
                    pending.insert(key, p2);
                }
                // Terminal: receipts written, slot freed.
                None => {
                    gate.lock().expect("pending gate").release(&key);
                }
            }
        }
    }
}

/// Flush one target: rail send, winner + loser (+ evicted) receipts, prune
/// on Unregistered (plan task 1.2's prune trigger). Returns Some(pending)
/// when the rail answered transient AND the retry budget has room — the
/// caller re-debounces the whole window (receipts deliberately NOT written:
/// one receipt per push_id, only at the terminal attempt); None when
/// terminal (receipts written). `allow_retry=false` forces the terminal
/// path (shutdown drain).
async fn dispatch_one(
    mut p: Pending,
    store: &Arc<dyn Store>,
    rails: &Rails,
    retry: &RetryPolicy,
    allow_retry: bool,
) -> Option<Pending> {
    let collapse_key = p
        .winner
        .collapse_key
        .clone()
        .unwrap_or_else(|| default_collapse_key(&p.winner.tenant_id, &p.winner.token));
    // Dispatch through borrows — p must stay whole for the Some(p) return.
    let rail_outcome = rails
        .dispatch(
            p.winner.platform,
            &p.winner.token,
            Some(&collapse_key),
            &p.winner.payload,
        )
        .await;
    // Transient + budget left (the embedded router shape): re-debounce the
    // whole entry — the retry re-sends the winning payload and the eventual
    // outcome still receipts every push_id exactly once.
    if allow_retry
        && rail_outcome == RailOutcome::TransientRetryable
        && p.attempts + 1 < retry.effective_max_attempts()
    {
        p.attempts += 1;
        p.deadline = Instant::now() + retry.delay;
        return Some(p);
    }
    let exhausted_transient = rail_outcome == RailOutcome::TransientRetryable && allow_retry;
    let (outcome, rail_detail) = Outcome::from_rail(&rail_outcome);
    // Terminal path: p is consumed piecemeal from here (never returned).
    let winner = p.winner;
    // A transient past the budget carries the exhaustion on the winner
    // receipt detail — operators see WHY it never delivered without
    // correlating rail logs.
    let rail_detail = if exhausted_transient && rail_detail.is_none() {
        Some(format!(
            "retries exhausted after {} attempts",
            p.attempts + 1
        ))
    } else {
        rail_detail
    };
    let provider_ts = crate::store::now_rfc3339();

    // Winner receipt — the send that actually went out.
    let winner_receipt = NewReceipt {
        tenant_id: winner.tenant_id.clone(),
        push_id: winner.push_id.clone(),
        token: winner.token.clone(),
        outcome,
        detail: rail_detail,
        metadata: winner.metadata.clone(),
        provider_ts: provider_ts.clone(),
    };
    if let Err(e) = store.append_receipt(&winner_receipt).await {
        warn!(error = %e, push_id = %winner.push_id, "receipt append failed");
    }

    // Loser receipts: same outcome, "coalesced:<winner>" detail, EACH echo's
    // own request metadata (the correlation channel must survive
    // coalescing or push-LSN acks go missing). Evicted losers (ceiling
    // overflow, absorbed earlier in the window) are receipted identically.
    for loser in p.losers.into_iter().chain(p.evicted) {
        let loser_receipt = NewReceipt {
            tenant_id: loser.tenant_id,
            push_id: loser.push_id.clone(),
            token: loser.token,
            outcome,
            detail: Some(format!("coalesced:{}", winner.push_id)),
            metadata: loser.metadata,
            provider_ts: provider_ts.clone(),
        };
        if let Err(e) = store.append_receipt(&loser_receipt).await {
            warn!(error = %e, push_id = %loser.push_id, "receipt append failed");
        }
    }

    if rail_outcome == RailOutcome::Unregistered {
        // APNs 410 / FCM UNREGISTERED / Web Push 404-410: the provider says
        // the target is gone — prune the registry row (owner-scoped delete;
        // a Foreign outcome here cannot happen for a live window's key).
        match store
            .delete_token_owner_scoped(&winner.tenant_id, &winner.token)
            .await
        {
            Ok(_) => {
                info!(
                    tenant = %winner.tenant_id,
                    token = %winner.token,
                    "pruned unregistered token"
                );
            }
            Err(e) => {
                warn!(error = %e, token = %winner.token, "token prune failed");
            }
        }
    }
    None // terminal: receipts written, caller frees the gate slot
}

/// Periodic receipt retention sweep (NOSTOS_PUSHD_RECEIPT_RETENTION_SECS).
/// Ticks at most hourly, at least minutely — the first tick fires
/// immediately, so boot sweeps leftovers from a previous run.
pub fn spawn_retention_sweeper(store: Arc<dyn Store>, retention_secs: u64) {
    tokio::spawn(async move {
        let tick_secs = retention_secs.clamp(60, 3600);
        let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
        loop {
            ticker.tick().await;
            match store.sweep_receipts(retention_secs).await {
                Ok(0) => {}
                Ok(n) => info!(swept = n, "receipt retention sweep"),
                Err(e) => warn!(error = %e, "receipt retention sweep failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{CoalescerLimits, PendingGate};
    use std::time::Duration;

    #[test]
    fn gate_admits_until_ceiling_then_refuses_new_keys() {
        let mut gate = PendingGate::new(2);
        let k1 = ("t".to_string(), "a".to_string());
        let k2 = ("t".to_string(), "b".to_string());
        let k3 = ("t".to_string(), "c".to_string());
        assert!(gate.admit(&k1));
        assert!(gate.admit(&k2));
        assert!(!gate.admit(&k3), "new key past the ceiling");
        assert!(gate.admit(&k1), "an already-open key always passes");
        gate.release(&k1);
        assert!(gate.admit(&k3), "released slot is reusable");
        // Release is idempotent.
        gate.release(&k1);
    }

    #[test]
    fn defaults_match_the_pinned_ceilings() {
        let limits = CoalescerLimits::default();
        assert_eq!(limits.pending_keys_max, 10_000);
        assert_eq!(limits.losers_max, 64);
    }

    #[test]
    fn retry_policy_defaults_match_the_embedded_router() {
        let p = super::RetryPolicy::default();
        assert_eq!(
            p.max_attempts, 2,
            "1 initial + 1 deferred (router MAX_ATTEMPTS)"
        );
        assert_eq!(p.delay, Duration::from_millis(500), "router RETRY_DELAY");
        assert_eq!(p.effective_max_attempts(), 2);
    }

    #[test]
    fn retry_policy_zero_clamps_to_one_send() {
        let p = super::RetryPolicy {
            max_attempts: 0,
            delay: Duration::from_millis(1),
        };
        assert_eq!(
            p.effective_max_attempts(),
            1,
            "a configured 0 means no retries, never no send"
        );
    }
}
