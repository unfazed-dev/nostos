//! RemoteNotifier — delegating the push doorbell to a remote nostos-pushd
//! (ADR-0038 §3, plan tasks 2.1–2.2 + the pin-2.0 amendment). The opt-in
//! third push mode: embedded `PushRouter` (ADR-0037) stays the default;
//! the composition root wires THIS adapter only when
//! `NOSTOS_PUSH_REMOTE_URL` + `NOSTOS_PUSH_REMOTE_KEY` are both set.
//!
//! ## Architecture (the non-blocking contract is law)
//!
//! [`PushNotifier::notify`] is the fan-out hot loop's matched-set drain —
//! it MUST return promptly (see the port doc, ADR-0037 §4). Here it is a
//! `try_send` into a bounded hint channel (capacity
//! [`HINT_CHANNEL_CAPACITY`], mirroring the embedded router); a full
//! channel drops-and-counts into `Metrics::push_dropped`. No network I/O
//! ever happens on the caller's path.
//!
//! Two background tasks do the rest:
//!
//! 1. **Deliver** — drains hints: resolve account→tokens via the SAME
//!    [`PushTokenRegistry`] the embedded router uses, apply
//!    [`SessionStore::account_online`] presence at send time (never
//!    doorbell an online account — ADR-0037 §4; the empty-account hint
//!    expands tenant-wide, §1 amendment), build the payload with the SAME
//!    `RouterConfig` tables/templates (`router::build_payload` — the one
//!    shared seam; template semantics are not reinvented next door), then
//!    POST `/v1/send` once per offline token in rail mode: the request
//!    carries the registry's `platform` so the daemon dispatches directly
//!    without any registry of ours (pin 2.0 — registries are never shared
//!    or synced; a second networked registry is exactly the drift ADR-0037
//!    §1 rejects). `metadata: {table, lsn, account}` rides every send —
//!    the receipt echo is the push-LSN correlation channel.
//! 2. **Receipts** — polls `GET /v1/receipts?since=<cursor>` and maps
//!    outcomes to the same `Metrics` accounting the embedded router does
//!    at flush: `delivered`→`push_sent` (+ correlation map),
//!    `unregistered`→prune the LOCAL registry row + `push_pruned`,
//!    `transient`/`fatal`→`push_failed`. Coalesced-away receipts (the
//!    daemon's pinned `coalesced:<winner>` detail, plan 1.6) never touched
//!    a rail, so they feed only the correlation map, never the counters.
//!    Poll failures back off exponentially; the cursor makes re-polls
//!    idempotent, and optionally persists across restarts
//!    (`NOSTOS_PUSH_REMOTE_STATE_PATH` — see [`CursorFile`]).
//!
//! ## Daemon outage (ponytail: no durable spool in v1)
//!
//! An unreachable daemon fails each POST fast (drop-and-count into
//! `push_failed`); the bounded hint channel is the backlog and its
//! overflow is counted in `push_dropped`. A daemon-down window therefore
//! DROPS doorbells — by design: push is a wake-up trigger, never a data
//! channel, and the client's durable LSN checkpoint is the correctness
//! mechanism, so sync correctness is unaffected. Upgrade path: a durable
//! spool (SQLite outbox) if operators ever need outage survival; until a
//! deployment asks for it, the simpler drop-and-count is the honest
//! behavior.
//!
//! ## Design choice (plan Wave-2 brief: seam vs parallel)
//!
//! PARALLEL implementation reusing `PushTokenRegistry`/`SessionStore`/
//! `RouterConfig` + the one pure seam `router::build_payload` (made
//! `pub(crate)` for exactly this). Extracting the router's whole flush
//! step was rejected after reading `router.rs`: its loop is welded to
//! SYNCHRONOUS `RailOutcome`s (retry re-debounce, prune, per-outcome
//! counters), while this adapter learns outcomes asynchronously from the
//! receipts poll — sharing that loop would force a "result arrives later"
//! contortion onto the embedded path for zero reuse gain. The daemon is
//! the coalescing rail in delegation mode (pin 2.0), so this side also
//! deliberately carries NO debounce window of its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostos_application::ports::{Metrics, PushHint, PushNotifier, SessionStore};
use nostos_domain::Lsn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::router::{build_payload, PushTokenRegistry, RouterConfig, PLATFORM_APNS_LIVE_ACTIVITY};

/// Inbound hint channel depth — the same bound the embedded router uses.
/// Full ⇒ drop-and-count (`Metrics::push_dropped`); this channel IS the
/// bounded backlog a daemon outage fills before hints start dropping.
const HINT_CHANNEL_CAPACITY: usize = 1024;
/// Receipt poll cadence once a page has drained.
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Backoff bounds after a failed poll (transport error / non-2xx / bad
/// body). Exponential from [`RECEIPT_BACKOFF_MIN`], capped.
const RECEIPT_BACKOFF_MIN: Duration = Duration::from_millis(250);
const RECEIPT_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Receipt page size — the contract's maximum, so the log drains in as few
/// polls as possible.
const RECEIPT_PAGE: usize = 1000;
/// Minimum spacing between cursor-state writes while pages stream in; a
/// caught-up (short) page always flushes a pending newer value immediately.
/// Metrics-only honesty: no fsync — a crash loses at most this much cursor.
const CURSOR_WRITE_INTERVAL: Duration = Duration::from_secs(1);
/// The daemon's pinned loser-receipt detail prefix (plan 1.6): a receipt
/// whose detail starts with this never caused a rail send.
const COALESCED_DETAIL_PREFIX: &str = "coalesced:";

/// The delegation adapter over the [`PushNotifier`] port (ADR-0038 §3).
/// Constructing it spawns the deliver + receipts tasks; dropping every
/// clone ends the deliver task after draining pending hints (the receipts
/// task polls for the process lifetime — it owns no queue to drain).
pub struct RemoteNotifier {
    tx: mpsc::Sender<PushHint>,
    metrics: Arc<Metrics>,
}

impl RemoteNotifier {
    /// Compose the delegating notifier. Must be called inside a tokio
    /// runtime (it spawns the background tasks). `base_url` is the
    /// daemon's origin (e.g. `http://127.0.0.1:8090`); `api_key` is the
    /// bearer key minted from the daemon's `NOSTOS_PUSHD_API_KEYS`.
    /// `receipts_state_path` opts the receipts cursor into on-disk
    /// persistence across restarts (`None` keeps it in-memory, logged).
    #[must_use]
    pub fn new(
        base_url: &str,
        api_key: &str,
        registry: Arc<dyn PushTokenRegistry>,
        store: Arc<dyn SessionStore>,
        config: RouterConfig,
        metrics: Arc<Metrics>,
        receipts_state_path: Option<PathBuf>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(HINT_CHANNEL_CAPACITY);
        let base = base_url.trim_end_matches('/');
        let client = super::http_client();
        let delegation = Delegation {
            daemon: DaemonEndpoint {
                client: client.clone(),
                send_url: format!("{base}/v1/send"),
                api_key: api_key.to_string(),
            },
            registry,
            store,
            config,
            metrics: Arc::clone(&metrics),
        };
        let receipt_registry = Arc::clone(&delegation.registry);
        let receipt_metrics = Arc::clone(&delegation.metrics);
        if let Some(path) = &receipts_state_path {
            info!(
                path = %path.display(),
                "remote push receipts cursor persists across restarts"
            );
        } else {
            info!(
                "remote push receipts cursor is in-memory (NOSTOS_PUSH_REMOTE_STATE_PATH \
                 unset — a restart replays the daemon's receipt log, metrics-only skew)"
            );
        }
        tokio::spawn(deliver_loop(rx, delegation));
        tokio::spawn(receipt_loop(
            client,
            format!("{base}/v1/receipts"),
            api_key.to_string(),
            receipt_registry,
            receipt_metrics,
            receipts_state_path,
        ));
        Self { tx, metrics }
    }
}

#[async_trait]
impl PushNotifier for RemoteNotifier {
    async fn notify(&self, hint: PushHint) {
        use std::sync::atomic::Ordering;
        if self.tx.try_send(hint).is_err() {
            // Full (or consumer gone): the same failure class as the
            // fan-out channel's drop-on-full — count it in the same counter.
            self.metrics.push_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Everything the deliver loop needs, grouped so the loop signature stays
/// small. Cloned-cheap (Arcs + one config value), mirroring the embedded
/// router's spawned-consumer shape.
struct Delegation {
    daemon: DaemonEndpoint,
    registry: Arc<dyn PushTokenRegistry>,
    store: Arc<dyn SessionStore>,
    config: RouterConfig,
    metrics: Arc<Metrics>,
}

/// The daemon's send endpoint (URL + auth + shared HTTP client).
struct DaemonEndpoint {
    client: reqwest::Client,
    send_url: String,
    api_key: String,
}

impl DaemonEndpoint {
    /// POST one rail-mode send. Fire-and-forget: 2xx is the daemon's
    /// "accepted into the coalescer" — outcomes arrive later via receipts.
    async fn send(&self, body: &SendRequestDto, metrics: &Metrics) {
        use std::sync::atomic::Ordering;
        match self
            .client
            .post(&self.send_url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                warn!(status = %resp.status(), "remote push daemon rejected a send");
                metrics.push_failed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Unreachable daemon: drop-and-count (see the module doc's
                // outage posture) — the doorbell is lost, sync is not.
                warn!(error = %e, "remote push daemon unreachable");
                metrics.push_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Drain hints → resolve → POST. Ends when every sender clone drops.
async fn deliver_loop(mut rx: mpsc::Receiver<PushHint>, d: Delegation) {
    while let Some(hint) = rx.recv().await {
        deliver_one(&hint, &d).await;
    }
}

/// Deliver one hint: presence-filtered tokens × one POST each.
async fn deliver_one(hint: &PushHint, d: &Delegation) {
    use std::sync::atomic::Ordering;
    // Account→tokens; the empty-account marker expands tenant-wide
    // (ADR-0037 §1 amendment — the killed-app case).
    let tokens = if hint.account_id.is_empty() {
        d.registry.list_by_tenant(&hint.tenant_id).await
    } else {
        d.registry
            .list_by_account(&hint.tenant_id, &hint.account_id)
            .await
    };
    let tokens = match tokens {
        Ok(t) => t,
        Err(e) => {
            warn!(
                error = %e,
                tenant = %hint.tenant_id,
                "remote push token lookup failed"
            );
            d.metrics.push_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    if tokens.is_empty() {
        return;
    }
    // Same payload resolution as the embedded router's flush. A
    // live-activity-configured table's PushTables row is a placeholder
    // Visible (parse_push_tables) — ordinary devices get the plain
    // doorbell; activity tokens are skipped below.
    let payload = if d.config.live_activities.contains_key(&hint.table) {
        super::PushPayload::Silent {
            table: hint.table.clone(),
            lsn: hint.lsn,
        }
    } else {
        build_payload(&d.config, &hint.table, hint.lsn, hint.payload.as_deref())
    };
    let payload_dto = PayloadDto::from_push(&payload);
    for t in tokens {
        // Presence at send time, per resolved account (ADR-0037 §4) —
        // closes the enqueue race exactly like the router's flush.
        // ponytail (L4, same as router): keyed by bare account id, so a
        // cross-tenant collision over-suppresses; harmless (missed pushes
        // lose nothing — the LSN checkpoint is correctness).
        if d.store.account_online(&t.account_id).await {
            continue;
        }
        // ponytail: Live Activity delegation is deferred — the 0.2.0
        // daemon contract has no content-state send, and an ActivityKit
        // token cannot be doorbelled (a plain push to it is wire-invalid).
        // Upgrade path: a contract extension for content-state sends; until
        // then those tokens are skipped, not mis-sent.
        if t.platform == PLATFORM_APNS_LIVE_ACTIVITY {
            continue;
        }
        d.daemon
            .send(
                &SendRequestDto {
                    token: t.token.clone(),
                    platform: t.platform.clone(),
                    payload: payload_dto.clone(),
                    // Collapse key = the subscription's table, mirroring
                    // the router: rail-native supersede per (device,
                    // subscription) — the daemon coalesces (tenant, token).
                    collapse_key: hint.table.clone(),
                    metadata: metadata_echo(&hint.table, hint.lsn, &t.account_id),
                },
                &d.metrics,
            )
            .await;
    }
}

/// The metadata echo on every delegated send — the push-LSN correlation
/// channel (pin 0.4): the daemon copies it into the receipt, the receipts
/// task reads it back. `lsn` rides the wire as a decimal string (the
/// contract's silent-payload convention).
fn metadata_echo(table: &str, lsn: Lsn, account: &str) -> Value {
    serde_json::json!({
        "table": table,
        "lsn": lsn.raw().to_string(),
        "account": account,
    })
}

// ---------------------------------------------------------------- wire DTOs

/// POST /v1/send body (contract 0.2.0) — `platform` always set: rail mode
/// (pin 2.0) is what keeps delegation registry-free.
#[derive(Serialize)]
struct SendRequestDto {
    token: String,
    platform: String,
    payload: PayloadDto,
    collapse_key: String,
    metadata: Value,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum PayloadDto {
    Silent(SilentDto),
    Visible(VisibleDto),
}

impl PayloadDto {
    /// The infra payload → the contract's wire shape.
    fn from_push(p: &super::PushPayload) -> Self {
        match p {
            super::PushPayload::Silent { table, lsn } => Self::Silent(SilentDto {
                silent: SilentBody {
                    table: table.clone(),
                    lsn: lsn.raw().to_string(),
                },
            }),
            super::PushPayload::Visible {
                title,
                body,
                category,
            } => Self::Visible(VisibleDto {
                visible: VisibleBody {
                    title: title.clone(),
                    body: body.clone(),
                    category: category.clone(),
                },
            }),
        }
    }
}

#[derive(Serialize, Clone)]
struct SilentDto {
    silent: SilentBody,
}

#[derive(Serialize, Clone)]
struct SilentBody {
    table: String,
    lsn: String,
}

#[derive(Serialize, Clone)]
struct VisibleDto {
    visible: VisibleBody,
}

#[derive(Serialize, Clone)]
struct VisibleBody {
    title: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
}

#[derive(serde::Deserialize)]
struct ReceiptsDto {
    #[serde(default)]
    receipts: Vec<ReceiptDto>,
}

#[derive(serde::Deserialize)]
struct ReceiptDto {
    seq: i64,
    push_id: String,
    token: String,
    outcome: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
}

/// Poll the receipt log forever, advancing the `since` cursor page by
/// page. Failures back off; a full page loops immediately to drain a
/// backlogged log. When `state_path` is set the cursor persists across
/// restarts (see [`CursorFile`]).
async fn receipt_loop(
    client: reqwest::Client,
    receipts_url: String,
    api_key: String,
    registry: Arc<dyn PushTokenRegistry>,
    metrics: Arc<Metrics>,
    state_path: Option<PathBuf>,
) {
    // The cursor persists across restarts when a state file is configured
    // (NOSTOS_PUSH_REMOTE_STATE_PATH): loaded once here, written back after
    // every advance — throttled to at most one write per second, plus a
    // flush on a caught-up page. Atomic tmp+rename, deliberately NO fsync:
    // a crash loses at most ~1s of cursor and the daemon replay that
    // follows is monotonicity-guarded (metrics-only skew, never state
    // corruption). A FORGED state file (local write access to an
    // operator-set path) can only skip receipts forward — worst case a
    // delayed unregistered-prune; delivery counters stay monotonic.
    let mut cursor_file = state_path.map(CursorFile::load);
    let mut since: i64 = cursor_file.as_ref().map_or(0, |f| f.persisted);
    let mut failures: u32 = 0;
    loop {
        let page = client
            .get(format!("{receipts_url}?since={since}&limit={RECEIPT_PAGE}"))
            .bearer_auth(&api_key)
            .send()
            .await;
        let body = match page {
            Ok(resp) if resp.status().is_success() => match resp.json::<ReceiptsDto>().await {
                Ok(body) => {
                    failures = 0;
                    body
                }
                Err(e) => {
                    warn!(error = %e, "remote push receipts body unparseable");
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                    continue;
                }
            },
            Ok(resp) => {
                warn!(status = %resp.status(), "remote push receipts poll rejected");
                failures += 1;
                tokio::time::sleep(backoff(failures)).await;
                continue;
            }
            Err(e) => {
                warn!(error = %e, "remote push receipts poll unreachable");
                failures += 1;
                tokio::time::sleep(backoff(failures)).await;
                continue;
            }
        };
        for r in &body.receipts {
            since = since.max(r.seq);
        }
        for r in &body.receipts {
            apply_receipt(r, &registry, &metrics).await;
        }
        let caught_up = body.receipts.len() < RECEIPT_PAGE;
        // Persist AFTER applying: a crash between the two replays
        // idempotent receipts, while persisting first would skip them.
        if let Some(file) = cursor_file.as_mut() {
            file.maybe_persist(since, caught_up);
        }
        if caught_up {
            tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
        }
    }
}

/// Map one receipt onto `Metrics` + the local registry, mirroring the
/// embedded router's per-outcome accounting.
async fn apply_receipt(
    r: &ReceiptDto,
    registry: &Arc<dyn PushTokenRegistry>,
    metrics: &Arc<Metrics>,
) {
    use std::sync::atomic::Ordering;
    // A coalesced-away send never touched a rail (the daemon's pinned
    // `coalesced:<winner>` detail, plan 1.6): it feeds only the LSN
    // correlation map, never the sent/failed/pruned counters.
    let loser = r
        .detail
        .as_deref()
        .is_some_and(|d| d.starts_with(COALESCED_DETAIL_PREFIX));
    match r.outcome.as_str() {
        "delivered" => {
            if !loser {
                metrics.push_sent.fetch_add(1, Ordering::Relaxed);
            }
            if let Some((account, lsn)) = correlation(r.metadata.as_ref()) {
                record_lsn_max(metrics, &account, lsn);
            }
        }
        "unregistered" if !loser => {
            // The daemon's rail says the target is gone — prune the LOCAL
            // row (the registry this side resolved against). Acting on rail
            // outcomes is not registry sharing; the embedded router prunes
            // the same way.
            match registry.prune(&r.token).await {
                Ok(n) => {
                    metrics.push_pruned.fetch_add(n, Ordering::Relaxed);
                    if n > 0 {
                        info!(push_id = %r.push_id, token = %r.token, "pruned unregistered token");
                    }
                }
                Err(e) => warn!(error = %e, token = %r.token, "remote push prune failed"),
            }
        }
        "transient" | "fatal" if !loser => {
            metrics.push_failed.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// The (account, lsn) correlation tuple from a receipt's metadata echo.
fn correlation(metadata: Option<&Value>) -> Option<(String, u64)> {
    let m = metadata?;
    let account = m.get("account")?.as_str()?.to_string();
    let lsn = m.get("lsn")?.as_str()?.parse().ok()?;
    Some((account, lsn))
}

/// `Metrics::record_push_lsn` with a monotonicity guard: receipts arrive
/// winner-first then losers (the daemon's append order), and a loser's LSN
/// is ≤ its winner's — the correlation map must never move backward.
fn record_lsn_max(metrics: &Metrics, account: &str, lsn: u64) {
    if let Ok(mut map) = metrics.push_last_lsn.lock() {
        match map.get(account) {
            Some(&cur) if cur >= lsn => {}
            _ => {
                map.insert(account.to_string(), lsn);
            }
        }
    }
}

/// Exponential poll backoff: `min` doubled per consecutive failure, capped
/// at [`RECEIPT_BACKOFF_MAX`].
fn backoff(failures: u32) -> Duration {
    let shifts = failures.saturating_sub(1).min(5);
    (RECEIPT_BACKOFF_MIN * (1u32 << shifts)).min(RECEIPT_BACKOFF_MAX)
}

// -------------------------------------------------- receipts cursor state

/// The state file's entire content: `{"receipts_since": N}`. One serde DTO
/// so read and write share a single shape definition.
#[derive(Serialize, Deserialize)]
struct CursorDto {
    receipts_since: i64,
}

/// Throttled writer for the receipts-cursor state file. Metrics-only
/// honesty (see the module doc): no fsync — a crash may lose up to
/// [`CURSOR_WRITE_INTERVAL`] of cursor, and the daemon replay that follows
/// is monotonicity-guarded.
struct CursorFile {
    path: PathBuf,
    /// The newest value durably on disk — [`CursorFile::maybe_persist`] is
    /// a no-op while the in-memory cursor equals it.
    persisted: i64,
    /// When the last write landed, for the one-per-second throttle.
    last_write: Option<Instant>,
}

impl CursorFile {
    /// Load the cursor: a missing file is a fresh start (debug log), and an
    /// unparseable or unreadable one warns and ALSO starts at 0 — the
    /// daemon replays its retention window, which skews metrics but cannot
    /// corrupt state.
    fn load(path: PathBuf) -> Self {
        let persisted = match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<CursorDto>(&content) {
                Ok(dto) => dto.receipts_since,
                Err(e) => {
                    warn!(
                        error = %e,
                        path = %path.display(),
                        "remote push receipts cursor file unparseable — starting at 0"
                    );
                    0
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    path = %path.display(),
                    "remote push receipts cursor file absent — fresh start at 0"
                );
                0
            }
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "remote push receipts cursor file unreadable — starting at 0"
                );
                0
            }
        };
        Self {
            path,
            persisted,
            last_write: None,
        }
    }

    /// Write `since` back once it advances past [`Self::persisted`], at most
    /// once per [`CURSOR_WRITE_INTERVAL`] — except a caught-up (short) page
    /// flushes a pending newer value immediately, so the resting cursor
    /// always lands on disk instead of waiting out the throttle. A failed
    /// write warns and stays pending; the next advance-or-caught-up retries.
    fn maybe_persist(&mut self, since: i64, caught_up: bool) {
        if since == self.persisted {
            return;
        }
        let throttled = !caught_up
            && self
                .last_write
                .is_some_and(|t| t.elapsed() < CURSOR_WRITE_INTERVAL);
        if throttled {
            return;
        }
        if let Err(e) = write_cursor(&self.path, since) {
            warn!(
                error = %e,
                path = %self.path.display(),
                "remote push receipts cursor persist failed"
            );
            return;
        }
        self.persisted = since;
        self.last_write = Some(Instant::now());
    }
}

/// Atomic cursor write: create parent dirs as needed, serialize to
/// `<path>.tmp`, rename over `<path>` — a crash mid-write can never leave a
/// torn state file.
fn write_cursor(path: &Path, since: i64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&CursorDto {
        receipts_since: since,
    })
    .map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    // create_new: never follow a pre-placed symlink at <path>.tmp into
    // clobbering its target (review 2026-08-17 finding 4). A stale .tmp
    // orphan from a crashed earlier write is removed and retried ONCE.
    let write_fresh = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()))
    };
    match write_fresh() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            write_fresh()?;
        }
        Err(e) => return Err(e),
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::router::InMemoryTokenRegistry;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::Json;
    use axum::routing::{get, post};
    use axum::Router;
    use nostos_application::ports::{
        EventSink, PushTables, PushTemplate, SessionCandidate, StoreRejection,
    };
    use nostos_domain::{ReplicationEvent, SessionId, SyncSession};
    use std::collections::{HashMap, HashSet};
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    // ---- test doubles ----

    /// A store double whose only interesting answer is presence (the
    /// router.rs test idiom).
    struct FakeStore {
        online: Mutex<HashSet<String>>,
    }

    #[async_trait]
    impl SessionStore for FakeStore {
        async fn add(&self, _session: SyncSession, _sink: Arc<dyn EventSink>) {}
        async fn try_add_below_cap(
            &self,
            _session: SyncSession,
            _sink: Arc<dyn EventSink>,
            _cap: u64,
        ) -> Result<SessionId, StoreRejection> {
            Ok(SessionId::new())
        }
        async fn remove(&self, _id: SessionId) {}
        async fn candidates_for(&self, _event: &ReplicationEvent) -> Vec<SessionCandidate> {
            Vec::new()
        }
        async fn len(&self) -> usize {
            0
        }
        async fn min_acked_lsn(&self) -> Option<Lsn> {
            None
        }
        async fn account_online(&self, account_id: &str) -> bool {
            self.online.lock().unwrap().contains(account_id)
        }
    }

    #[derive(Clone, Debug)]
    struct RecordedSend {
        auth: Option<String>,
        body: Value,
    }

    /// A recording daemon double: a real spawned axum listener (the
    /// ProviderMock idiom, `push/mod.rs` test_support) that records POST
    /// /v1/send bodies + bearer auth and serves a test-mutable receipt
    /// log with contract-accurate `since` cursor filtering.
    struct MockDaemon {
        addr: SocketAddr,
        sends: Arc<Mutex<Vec<RecordedSend>>>,
        receipts: Arc<Mutex<Vec<Value>>>,
        /// Every `since` value polled, in order — the restart-resume
        /// tests assert on the FIRST poll a fresh notifier makes.
        sinces: Arc<Mutex<Vec<i64>>>,
    }

    impl MockDaemon {
        /// `hang_sends` parks every send handler 30s — the backlog test's
        /// way of sticking the deliver task on one slow POST.
        async fn start(hang_sends: bool) -> Self {
            let sends: Arc<Mutex<Vec<RecordedSend>>> = Arc::new(Mutex::new(Vec::new()));
            let receipts: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
            let sinces: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
            let s1 = Arc::clone(&sends);
            let send_handler = move |headers: HeaderMap, body: String| {
                let sends = Arc::clone(&s1);
                async move {
                    sends.lock().expect("sends").push(RecordedSend {
                        auth: headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        body: serde_json::from_str(&body).unwrap_or(Value::Null),
                    });
                    if hang_sends {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    (
                        StatusCode::ACCEPTED,
                        Json(serde_json::json!({"push_id": "mock", "status": "accepted"})),
                    )
                }
            };
            let r1 = Arc::clone(&receipts);
            let s2 = Arc::clone(&sinces);
            let receipts_handler = move |uri: Uri| {
                let receipts = Arc::clone(&r1);
                let sinces = Arc::clone(&s2);
                async move {
                    let since: i64 = uri
                        .query()
                        .and_then(|q| q.split('&').find(|p| p.starts_with("since=")))
                        .and_then(|p| p.strip_prefix("since="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    sinces.lock().expect("sinces").push(since);
                    let list: Vec<Value> = receipts
                        .lock()
                        .expect("receipts")
                        .iter()
                        .filter(|r| r.get("seq").and_then(Value::as_i64).unwrap_or(0) > since)
                        .cloned()
                        .collect();
                    Json(serde_json::json!({ "receipts": list }))
                }
            };
            let app = Router::new()
                .route("/v1/send", post(send_handler))
                .route("/v1/receipts", get(receipts_handler));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve");
            });
            Self {
                addr,
                sends,
                receipts,
                sinces,
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn sends(&self) -> Vec<RecordedSend> {
            self.sends.lock().expect("sends").clone()
        }

        fn add_receipt(&self, r: Value) {
            self.receipts.lock().expect("receipts").push(r);
        }

        fn sinces(&self) -> Vec<i64> {
            self.sinces.lock().expect("sinces").clone()
        }
    }

    // ---- helpers ----

    fn hint(tenant: &str, account: &str, table: &str, lsn: u64) -> PushHint {
        PushHint {
            table: table.to_string(),
            tenant_id: tenant.to_string(),
            account_id: account.to_string(),
            lsn: Lsn::new(lsn),
            payload: None,
        }
    }

    fn notifier(
        base_url: &str,
        registry: Arc<InMemoryTokenRegistry>,
        online: &[&str],
        config: RouterConfig,
        metrics: Arc<Metrics>,
        state_path: Option<PathBuf>,
    ) -> RemoteNotifier {
        let store: Arc<dyn SessionStore> = Arc::new(FakeStore {
            online: Mutex::new(online.iter().map(|s| (*s).to_string()).collect()),
        });
        RemoteNotifier::new(
            base_url,
            "secret-key",
            registry,
            store,
            config,
            metrics,
            state_path,
        )
    }

    /// Poll (bounded) until `f` holds — the deliver/receipt tasks are async.
    async fn soon(mut f: impl FnMut() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_quiet() {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // ---- the non-blocking contract (plan task 2.1) ----

    /// The daemon is DOWN: notify() returns promptly for every hint (it is
    /// enqueue-only), the worker counts each failed POST, and nothing about
    /// the sync path blocks or fails outward.
    #[tokio::test]
    async fn notify_is_non_blocking_and_survives_daemon_outage() {
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "tok-aaaaaaaaaa", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        // A guaranteed-dead port: bind an ephemeral listener, then drop it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let n = notifier(
            &format!("http://{addr}"),
            registry,
            &[],
            RouterConfig::default(),
            Arc::clone(&metrics),
            None,
        );

        let start = std::time::Instant::now();
        for lsn in 0..50u64 {
            n.notify(hint("t1", "u1", "tasks", lsn)).await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "50 notifies against a dead daemon must be enqueue-only (took {elapsed:?})"
        );

        soon(|| metrics.snapshot().push_failed >= 50).await;
        assert_eq!(
            metrics.snapshot().push_dropped,
            0,
            "50 hints fit the backlog; none dropped"
        );
    }

    /// The bounded backlog: with the deliver task stuck on one slow POST,
    /// hints beyond the channel capacity drop and are counted.
    #[tokio::test]
    async fn notify_counts_drops_when_backlog_is_full() {
        let mock = MockDaemon::start(true).await; // hang every send 30s
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "tok-aaaaaaaaaa", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &[],
            RouterConfig::default(),
            Arc::clone(&metrics),
            None,
        );

        let total = u64::try_from(HINT_CHANNEL_CAPACITY + 64).expect("fits");
        let start = std::time::Instant::now();
        for lsn in 0..total {
            n.notify(hint("t1", "u1", "tasks", lsn)).await; // never blocks
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "a full backlog drops, it never blocks notify (took {elapsed:?})"
        );
        // The worker holds ONE hint in flight; the channel holds
        // HINT_CHANNEL_CAPACITY; everything beyond that dropped on the floor.
        assert!(
            metrics.snapshot().push_dropped >= 62,
            "overflow beyond the bounded backlog is counted: {}",
            metrics.snapshot().push_dropped
        );
    }

    // ---- wire shape ----

    /// The rail-mode request: platform always set, collapse key = table,
    /// metadata {table, lsn (decimal string), account}, silent payload in
    /// the contract's shape; a None category is omitted, not nulled.
    #[test]
    fn send_request_shape_platform_collapse_key_metadata() {
        let dto = SendRequestDto {
            token: "tok-1".to_string(),
            platform: "apns".to_string(),
            payload: PayloadDto::from_push(&super::super::PushPayload::Silent {
                table: "tasks".to_string(),
                lsn: Lsn::new(4242),
            }),
            collapse_key: "tasks".to_string(),
            metadata: metadata_echo("tasks", Lsn::new(4242), "u1"),
        };
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["platform"], serde_json::json!("apns"));
        assert_eq!(v["collapse_key"], serde_json::json!("tasks"));
        assert_eq!(
            v["payload"]["silent"],
            serde_json::json!({"table": "tasks", "lsn": "4242"})
        );
        assert_eq!(
            v["metadata"],
            serde_json::json!({"table": "tasks", "lsn": "4242", "account": "u1"})
        );

        let visible =
            serde_json::to_value(PayloadDto::from_push(&super::super::PushPayload::Visible {
                title: "t".to_string(),
                body: "b".to_string(),
                category: None,
            }))
            .unwrap();
        assert_eq!(
            visible["visible"],
            serde_json::json!({"title": "t", "body": "b"}),
            "None category omitted, not serialized as null"
        );
    }

    // ---- delivery behavior (mock daemon) ----

    /// An account hint ⇒ exactly one POST per registered OFFLINE token,
    /// each with bearer auth, its own platform, and the LSN metadata echo.
    #[tokio::test]
    async fn hint_posts_once_per_offline_token_with_platform_and_metadata() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "tok-off-a", "u1", "t1")
            .await
            .unwrap();
        registry
            .upsert("fcm", "tok-off-b", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &[],
            RouterConfig::default(),
            metrics,
            None,
        );

        n.notify(hint("t1", "u1", "tasks", 4242)).await;
        drop(n); // ends the deliver task after draining pending hints
        soon(|| mock.sends().len() == 2).await;
        wait_quiet().await;

        let sends = mock.sends();
        assert_eq!(sends.len(), 2, "one POST per registered token: {sends:?}");
        let mut platforms = Vec::new();
        for s in &sends {
            assert_eq!(s.auth.as_deref(), Some("Bearer secret-key"));
            assert_eq!(s.body["collapse_key"], serde_json::json!("tasks"));
            assert_eq!(
                s.body["metadata"],
                serde_json::json!({"table": "tasks", "lsn": "4242", "account": "u1"})
            );
            assert_eq!(
                s.body["payload"]["silent"],
                serde_json::json!({"table": "tasks", "lsn": "4242"})
            );
            platforms.push(s.body["platform"].as_str().unwrap_or_default().to_string());
        }
        platforms.sort_unstable();
        assert_eq!(
            platforms,
            vec!["apns".to_string(), "fcm".to_string()],
            "each token carries ITS registry platform (rail mode)"
        );
    }

    /// Tenant-wide hint (empty account, ADR-0037 §1 amendment) expands to
    /// the tenant's accounts — and an ONLINE account gets no POST at all.
    #[tokio::test]
    async fn tenant_wide_hint_skips_online_accounts() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "tok-off", "u-off", "t1")
            .await
            .unwrap();
        registry
            .upsert("fcm", "tok-on", "u-on", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &["u-on"],
            RouterConfig::default(),
            metrics,
            None,
        );

        n.notify(hint("t1", "", "tasks", 7)).await;
        drop(n);
        soon(|| mock.sends().len() == 1).await;
        wait_quiet().await;

        let sends = mock.sends();
        assert_eq!(
            sends.len(),
            1,
            "only the offline account's token: {sends:?}"
        );
        assert_eq!(sends[0].body["token"], serde_json::json!("tok-off"));
        assert_eq!(
            sends[0].body["metadata"]["account"],
            serde_json::json!("u-off")
        );
    }

    /// A visible-configured table interpolates server-side — through the
    /// SAME build_payload seam the embedded router uses.
    #[tokio::test]
    async fn visible_template_interpolates_into_post_body() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry.upsert("apns", "tok-1", "u1", "t1").await.unwrap();
        let tables = PushTables {
            tenant_column: None,
            tables: [(
                "orders".to_string(),
                PushTemplate::Visible {
                    title: "New activity".to_string(),
                    body: "Order {id} changed ({missing})".to_string(),
                    category: Some("ORDER".to_string()),
                },
            )]
            .into_iter()
            .collect(),
        };
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &[],
            RouterConfig {
                tables,
                live_activities: HashMap::new(),
            },
            metrics,
            None,
        );

        let mut h = hint("t1", "u1", "orders", 9);
        h.payload = Some(br#"{"id":"ord-42"}"#.to_vec());
        n.notify(h).await;
        drop(n);
        soon(|| !mock.sends().is_empty()).await;

        let sends = mock.sends();
        assert_eq!(
            sends[0].body["payload"]["visible"],
            serde_json::json!({
                "title": "New activity",
                "body": "Order ord-42 changed ()",
                "category": "ORDER"
            }),
            "{{col}} interpolates; a missing column substitutes empty"
        );
    }

    /// ActivityKit tokens are skipped entirely — they cannot be doorbelled
    /// and the 0.2.0 contract has no content-state send (see the ponytail).
    #[tokio::test]
    async fn activity_tokens_are_skipped_not_doorbelled() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert(PLATFORM_APNS_LIVE_ACTIVITY, "la-1", "u1", "t1")
            .await
            .unwrap();
        registry.upsert("apns", "dev-1", "u1", "t1").await.unwrap();
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &[],
            RouterConfig::default(),
            Arc::clone(&metrics),
            None,
        );

        n.notify(hint("t1", "u1", "tasks", 1)).await;
        drop(n);
        soon(|| !mock.sends().is_empty()).await;
        wait_quiet().await;

        let sends = mock.sends();
        assert_eq!(sends.len(), 1, "only the ordinary device token: {sends:?}");
        assert_eq!(sends[0].body["token"], serde_json::json!("dev-1"));
        assert_eq!(metrics.snapshot().push_failed, 0, "a skip is not a failure");
    }

    /// A live-activity-configured table degrades to the plain doorbell for
    /// ordinary devices (the placeholder Visible must never leak).
    #[tokio::test]
    async fn liveactivity_table_degrades_to_silent_doorbell() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry.upsert("apns", "dev-1", "u1", "t1").await.unwrap();
        let live_activities = [(
            "deliveries".to_string(),
            serde_json::json!({ "status": "{status}" }),
        )]
        .into_iter()
        .collect();
        let metrics = Arc::new(Metrics::new());
        let n = notifier(
            &mock.url(),
            registry,
            &[],
            RouterConfig {
                tables: PushTables::default(),
                live_activities,
            },
            metrics,
            None,
        );

        n.notify(hint("t1", "u1", "deliveries", 5)).await;
        drop(n);
        soon(|| !mock.sends().is_empty()).await;

        let sends = mock.sends();
        assert_eq!(
            sends[0].body["payload"]["silent"],
            serde_json::json!({"table": "deliveries", "lsn": "5"}),
            "ordinary devices get the doorbell, never the placeholder template"
        );
    }

    // ---- receipts (plan task 2.2) ----

    /// The receipt poll flips Metrics exactly like the embedded router's
    /// flush accounting, prunes the LOCAL registry on `unregistered`, keeps
    /// coalesced losers out of the counters (but in the correlation map),
    /// and advances the cursor so later receipts are picked up.
    #[tokio::test]
    async fn receipt_poll_flips_metrics_prunes_and_advances_cursor() {
        let mock = MockDaemon::start(false).await;
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "live-tok", "u1", "t1")
            .await
            .unwrap();
        registry
            .upsert("apns", "dead-tok", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let _n = notifier(
            &mock.url(),
            Arc::clone(&registry),
            &[],
            RouterConfig::default(),
            Arc::clone(&metrics),
            None,
        );

        mock.add_receipt(serde_json::json!({
            "seq": 1, "push_id": "p1", "token": "live-tok", "outcome": "delivered",
            "metadata": {"table": "tasks", "lsn": "4242", "account": "u1"}
        }));
        mock.add_receipt(serde_json::json!({
            "seq": 2, "push_id": "p2", "token": "dead-tok", "outcome": "unregistered"
        }));
        mock.add_receipt(serde_json::json!({
            "seq": 3, "push_id": "p3", "token": "live-tok", "outcome": "fatal", "detail": "boom"
        }));
        mock.add_receipt(serde_json::json!({
            "seq": 4, "push_id": "p4", "token": "live-tok", "outcome": "delivered",
            "detail": "coalesced:p1",
            "metadata": {"table": "tasks", "lsn": "4241", "account": "u1"}
        }));

        soon(|| {
            let s = metrics.snapshot();
            s.push_sent == 1 && s.push_failed == 1 && s.push_pruned == 1
        })
        .await;
        let s = metrics.snapshot();
        assert_eq!(
            s.push_sent, 1,
            "winner delivered; the coalesced loser is not a rail send"
        );
        assert_eq!(s.push_failed, 1);
        assert_eq!(s.push_pruned, 1);
        assert_eq!(
            metrics.push_last_lsn.lock().unwrap().get("u1").copied(),
            Some(4242),
            "correlation map holds the max LSN (loser 4241 never moves it back)"
        );
        assert!(
            registry
                .list_by_account("t1", "u1")
                .await
                .unwrap()
                .iter()
                .all(|t| t.token != "dead-tok"),
            "the unregistered receipt pruned the LOCAL registry row"
        );

        // Cursor advanced: a later receipt is seen, the old ones are not
        // re-applied.
        mock.add_receipt(serde_json::json!({
            "seq": 5, "push_id": "p5", "token": "live-tok", "outcome": "delivered",
            "metadata": {"table": "tasks", "lsn": "4243", "account": "u1"}
        }));
        soon(|| metrics.snapshot().push_sent == 2).await;
        assert_eq!(
            metrics.push_last_lsn.lock().unwrap().get("u1").copied(),
            Some(4243)
        );
    }

    // ---- receipts cursor persistence (restart-resume) ----

    /// A unique state-file path per test: temp dir + pid + atomic counter
    /// (the workspace has no tempfile dep — see the root Cargo.toml).
    fn temp_state_path(label: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nostos-remote-cursor-{}-{label}-{n}.json",
            std::process::id()
        ))
    }

    /// The persisted cursor, parsed — `None` when absent or unparseable.
    fn read_cursor(path: &Path) -> Option<i64> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<CursorDto>(&content)
            .ok()
            .map(|dto| dto.receipts_since)
    }

    /// Restart-resume: the first notifier drains receipts and persists the
    /// cursor; a SECOND notifier on the same state path starts its FIRST
    /// receipts poll at the persisted value — the replayed window is
    /// never re-fetched.
    #[tokio::test]
    async fn receipts_cursor_resumes_across_restart() {
        let path = temp_state_path("resume");
        let _ = std::fs::remove_file(&path);

        // First life: seq 1..=3 already sit in the log when the poll
        // starts, and the caught-up flush persists the advanced cursor.
        let mock = MockDaemon::start(false).await;
        for seq in 1..=3i64 {
            mock.add_receipt(serde_json::json!({
                "seq": seq, "push_id": format!("p{seq}"), "token": "tok-1",
                "outcome": "delivered",
                "metadata": {"table": "tasks", "lsn": "10", "account": "u1"}
            }));
        }
        let n1 = notifier(
            &mock.url(),
            Arc::new(InMemoryTokenRegistry::new()),
            &[],
            RouterConfig::default(),
            Arc::new(Metrics::new()),
            Some(path.clone()),
        );
        soon(|| read_cursor(&path) == Some(3)).await;
        drop(n1); // the receipts task itself runs for the process lifetime

        // Second life against a FRESH daemon log (the daemon kept its
        // receipts; only the server restarted): the first poll must carry
        // since=<persisted>, never since=0.
        let mock2 = MockDaemon::start(false).await;
        for seq in 4..=5i64 {
            mock2.add_receipt(serde_json::json!({
                "seq": seq, "push_id": format!("p{seq}"), "token": "tok-1",
                "outcome": "delivered",
                "metadata": {"table": "tasks", "lsn": "11", "account": "u2"}
            }));
        }
        let metrics = Arc::new(Metrics::new());
        let _n2 = notifier(
            &mock2.url(),
            Arc::new(InMemoryTokenRegistry::new()),
            &[],
            RouterConfig::default(),
            Arc::clone(&metrics),
            Some(path.clone()),
        );
        soon(|| !mock2.sinces().is_empty()).await;
        assert_eq!(
            mock2.sinces()[0],
            3,
            "the first poll after restart starts at the persisted cursor"
        );
        // And the resumed poll works from there: both post-cursor
        // receipts are applied.
        soon(|| metrics.snapshot().push_sent == 2).await;

        let _ = std::fs::remove_file(&path);
    }

    /// Review finding 4 (2026-08-17): a stale `<path>.tmp` orphan (from a
    /// crashed earlier write) is removed and the fresh cursor still lands —
    /// and `create_new` means a pre-placed SYMLINK at the tmp path is
    /// treated as that orphan, never followed into a clobber.
    #[test]
    fn write_cursor_overwrites_a_stale_tmp_orphan() {
        let path = temp_state_path("stale-tmp");
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, "stale").expect("seed stale tmp");
        write_cursor(&path, 42).expect("write over stale tmp");
        assert_eq!(read_cursor(&path), Some(42));
        let _ = std::fs::remove_file(&path);
    }

    /// A corrupt state file ⇒ the first poll starts at since=0 (warn +
    /// fresh, asserted behaviorally) and the next advance self-heals the
    /// file with valid JSON.
    #[tokio::test]
    async fn corrupt_state_file_starts_at_zero_and_self_heals() {
        let path = temp_state_path("corrupt");
        std::fs::write(&path, b"{\"receipts_since\": not-a-number").unwrap();

        let mock = MockDaemon::start(false).await;
        mock.add_receipt(serde_json::json!({
            "seq": 7, "push_id": "p7", "token": "tok-1", "outcome": "delivered",
            "metadata": {"table": "tasks", "lsn": "1", "account": "u1"}
        }));
        let _n = notifier(
            &mock.url(),
            Arc::new(InMemoryTokenRegistry::new()),
            &[],
            RouterConfig::default(),
            Arc::new(Metrics::new()),
            Some(path.clone()),
        );
        soon(|| !mock.sinces().is_empty()).await;
        assert_eq!(
            mock.sinces()[0],
            0,
            "an unparseable cursor file restarts the poll at seq 0"
        );
        soon(|| read_cursor(&path) == Some(7)).await;

        let _ = std::fs::remove_file(&path);
    }

    /// A state path under not-yet-existing directories: the first persist
    /// creates them (`create_dir_all` on the parent).
    #[tokio::test]
    async fn state_file_creates_missing_parent_dirs() {
        let base = temp_state_path("dirs");
        let stem = base.file_stem().expect("stem").to_str().expect("utf-8");
        let dir = std::env::temp_dir().join(format!("{stem}-nested/deeper"));
        let path = dir.join("cursor.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mock = MockDaemon::start(false).await;
        mock.add_receipt(serde_json::json!({
            "seq": 2, "push_id": "p2", "token": "tok-1", "outcome": "delivered",
            "metadata": {"table": "tasks", "lsn": "1", "account": "u1"}
        }));
        let _n = notifier(
            &mock.url(),
            Arc::new(InMemoryTokenRegistry::new()),
            &[],
            RouterConfig::default(),
            Arc::new(Metrics::new()),
            Some(path.clone()),
        );
        soon(|| read_cursor(&path) == Some(2)).await;
        assert!(dir.is_dir(), "the missing parent dirs were created");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
