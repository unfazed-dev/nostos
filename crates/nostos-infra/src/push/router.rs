//! PushRouter — the coalescer/composer over the provider rails (ADR-0037
//! §4, plan task 2.4). This is the piece that turns fan-out hints into
//! provider sends:
//!
//! 1. [`PushNotifier`] impl: `notify` is `try_send` into a bounded channel,
//!    drop-on-full counted in `Metrics` — the same non-blocking contract the
//!    fan-out enqueue site obeys, so a stalled coalescer can never stall
//!    `fan_out`.
//! 2. The background coalescer: per-`(tenant, account)` debounce window
//!    (default 2s, `NOSTOS_PUSH_DEBOUNCE_MS` at the composition root)
//!    collapsing a burst to ONE send per account per window; hints with an
//!    empty `account_id` are tenant-wide (ADR-0037 §1 amendment) and expand
//!    to the tenant's registered tokens.
//! 3. At flush: presence RE-CHECK via [`SessionStore::account_online`]
//!    (application-port access from infra — dependencies point inward, and
//!    infra → application is the legal direction). This closes the
//!    enqueue-race window noted in `fanout.rs`: an account that connected
//!    between enqueue and send is skipped, and accounts that came online
//!    during the window are skipped too.
//! 4. Sends ride the rail set behind the [`PushSink`] seam with a
//!    per-`(device, subscription)` collapse key (the table name — FCM
//!    `collapse_key`, APNs `apns-collapse-id`, Web Push `Topic` supersede a
//!    prior push to the same device for the same subscription).
//!    [`RailOutcome::Unregistered`] prunes the token row; transient outcomes
//!    get ONE deferred retry (doorbell semantics: beyond that, the client's
//!    durable LSN checkpoint reconciles).
//! 5. Live Activity tables (plan task 6.4, experimental): a table listed in
//!    the `live_activities` map sends interpolated content-state updates to
//!    [`PLATFORM_APNS_LIVE_ACTIVITY`] tokens (APNs-only, priority 5) while
//!    ordinary device tokens still get the plain doorbell — the update
//!    repaints the Lock Screen, it does not move the device's LSN.
//!
//! Template resolution lives HERE (per `push/mod.rs`'s handoff note): a
//! visible-configured table's hint carries the event tuple bytes; the
//! coalescer interpolates `{col}` placeholders (static substitution only)
//! before handing the rails an already-built [`PushPayload`]. Row bytes
//! never transit a provider except as the interpolated title/body — the
//! operator's visible-push opt-in (ADR-0037 §2).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostos_application::ports::{
    Metrics, PushHint, PushNotifier, PushTables, PushTemplate, SessionStore,
};
use nostos_domain::{ColumnValue, Lsn};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::warn;

use super::{
    apns::ApnsRail, fcm::FcmRail, webpush::WebPushRail, PushPayload, PushRailError, RailOutcome,
};

/// The registry `platform` value for an ActivityKit push token (plan task
/// 6.4, experimental). A dedicated string — not plain `apns` — because the
/// router must route content-state updates ONLY to activity tokens and must
/// never doorbell them (an activity token cannot wake the app; a
/// non-liveactivity push to it is wire-invalid). The token itself still rides
/// the APNs rail.
pub const PLATFORM_APNS_LIVE_ACTIVITY: &str = "apns-liveactivity";

/// Depth of the router's inbound hint channel. Full ⇒ drop-and-count (the
/// fan-out channel upstream already sheds load the same way).
const ROUTER_CHANNEL_CAPACITY: usize = 1024;
/// Total attempts per flush (1 initial + 1 deferred retry). A doorbell that
/// fails twice is counted failed and abandoned — the next event re-pushes.
const MAX_ATTEMPTS: u8 = 2;
/// How long a transiently-failed entry waits before its one retry.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// One row of the push-token registry, as the router sees it. `tenant_id`
/// and `account_id` are redundant for the per-account lookup but keep one
/// uniform shape for both lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredToken {
    pub tenant_id: String,
    pub account_id: String,
    pub platform: String,
    pub token: String,
}

/// The token-registry seam (ADR-0037 §3). `PgTokenStore` (feature `pg`) is
/// the production implementation; [`InMemoryTokenRegistry`] serves the
/// non-pg/fake dev path and the tests. Upsert semantics migrate a
/// re-registered token to its new owner (see `token_store.rs`).
#[async_trait]
pub trait PushTokenRegistry: Send + Sync {
    /// Register (or re-register) a device token for `(account, tenant)`.
    ///
    /// # Errors
    /// `Err` on a backend failure (mapped to HTTP 5xx by the REST surface).
    async fn upsert(
        &self,
        platform: &str,
        token: &str,
        account_id: &str,
        tenant_id: &str,
    ) -> Result<(), String>;

    /// Unscoped prune: the rail says this token is dead (APNs 410, FCM
    /// `UNREGISTERED`, Web Push 404/410). Returns rows deleted.
    ///
    /// # Errors
    /// `Err` on a backend failure.
    async fn prune(&self, token: &str) -> Result<u64, String>;

    /// Owner-scoped delete (sign-out deregistration): only the
    /// authenticated `(tenant, account)`'s own row disappears — a token that
    /// migrated to another principal is a no-op (0 rows), so one user can
    /// never deregister another's device. Returns rows deleted.
    ///
    /// # Errors
    /// `Err` on a backend failure.
    async fn delete_for_owner(
        &self,
        tenant_id: &str,
        account_id: &str,
        token: &str,
    ) -> Result<u64, String>;

    /// Tokens registered to one account within one tenant.
    ///
    /// # Errors
    /// `Err` on a backend failure.
    async fn list_by_account(
        &self,
        tenant_id: &str,
        account_id: &str,
    ) -> Result<Vec<RegisteredToken>, String>;

    /// Every token registered within one tenant, with its account — the
    /// tenant-wide expansion lookup (ADR-0037 §1 amendment).
    ///
    /// # Errors
    /// `Err` on a backend failure.
    async fn list_by_tenant(&self, tenant_id: &str) -> Result<Vec<RegisteredToken>, String>;
}

/// In-memory [`PushTokenRegistry`] — the fake-replicator / no-`pg` dev path
/// and the test double. Same identity semantics as `PgTokenStore`: the token
/// is the key, so re-registration under a different account migrates the row.
#[derive(Default)]
pub struct InMemoryTokenRegistry {
    rows: std::sync::Mutex<HashMap<String, RegisteredToken>>,
}

impl InMemoryTokenRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PushTokenRegistry for InMemoryTokenRegistry {
    async fn upsert(
        &self,
        platform: &str,
        token: &str,
        account_id: &str,
        tenant_id: &str,
    ) -> Result<(), String> {
        self.rows.lock().expect("registry").insert(
            token.to_string(),
            RegisteredToken {
                tenant_id: tenant_id.to_string(),
                account_id: account_id.to_string(),
                platform: platform.to_string(),
                token: token.to_string(),
            },
        );
        Ok(())
    }

    async fn prune(&self, token: &str) -> Result<u64, String> {
        Ok(self
            .rows
            .lock()
            .expect("registry")
            .remove(token)
            .map_or(0, |_| 1))
    }

    async fn delete_for_owner(
        &self,
        tenant_id: &str,
        account_id: &str,
        token: &str,
    ) -> Result<u64, String> {
        let mut rows = self.rows.lock().expect("registry");
        let owned = rows
            .get(token)
            .is_some_and(|r| r.tenant_id == tenant_id && r.account_id == account_id);
        Ok(u64::from(owned && rows.remove(token).is_some()))
    }

    async fn list_by_account(
        &self,
        tenant_id: &str,
        account_id: &str,
    ) -> Result<Vec<RegisteredToken>, String> {
        Ok(self
            .rows
            .lock()
            .expect("registry")
            .values()
            .filter(|r| r.tenant_id == tenant_id && r.account_id == account_id)
            .cloned()
            .collect())
    }

    async fn list_by_tenant(&self, tenant_id: &str) -> Result<Vec<RegisteredToken>, String> {
        Ok(self
            .rows
            .lock()
            .expect("registry")
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
}

/// The send seam the coalescer drives: one token, one payload, one collapse
/// key. Production = [`RailSet`] (platform dispatch over the three provider
/// rails); tests substitute a recording/failing fake. This is deliberately
/// NOT the application `PushNotifier` port — that one stays the
/// fan-out→coalescer seam; this one is rail-shaped (token-level, with
/// outcome) and lives here next to its two implementations.
#[async_trait]
pub trait PushSink: Send + Sync {
    /// Send one payload to one device token. `collapse_key` is the
    /// per-(device, subscription) supersede key (rail-native: FCM
    /// `collapse_key` / APNs `apns-collapse-id` / Web Push `Topic`).
    async fn send(
        &self,
        platform: &str,
        token: &str,
        collapse_key: &str,
        payload: &PushPayload,
    ) -> RailOutcome;

    /// Send one Live Activity state update (plan task 6.4, experimental).
    /// Only [`PLATFORM_APNS_LIVE_ACTIVITY`] tokens reach this method — APNs
    /// is the only rail with ActivityKit, and `content_state` is not a
    /// [`PushPayload`] variant because the other rails must never see it.
    async fn send_live_activity(
        &self,
        token: &str,
        collapse_key: &str,
        content_state: &Value,
    ) -> RailOutcome;
}

/// The production [`PushSink`]: platform dispatch over the provider rails
/// built from env (`from_env` per rail — `Ok(None)` when unconfigured, per
/// `push/mod.rs`). A platform whose rail is absent maps to `Fatal` — an
/// operator registered tokens for a rail they never configured, and that
/// gap must be visible in `push_failed`, not silent.
pub struct RailSet {
    pub fcm: Option<FcmRail>,
    pub apns: Option<ApnsRail>,
    pub webpush: Option<WebPushRail>,
}

impl RailSet {
    /// Build every rail from env.
    ///
    /// # Errors
    /// [`PushRailError`] when any configured rail fails to construct (bad
    /// credentials / partial config) — the caller refuses to start.
    pub fn from_env() -> Result<Self, PushRailError> {
        Ok(Self {
            fcm: FcmRail::from_env()?,
            apns: ApnsRail::from_env()?,
            webpush: WebPushRail::from_env()?,
        })
    }

    /// True when no rail is configured — push cannot deliver anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fcm.is_none() && self.apns.is_none() && self.webpush.is_none()
    }
}

#[async_trait]
impl PushSink for RailSet {
    async fn send(
        &self,
        platform: &str,
        token: &str,
        collapse_key: &str,
        payload: &PushPayload,
    ) -> RailOutcome {
        match platform {
            "fcm" => match &self.fcm {
                Some(rail) => {
                    rail.send(
                        &super::fcm::FcmTarget::Token(token.to_string()),
                        Some(collapse_key),
                        payload,
                    )
                    .await
                }
                None => RailOutcome::Fatal("no fcm rail configured".into()),
            },
            "apns" => match &self.apns {
                Some(rail) => rail.send(token, Some(collapse_key), payload).await,
                None => RailOutcome::Fatal("no apns rail configured".into()),
            },
            "webpush" => match &self.webpush {
                Some(rail) => rail.send(token, Some(collapse_key), payload).await,
                None => RailOutcome::Fatal("no webpush rail configured".into()),
            },
            other => RailOutcome::Fatal(format!("unknown push platform {other:?}")),
        }
    }

    async fn send_live_activity(
        &self,
        token: &str,
        collapse_key: &str,
        content_state: &Value,
    ) -> RailOutcome {
        match &self.apns {
            Some(rail) => {
                rail.send_live_activity(token, Some(collapse_key), content_state)
                    .await
            }
            None => {
                RailOutcome::Fatal("no apns rail configured (live activities ride APNs)".into())
            }
        }
    }
}

/// Static per-table config the coalescer resolves sends from: the
/// application-layer `PushTables` (doorbell/visible templates) plus the Live
/// Activity content-state templates (plan task 6.4, experimental). Grouped
/// so the spawned consumer carries one config value.
#[derive(Debug, Clone, Default)]
pub struct RouterConfig {
    /// Doorbell + visible-notification templates (ADR-0037 §1/§2).
    pub tables: PushTables,
    /// table → ActivityKit `content-state` template. A table present here
    /// sends priority-5 `event:"update"` pushes to
    /// [`PLATFORM_APNS_LIVE_ACTIVITY`] tokens instead of a visible render.
    pub live_activities: HashMap<String, Value>,
}

/// The coalescer — the application `PushNotifier` port implementation wired
/// into `FanOutService::with_push_notifier`. Constructing it spawns the
/// background consumer; dropping every clone of the fan-out side's sender
/// drains pending debounces and ends the task.
pub struct PushRouter {
    tx: mpsc::Sender<PushHint>,
    metrics: Arc<Metrics>,
}

impl PushRouter {
    /// Compose the router. Must be called inside a tokio runtime (it spawns
    /// the coalescer task).
    #[must_use]
    pub fn new(
        sink: Arc<dyn PushSink>,
        registry: Arc<dyn PushTokenRegistry>,
        store: Arc<dyn SessionStore>,
        config: RouterConfig,
        debounce: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(ROUTER_CHANNEL_CAPACITY);
        tokio::spawn(coalesce(
            rx,
            sink,
            registry,
            store,
            config,
            debounce,
            metrics.clone(),
        ));
        Self { tx, metrics }
    }
}

#[async_trait]
impl PushNotifier for PushRouter {
    async fn notify(&self, hint: PushHint) {
        use std::sync::atomic::Ordering;
        if self.tx.try_send(hint).is_err() {
            // Full (or consumer gone): same failure class as the fan-out
            // channel's drop-on-full — count it in the same counter.
            self.metrics.push_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One debounced send target. The FIRST hint in a window fixes the deadline
/// (fixed look-back window); later hints in the window only refresh the
/// content (latest table/lsn/payload wins) — a steady stream must not push
/// the flush out forever (debounce-vs-throttle).
struct Pending {
    table: String,
    lsn: Lsn,
    payload: Option<Vec<u8>>,
    deadline: Instant,
    attempts: u8,
}

async fn coalesce(
    mut rx: mpsc::Receiver<PushHint>,
    sink: Arc<dyn PushSink>,
    registry: Arc<dyn PushTokenRegistry>,
    store: Arc<dyn SessionStore>,
    config: RouterConfig,
    debounce: Duration,
    metrics: Arc<Metrics>,
) {
    let mut pending: HashMap<(String, String), Pending> = HashMap::new();
    loop {
        // Recomputed every iteration: min over pending deadlines (the map is
        // bounded by accounts active in one window — push-scoped, never
        // fan-out-scoped).
        let next = pending.values().map(|p| p.deadline).min();
        tokio::select! {
            maybe = rx.recv() => {
                if let Some(hint) = maybe {
                    absorb(hint, &mut pending, &registry, debounce).await;
                } else {
                    // All senders dropped: final drain, then end the task.
                    flush(&mut pending, &sink, &registry, &store, &config, &metrics).await;
                    return;
                }
            }
            () = sleep_until(next) => {
                flush(&mut pending, &sink, &registry, &store, &config, &metrics).await;
            }
        }
    }
}

/// `None` parks forever (only the recv arm can fire); `Some(d)` sleeps to
/// the earliest pending deadline.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
        None => std::future::pending().await,
    }
}

/// Absorb one hint into the debounce map, expanding tenant-wide hints
/// (empty `account_id`) to the tenant's registered accounts.
async fn absorb(
    hint: PushHint,
    pending: &mut HashMap<(String, String), Pending>,
    registry: &Arc<dyn PushTokenRegistry>,
    debounce: Duration,
) {
    if hint.account_id.is_empty() {
        // ADR-0037 §1 amendment — fully-offline expansion. The per-account
        // token list is re-read at flush; this pass only maps tenant →
        // candidate accounts.
        match registry.list_by_tenant(&hint.tenant_id).await {
            Ok(tokens) => {
                for t in tokens {
                    upsert_pending(
                        pending,
                        (t.tenant_id.clone(), t.account_id.clone()),
                        &hint,
                        debounce,
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, tenant = %hint.tenant_id, "push tenant expansion lookup failed");
            }
        }
    } else {
        upsert_pending(
            pending,
            (hint.tenant_id.clone(), hint.account_id.clone()),
            &hint,
            debounce,
        );
    }
}

fn upsert_pending(
    pending: &mut HashMap<(String, String), Pending>,
    key: (String, String),
    hint: &PushHint,
    debounce: Duration,
) {
    match pending.get_mut(&key) {
        Some(p) => {
            p.table.clone_from(&hint.table);
            p.lsn = hint.lsn;
            p.payload.clone_from(&hint.payload);
            // deadline deliberately untouched — see `Pending`'s doc.
        }
        None => {
            pending.insert(
                key,
                Pending {
                    table: hint.table.clone(),
                    lsn: hint.lsn,
                    payload: hint.payload.clone(),
                    deadline: Instant::now() + debounce,
                    attempts: 0,
                },
            );
        }
    }
}

/// Send every due (and, at shutdown, every pending) entry: presence
/// re-check, token list, template resolution, rail sends, prune/retry.
async fn flush(
    pending: &mut HashMap<(String, String), Pending>,
    sink: &Arc<dyn PushSink>,
    registry: &Arc<dyn PushTokenRegistry>,
    store: &Arc<dyn SessionStore>,
    config: &RouterConfig,
    metrics: &Arc<Metrics>,
) {
    use std::sync::atomic::Ordering;
    let now = Instant::now();
    let due: Vec<(String, String)> = pending
        .iter()
        .filter(|(_, p)| p.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for key in due {
        let Some(mut p) = pending.remove(&key) else {
            continue;
        };
        // Presence re-check at SEND time (ADR-0037 §4) — closes the
        // enqueue-race window and suppresses expansion hits that came online
        // during the window. Store membership is presence; `Dropped` sinks
        // are still online.
        if store.account_online(&key.1).await {
            continue;
        }
        let tokens = match registry.list_by_account(&key.0, &key.1).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, tenant = %key.0, account = %key.1, "push token lookup failed");
                metrics.push_failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if tokens.is_empty() {
            continue;
        }
        // Live Activity tables (plan 6.4): interpolate the content-state from
        // the latest hint's tuple bytes; ordinary devices still get the plain
        // doorbell (they must sync too — the activity update only repaints
        // the Lock Screen, it does not move the device's LSN).
        let live = config
            .live_activities
            .get(&p.table)
            .map(|tpl| interpolate_state(tpl, p.payload.as_deref()));
        let payload = if live.is_some() {
            PushPayload::Silent {
                table: p.table.clone(),
                lsn: p.lsn,
            }
        } else {
            build_payload(config, &p.table, p.lsn, p.payload.as_deref())
        };
        metrics.record_push_lsn(&key.1, p.lsn.raw());
        // One deferred retry per transient outcome (doorbell semantics: two
        // failures ⇒ count failed and abandon; the next event re-pushes).
        let mut retry = false;
        for t in &tokens {
            // Collapse key = the subscription's table: rail-native supersede
            // per (device, subscription) — a newer doorbell replaces an
            // undelivered older one on the same device+table.
            let outcome = if t.platform == PLATFORM_APNS_LIVE_ACTIVITY {
                // An ActivityKit token is not a device token: it renders
                // state updates only and can never be doorbelled — a
                // non-liveactivity table skips it entirely.
                match &live {
                    Some(state) => sink.send_live_activity(&t.token, &p.table, state).await,
                    None => continue,
                }
            } else {
                sink.send(&t.platform, &t.token, &p.table, &payload).await
            };
            match outcome {
                RailOutcome::Delivered => {
                    metrics.push_sent.fetch_add(1, Ordering::Relaxed);
                }
                RailOutcome::Unregistered => match registry.prune(&t.token).await {
                    Ok(n) => {
                        metrics.push_pruned.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(e) => warn!(error = %e, "push token prune failed"),
                },
                RailOutcome::TransientRetryable if p.attempts + 1 < MAX_ATTEMPTS => {
                    retry = true;
                }
                // Attempts exhausted (or no retry possible): count failed.
                RailOutcome::TransientRetryable => {
                    metrics.push_failed.fetch_add(1, Ordering::Relaxed);
                }
                RailOutcome::Fatal(msg) => {
                    warn!(%msg, platform = %t.platform, "push send failed fatally");
                    metrics.push_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if retry {
            // Re-debounce the whole entry (a retry re-sends to every token —
            // doorbells are idempotent and collapse keys supersede).
            p.attempts += 1;
            p.deadline = Instant::now() + RETRY_DELAY;
            pending.insert(key, p);
        }
    }
}

/// Resolve the send payload from the per-table config: a visible template
/// interpolates `{col}` placeholders from the hint's tuple bytes; anything
/// else is a content-free silent doorbell (ADR-0037 §2). Live Activity
/// tables are resolved by the caller (`interpolate_state`) BEFORE this — a
/// `liveactivity` table's `PushTables` row is a placeholder `Visible` that
/// exists only so fan-out attaches tuple bytes (see `parse_push_tables`).
fn build_payload(
    config: &RouterConfig,
    table: &str,
    lsn: Lsn,
    payload: Option<&[u8]>,
) -> PushPayload {
    match config.tables.get(table) {
        Some(PushTemplate::Visible { title, body }) => PushPayload::Visible {
            title: interpolate(title, payload),
            body: interpolate(body, payload),
        },
        _ => PushPayload::Silent {
            table: table.to_string(),
            lsn,
        },
    }
}

/// Static `{col}` substitution over a template. A missing/absent column
/// substitutes the empty string (never the raw placeholder — a leaked
/// `{col}` in a notification reads like a bug). No expression language.
fn interpolate(template: &str, payload: Option<&[u8]>) -> String {
    let get = payload.and_then(crate::replicator::extract_json_column);
    interpolate_with(template, &|col| get.as_ref().and_then(|f| f(col)))
}

/// The `{col}` scanner behind both string templates and content-state leaves.
fn interpolate_with(template: &str, get: &dyn Fn(&str) -> Option<ColumnValue>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('}') {
            let col = &after[..close];
            out.push_str(&column_to_str(get(col)));
            rest = &after[close + 1..];
        } else {
            // Unterminated '{' — emit literally and stop.
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Interpolate `{col}` placeholders in every STRING leaf of a Live Activity
/// content-state template (plan 6.4). ActivityKit decodes `content-state`
/// into the app's `Activity.ContentState` type, so nested objects/arrays are
/// legal; non-string leaves pass through untouched. Same static-substitution
/// rules as [`interpolate`].
fn interpolate_state(template: &Value, payload: Option<&[u8]>) -> Value {
    fn walk(v: &Value, get: &dyn Fn(&str) -> Option<ColumnValue>) -> Value {
        match v {
            Value::String(s) => Value::String(interpolate_with(s, get)),
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, get)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), walk(x, get))).collect())
            }
            other => other.clone(),
        }
    }
    let get = payload.and_then(crate::replicator::extract_json_column);
    walk(template, &|col| get.as_ref().and_then(|f| f(col)))
}

fn column_to_str(v: Option<ColumnValue>) -> String {
    match v {
        Some(ColumnValue::Text(s)) => s,
        Some(ColumnValue::Number(n)) => n.to_string(),
        Some(ColumnValue::Float(f)) => f.to_string(),
        Some(ColumnValue::Bool(b)) => b.to_string(),
        Some(ColumnValue::Any) | None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_application::ports::SessionCandidate;
    use nostos_domain::{SessionId, SyncSession};
    use std::sync::Mutex;

    // ---- test doubles ----

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Sent {
        platform: String,
        token: String,
        collapse_key: String,
        payload: PushPayload,
    }

    /// A sink that records sends and replays a configurable outcome.
    struct FakeSink {
        sends: Mutex<Vec<Sent>>,
        live_sends: Mutex<Vec<(String, String, Value)>>, // (token, collapse_key, state)
        outcome: RailOutcome,
    }

    impl FakeSink {
        fn delivered() -> Arc<Self> {
            Arc::new(Self {
                sends: Mutex::new(Vec::new()),
                live_sends: Mutex::new(Vec::new()),
                outcome: RailOutcome::Delivered,
            })
        }

        fn with_outcome(outcome: RailOutcome) -> Arc<Self> {
            Arc::new(Self {
                sends: Mutex::new(Vec::new()),
                live_sends: Mutex::new(Vec::new()),
                outcome,
            })
        }

        fn sent(&self) -> Vec<Sent> {
            self.sends.lock().unwrap().clone()
        }

        fn live_sent(&self) -> Vec<(String, String, Value)> {
            self.live_sends.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PushSink for FakeSink {
        async fn send(
            &self,
            platform: &str,
            token: &str,
            collapse_key: &str,
            payload: &PushPayload,
        ) -> RailOutcome {
            self.sends.lock().unwrap().push(Sent {
                platform: platform.to_string(),
                token: token.to_string(),
                collapse_key: collapse_key.to_string(),
                payload: payload.clone(),
            });
            self.outcome.clone()
        }

        async fn send_live_activity(
            &self,
            token: &str,
            collapse_key: &str,
            content_state: &Value,
        ) -> RailOutcome {
            self.live_sends.lock().unwrap().push((
                token.to_string(),
                collapse_key.to_string(),
                content_state.clone(),
            ));
            self.outcome.clone()
        }
    }

    /// A store double whose only interesting answer is presence.
    struct FakeStore {
        online: Mutex<std::collections::HashSet<String>>,
    }

    #[async_trait]
    impl SessionStore for FakeStore {
        async fn add(
            &self,
            _session: SyncSession,
            _sink: Arc<dyn nostos_application::ports::EventSink>,
        ) {
        }
        async fn try_add_below_cap(
            &self,
            _session: SyncSession,
            _sink: Arc<dyn nostos_application::ports::EventSink>,
            _cap: u64,
        ) -> Result<SessionId, nostos_application::ports::StoreRejection> {
            Ok(SessionId::new())
        }
        async fn remove(&self, _id: SessionId) {}
        async fn candidates_for(
            &self,
            _event: &nostos_domain::ReplicationEvent,
        ) -> Vec<SessionCandidate> {
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

    fn config(tables: Vec<(&str, PushTemplate)>) -> PushTables {
        PushTables {
            tenant_column: None,
            tables: tables
                .into_iter()
                .map(|(t, tpl)| (t.to_string(), tpl))
                .collect(),
        }
    }

    fn router(
        sink: Arc<dyn PushSink>,
        registry: Arc<InMemoryTokenRegistry>,
        online: &[&str],
        tables: Vec<(&str, PushTemplate)>,
        metrics: Arc<Metrics>,
    ) -> PushRouter {
        router_with_live(sink, registry, online, tables, HashMap::new(), metrics)
    }

    fn router_with_live(
        sink: Arc<dyn PushSink>,
        registry: Arc<InMemoryTokenRegistry>,
        online: &[&str],
        tables: Vec<(&str, PushTemplate)>,
        live_activities: HashMap<String, Value>,
        metrics: Arc<Metrics>,
    ) -> PushRouter {
        let store: Arc<dyn SessionStore> = Arc::new(FakeStore {
            online: Mutex::new(online.iter().map(|s| (*s).to_string()).collect()),
        });
        PushRouter::new(
            sink,
            registry,
            store,
            RouterConfig {
                tables: config(tables),
                live_activities,
            },
            Duration::from_millis(40),
            metrics,
        )
    }

    /// Poll (bounded) until `f` holds — the coalescer's flushes are async.
    async fn soon(mut f: impl FnMut() -> bool) {
        for _ in 0..250 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    }

    async fn wait_quiet() {
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    fn hint(tenant: &str, account: &str, table: &str, lsn: u64) -> PushHint {
        PushHint {
            table: table.to_string(),
            tenant_id: tenant.to_string(),
            account_id: account.to_string(),
            lsn: Lsn::new(lsn),
            payload: None,
        }
    }

    async fn registry_with(token: &str, account: &str) -> Arc<InMemoryTokenRegistry> {
        let reg = Arc::new(InMemoryTokenRegistry::new());
        reg.upsert("apns", token, account, "t1").await.unwrap();
        reg
    }

    #[tokio::test]
    async fn burst_collapses_to_one_push_with_latest_lsn() {
        let sink = FakeSink::delivered();
        let reg = registry_with("dev-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &[], vec![], metrics.clone());

        for lsn in 1..=100 {
            router.notify(hint("t1", "u1", "tasks", lsn)).await;
        }

        soon(|| !sink.sent().is_empty()).await;
        wait_quiet().await;
        let sent = sink.sent();
        assert_eq!(sent.len(), 1, "100-event burst ⇒ exactly one push");
        assert_eq!(sent[0].token, "dev-1");
        assert_eq!(
            sent[0].payload,
            PushPayload::Silent {
                table: "tasks".into(),
                lsn: Lsn::new(100)
            },
            "latest hint in the window wins"
        );
        assert_eq!(sent[0].collapse_key, "tasks");
        assert_eq!(metrics.snapshot().push_sent, 1);
        assert_eq!(
            metrics.push_last_lsn.lock().unwrap().get("u1").copied(),
            Some(100),
            "last-pushed-LSN correlation map is updated"
        );
    }

    #[tokio::test]
    async fn online_account_is_suppressed_at_send_time() {
        let sink = FakeSink::delivered();
        let reg = registry_with("dev-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &["u1"], vec![], metrics);

        router.notify(hint("t1", "u1", "tasks", 1)).await;
        wait_quiet().await;
        assert!(
            sink.sent().is_empty(),
            "an online account gets the data over its socket — a push would double-signal it"
        );
    }

    #[tokio::test]
    async fn tenant_wide_hint_expands_to_offline_accounts_only() {
        let sink = FakeSink::delivered();
        let reg = Arc::new(InMemoryTokenRegistry::new());
        reg.upsert("apns", "dev-off", "u-off", "t1").await.unwrap();
        reg.upsert("fcm", "dev-on", "u-on", "t1").await.unwrap();
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &["u-on"], vec![], metrics);

        // Tenant-wide marker: empty account_id (ADR-0037 §1 amendment).
        router.notify(hint("t1", "", "tasks", 7)).await;

        soon(|| !sink.sent().is_empty()).await;
        wait_quiet().await;
        let sent = sink.sent();
        assert_eq!(sent.len(), 1, "only the offline account's token is pushed");
        assert_eq!(sent[0].token, "dev-off");
    }

    #[tokio::test]
    async fn unregistered_outcome_prunes_the_token_row() {
        let sink = FakeSink::with_outcome(RailOutcome::Unregistered);
        let reg = registry_with("dead-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg.clone(), &[], vec![], metrics.clone());

        router.notify(hint("t1", "u1", "tasks", 1)).await;
        soon(|| !sink.sent().is_empty()).await;
        wait_quiet().await;

        assert!(
            reg.list_by_account("t1", "u1").await.unwrap().is_empty(),
            "the 410/UNREGISTERED token row must be pruned"
        );
        assert_eq!(metrics.snapshot().push_pruned, 1);
    }

    #[tokio::test]
    async fn transient_failure_retries_once_then_counts_failed() {
        let sink = FakeSink::with_outcome(RailOutcome::TransientRetryable);
        let reg = registry_with("dev-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &[], vec![], metrics.clone());

        router.notify(hint("t1", "u1", "tasks", 1)).await;
        soon(|| sink.sent().len() == 2).await;
        wait_quiet().await;
        assert_eq!(sink.sent().len(), 2, "one initial + one deferred retry");
        assert_eq!(metrics.snapshot().push_failed, 1);
        assert_eq!(metrics.snapshot().push_sent, 0);
    }

    #[tokio::test]
    async fn visible_template_interpolates_from_payload() {
        let sink = FakeSink::delivered();
        let reg = registry_with("dev-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(
            sink.clone(),
            reg,
            &[],
            vec![(
                "orders",
                PushTemplate::Visible {
                    title: "New activity".into(),
                    body: "Order {id} changed ({missing})".into(),
                },
            )],
            metrics,
        );

        let mut h = hint("t1", "u1", "orders", 9);
        h.payload = Some(br#"{"id":"ord-42"}"#.to_vec());
        router.notify(h).await;

        soon(|| !sink.sent().is_empty()).await;
        let sent = sink.sent();
        assert_eq!(
            sent[0].payload,
            PushPayload::Visible {
                title: "New activity".into(),
                body: "Order ord-42 changed ()".into(),
            },
            "{{col}} interpolates; a missing column substitutes empty"
        );
    }

    #[tokio::test]
    async fn liveactivity_table_updates_activity_token_and_doorbells_devices() {
        let sink = FakeSink::delivered();
        let reg = Arc::new(InMemoryTokenRegistry::new());
        reg.upsert("apns", "dev-1", "u1", "t1").await.unwrap();
        reg.upsert(PLATFORM_APNS_LIVE_ACTIVITY, "la-1", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let live: HashMap<String, Value> = [(
            "deliveries".to_string(),
            serde_json::json!({ "status": "{status}", "eta_min": "{eta_min}" }),
        )]
        .into_iter()
        .collect();
        // The Visible placeholder mirrors what parse_push_tables enters for a
        // liveactivity table so fan-out attaches tuple bytes.
        let router = router_with_live(
            sink.clone(),
            reg,
            &[],
            vec![(
                "deliveries",
                PushTemplate::Visible {
                    title: String::new(),
                    body: String::new(),
                },
            )],
            live,
            metrics.clone(),
        );

        let mut h = hint("t1", "u1", "deliveries", 5);
        h.payload = Some(br#"{"status":"courier_assigned","eta_min":12}"#.to_vec());
        router.notify(h).await;

        soon(|| !sink.live_sent().is_empty() && !sink.sent().is_empty()).await;
        wait_quiet().await;
        // The activity token gets the interpolated content-state update…
        assert_eq!(
            sink.live_sent(),
            vec![(
                "la-1".to_string(),
                "deliveries".to_string(),
                serde_json::json!({ "status": "courier_assigned", "eta_min": "12" })
            )],
            "string leaves interpolate; numeric column values stringify"
        );
        // …and ordinary devices still get the plain doorbell (the update
        // repaints the Lock Screen, it does not move the device's LSN).
        let sent = sink.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].token, "dev-1");
        assert!(matches!(sent[0].payload, PushPayload::Silent { .. }));
        assert_eq!(metrics.snapshot().push_sent, 2);
    }

    #[tokio::test]
    async fn activity_tokens_never_receive_doorbell_payloads() {
        // An ActivityKit token cannot wake the app — a non-liveactivity
        // table's doorbell must skip it entirely (no send, no failure count).
        let sink = FakeSink::delivered();
        let reg = Arc::new(InMemoryTokenRegistry::new());
        reg.upsert(PLATFORM_APNS_LIVE_ACTIVITY, "la-1", "u1", "t1")
            .await
            .unwrap();
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &[], vec![], metrics.clone());

        router.notify(hint("t1", "u1", "tasks", 1)).await;
        wait_quiet().await;
        assert!(sink.sent().is_empty());
        assert!(sink.live_sent().is_empty());
        assert_eq!(metrics.snapshot().push_failed, 0);
    }

    #[tokio::test]
    async fn unconfigured_table_stays_silent_even_with_payload_attached() {
        // A hint may carry tuple bytes (fan-out attaches them for visible
        // tables); if the config no longer lists the table, the payload must
        // NOT leak — the doorbell is silent.
        let sink = FakeSink::delivered();
        let reg = registry_with("dev-1", "u1").await;
        let metrics = Arc::new(Metrics::new());
        let router = router(sink.clone(), reg, &[], vec![], metrics);

        let mut h = hint("t1", "u1", "orders", 9);
        h.payload = Some(br#"{"id":"ord-42"}"#.to_vec());
        router.notify(h).await;

        soon(|| !sink.sent().is_empty()).await;
        assert!(matches!(sink.sent()[0].payload, PushPayload::Silent { .. }));
    }

    /// Directly pin the interpolation edge cases (no router needed).
    #[test]
    fn interpolate_handles_adjacent_missing_and_literal_braces() {
        let payload = br#"{"a":"x","n":5,"t":true}"#.as_slice();
        assert_eq!(interpolate("{a}-{n}-{t}", Some(payload)), "x-5-true");
        assert_eq!(
            interpolate("no placeholders", Some(payload)),
            "no placeholders"
        );
        assert_eq!(interpolate("{missing}", Some(payload)), "");
        assert_eq!(interpolate("{missing}", None), "");
        assert_eq!(interpolate("open { brace", Some(payload)), "open { brace");
    }

    /// Content-state interpolation walks nested string leaves only.
    #[test]
    fn interpolate_state_walks_nested_leaves_only() {
        let tpl =
            serde_json::json!({ "a": "{a}", "n": 5, "nest": { "b": "{missing}" }, "arr": ["{a}"] });
        let payload = br#"{"a":"x"}"#.as_slice();
        assert_eq!(
            interpolate_state(&tpl, Some(payload)),
            serde_json::json!({ "a": "x", "n": 5, "nest": { "b": "" }, "arr": ["x"] })
        );
        assert_eq!(
            interpolate_state(&tpl, None),
            serde_json::json!({ "a": "", "n": 5, "nest": { "b": "" }, "arr": [""] })
        );
    }
}
