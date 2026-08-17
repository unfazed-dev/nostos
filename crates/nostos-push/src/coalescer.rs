//! Debounce coalescer (ADR-0038 §2, plan task 1.6) — the daemon's send
//! pipeline core.
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
//! ponytail: no retries in v1 — a transient rail outcome is terminal on the
//! receipt and callers retry (the RemoteNotifier of Wave 2 will). Upgrade
//! path: an attempts counter on Pending plus scheduled re-flush, exactly
//! the embedded router's shape; the receipt stays the source of truth.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nostos_infra::push::{PushPayload, RailOutcome};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::rail::{default_collapse_key, Rails};
use crate::store::{NewReceipt, Outcome, Store};

/// Bounded channel capacity between the send route and the coalescer task.
/// Full channel => the handler 503s; the request path never blocks.
pub const QUEUE_CAPACITY: usize = 1024;

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

/// Handle back to the coalescer's inbox.
#[derive(Clone)]
pub struct Coalescer {
    /// Bounded — try_send only; a full queue is a 503 at the route.
    pub tx: mpsc::Sender<SendJob>,
}

/// One debounced (tenant, token) target.
struct Pending {
    /// Fixed by the FIRST send in the window; later sends never move it.
    deadline: Instant,
    /// Latest payload wins; the send that actually goes out.
    winner: SendJob,
    /// Earlier sends coalesced away — each still gets its receipt.
    losers: Vec<SendJob>,
}

/// Spawn the coalescer task (plan task 1.6). Dropping every clone of the
/// returned sender ends the task after a final drain-flush.
#[must_use]
pub fn spawn_coalescer(store: Arc<dyn Store>, rails: Rails, debounce: Duration) -> Coalescer {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    tokio::spawn(coalesce(rx, store, rails, debounce));
    Coalescer { tx }
}

/// The debounce loop — router.rs's shape: recv arm absorbs, min-deadline
/// sleep arm flushes what is due, channel close does a final full drain.
async fn coalesce(
    mut rx: mpsc::Receiver<SendJob>,
    store: Arc<dyn Store>,
    rails: Rails,
    debounce: Duration,
) {
    let mut pending: HashMap<(String, String), Pending> = HashMap::new();
    loop {
        // Recomputed every iteration: min over pending deadlines (the map is
        // bounded by targets active in one window — coalescer-scoped, never
        // request-scoped).
        let next = pending.values().map(|p| p.deadline).min();
        tokio::select! {
            maybe = rx.recv() => {
                if let Some(job) = maybe {
                    absorb(job, &mut pending, debounce);
                } else {
                    let drained: Vec<Pending> = pending.drain().map(|(_, p)| p).collect();
                    for p in drained {
                        dispatch_one(p, &store, &rails).await;
                    }
                    return;
                }
            }
            () = sleep_until(next) => {
                flush_due(&mut pending, &store, &rails).await;
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
/// untouched); no window -> open one with deadline now + debounce.
fn absorb(job: SendJob, pending: &mut HashMap<(String, String), Pending>, debounce: Duration) {
    let key = (job.tenant_id.clone(), job.token.clone());
    match pending.get_mut(&key) {
        Some(p) => {
            p.losers.push(std::mem::replace(&mut p.winner, job));
        }
        None => {
            pending.insert(
                key,
                Pending {
                    deadline: Instant::now() + debounce,
                    winner: job,
                    losers: Vec::new(),
                },
            );
        }
    }
}

/// Flush every entry whose deadline has passed (due-only — later windows
/// keep their fixed deadlines, the same filter as router.rs's flush).
async fn flush_due(
    pending: &mut HashMap<(String, String), Pending>,
    store: &Arc<dyn Store>,
    rails: &Rails,
) {
    let now = Instant::now();
    let due: Vec<(String, String)> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for key in due {
        if let Some(p) = pending.remove(&key) {
            dispatch_one(p, store, rails).await;
        }
    }
}

/// Flush one target: rail send, winner + loser receipts, prune on
/// Unregistered (plan task 1.2's prune trigger).
async fn dispatch_one(p: Pending, store: &Arc<dyn Store>, rails: &Rails) {
    let winner = p.winner;
    let collapse_key = winner
        .collapse_key
        .clone()
        .unwrap_or_else(|| default_collapse_key(&winner.tenant_id, &winner.token));
    let rail_outcome = rails
        .dispatch(
            winner.platform,
            &winner.token,
            Some(&collapse_key),
            &winner.payload,
        )
        .await;
    let (outcome, rail_detail) = Outcome::from_rail(&rail_outcome);
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
    // its own request metadata (the correlation channel must survive
    // coalescing or push-LSN acks go missing).
    for loser in p.losers {
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
