//! REST surface — the OpenAPI contract at docs/api/nostos-pushd.yaml
//! (version 0.3.0, bumped by the 2026-08-17 security-audit closeout, plan
//! task 4.1), implemented EXACTLY (plan task 1.4 for tokens, 1.5 for send;
//! receipts per pin 0.4). Route discipline mirrors nostos-server
//! push_api.rs: deny_unknown_fields DTOs, auth ordering handled by
//! middleware before any handler runs, owner-scoped deletes, and the
//! {"error": string} error shape on every non-2xx this crate produces.
//!
//! Audit closeout behaviors (plan task 4.1): rail-mode dispatch requires a
//! Rail-role key (403 otherwise — finding 1); /v1/send is rate-limited per
//! tenant (429) and field-capped (400 — finding 2); cross-tenant token
//! registration is a 409 (finding 3); /v1/healthz is status-only with the
//! rails booleans behind authed /v1/status (finding 5); DELETE is 204 for
//! every not-yours case (finding 6).
//!
//! Bodies are parsed from raw Bytes (not the Json extractor) so a
//! malformed body gets THIS crate's 400 error shape, not axum's default
//! rejection body.
// ponytail: clippy 1.98 `result_large_err` flags every handler because ApiError
// is an (StatusCode, HeaderMap, Json<Value>) tuple (~200 B). Each Err is built
// once and moved straight into the axum response; boxing it buys nothing.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Json;
use axum::routing::{delete, get, post};
use axum::Router;
use nostos_domain::Lsn;
use nostos_infra::push::{PushData, PushPayload};
use tokio::sync::mpsc;

use crate::auth::{auth_middleware, ApiKeys, KeyRole, TenantId};
use crate::coalescer::{SendJob, SharedPendingGate};
use crate::limit::SendRateLimiter;
use crate::rail::Rails;
use crate::store::{Platform, Store, UpsertOutcome};

/// Shared route state. Cloned per request; cheap (Arcs + a channel sender).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub rails: Rails,
    pub api_keys: ApiKeys,
    /// Bounded coalescer inbox — try_send only.
    pub sender: mpsc::Sender<SendJob>,
    /// Per-tenant token bucket for /v1/send (audit finding 2) — 429 when
    /// exhausted. Arc: the map mutates under its own lock.
    pub send_limiter: Arc<SendRateLimiter>,
    /// The coalescer's pending-key admission gate (audit finding 2) — 429
    /// when a NEW (tenant, token) key would exceed the ceiling.
    pub gate: SharedPendingGate,
}

/// The contract's error body everywhere: {"error": string}, plus an
/// (almost always empty) header map — the one header ever set today is
/// `Retry-After` on limiter 429s (B2).
type ApiError = (StatusCode, HeaderMap, Json<serde_json::Value>);

fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    (
        status,
        HeaderMap::new(),
        Json(serde_json::json!({ "error": message.into() })),
    )
}

/// A 429 with the standard `Retry-After` header (seconds) — the limiter
/// derives it from the tenant's bucket deficit (B2).
fn err_retry_after(message: impl Into<String>, retry_after_secs: u64) -> ApiError {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        headers.insert(header::RETRY_AFTER, v);
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        Json(serde_json::json!({ "error": message.into() })),
    )
}

fn internal(message: &str) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Wire the whole router: /v1/healthz unauthenticated, every other /v1
/// route behind the bearer middleware (which stamps the tenant + role).
pub fn build_router(state: AppState) -> Router {
    let open = Router::new()
        .route("/v1/healthz", get(healthz))
        .with_state(state.clone());
    let authed = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/tokens", post(register_token))
        .route("/v1/tokens/:token", delete(delete_token))
        .route("/v1/send", post(send))
        .route("/v1/send/batch", post(send_batch))
        .route("/v1/receipts", get(receipts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    open.merge(authed)
}

// --------------------------------------------------------- healthz / status

#[derive(serde::Serialize)]
struct HealthzBody {
    status: &'static str,
}

/// GET /v1/healthz — unauthenticated LIVENESS only (audit finding 5): the
/// response is {"status":"ok"} and nothing else. Which rails are
/// configured is deployment detail an anonymous caller must not learn; it
/// moved to the authenticated GET /v1/status.
async fn healthz(State(_state): State<AppState>) -> Json<HealthzBody> {
    Json(HealthzBody { status: "ok" })
}

#[derive(serde::Serialize)]
struct StatusBody {
    status: &'static str,
    rails: crate::rail::RailsHealth,
}

/// GET /v1/status — authenticated readiness (audit finding 5): liveness
/// plus which rails from_env() built — the pre-0.3.0 healthz shape, now
/// behind the bearer gate.
async fn status(State(state): State<AppState>) -> Json<StatusBody> {
    Json(StatusBody {
        status: "ok",
        rails: state.rails.health(),
    })
}

// ----------------------------------------------------------------- tokens

/// POST /v1/tokens body — deny_unknown_fields IS the client-attestation
/// rejection (a tenant field a client sends is a 400 before any row
/// exists; the tenant is stamped by the middleware).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRegistration {
    token: String,
    platform: Platform,
    account_tag: Option<String>,
}

#[derive(serde::Serialize)]
struct TokenRegisteredBody {
    registered: bool,
}

/// Contract minLength: 8.
const MIN_TOKEN_LEN: usize = 8;
/// Sanity cap (the nostos-server push_api cap — anything past this is
/// garbage, not a token).
const MAX_TOKEN_LEN: usize = 2048;

/// POST /v1/tokens — register (upsert) the caller's device token. A token
/// already held by ANOTHER tenant is a 409 (audit finding 3): ownership
/// never silently reassigns — the migration path is the old owner's
/// DELETE, then this POST.
async fn register_token(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    raw: Bytes,
) -> Result<(StatusCode, Json<TokenRegisteredBody>), ApiError> {
    let body: TokenRegistration = serde_json::from_slice(&raw)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;
    let token = body.token.trim();
    if token.len() < MIN_TOKEN_LEN || token.len() > MAX_TOKEN_LEN {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("token must be {MIN_TOKEN_LEN}..={MAX_TOKEN_LEN} chars"),
        ));
    }
    match state
        .store
        .upsert_token(&tenant.0, token, body.platform, body.account_tag.as_deref())
        .await
    {
        Ok(UpsertOutcome::Registered) => Ok((
            StatusCode::CREATED,
            Json(TokenRegisteredBody { registered: true }),
        )),
        Ok(UpsertOutcome::Conflict) => Err(err(
            StatusCode::CONFLICT,
            "token is already registered to another tenant — its owner must DELETE it first",
        )),
        Err(e) => {
            tracing::error!(error = %e, "token upsert failed");
            Err(internal("token registry unavailable"))
        }
    }
}

/// DELETE /v1/tokens/{token} — deregister, idempotent 204. Not-yours is
/// ALSO a 204 (audit finding 6): distinguishing foreign from missing was a
/// token-existence oracle for other tenants, so the 404 is gone — the
/// response confirms nothing the caller did not already know.
async fn delete_token(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Path(token): Path<String>,
) -> Result<StatusCode, ApiError> {
    use crate::store::DeleteOutcome;
    match state
        .store
        .delete_token_owner_scoped(&tenant.0, &token)
        .await
    {
        // Foreign and Missing are BOTH 204 now — see the handler doc.
        Ok(DeleteOutcome::Deleted | DeleteOutcome::Foreign | DeleteOutcome::Missing) => {
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            tracing::error!(error = %e, "token delete failed");
            Err(internal("token registry unavailable"))
        }
    }
}

// ------------------------------------------------------------------- send

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SilentBody {
    table: String,
    /// LSN as a decimal string (contract).
    lsn: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VisibleBody {
    title: String,
    body: String,
    category: Option<String>,
    /// Routing keys handed to the app on tap — `{"nostos_route":
    /// "/orders/42"}` is the shape that makes a notification open the thing
    /// it is about (ADR-0037 §2 amendment). Absent = an unroutable push,
    /// which is what every caller sent before this field existed.
    #[serde(default)]
    data: PushData,
    /// Presentation options (ADR-0047) — image, subtitle, collapse,
    /// interruption level…, validated by `nostos_infra::push::validate_options`.
    #[serde(default)]
    options: nostos_infra::push::PushOptions,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SilentPayload {
    silent: SilentBody,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VisiblePayload {
    visible: VisibleBody,
}

/// oneOf silent | visible — untagged; the disjoint top-level keys keep the
/// variants unambiguous, and deny_unknown_fields on each variant rejects a
/// body carrying both.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SendPayloadDto {
    Silent(SilentPayload),
    Visible(VisiblePayload),
}

impl SendPayloadDto {
    /// Build the rail payload. The daemon performs no template
    /// interpolation — visible title/body arrive final (ADR-0038 §2 keeps
    /// templates a caller-side concern here; the embedded router is where
    /// per-table templates live).
    fn into_push_payload(self) -> Result<PushPayload, String> {
        match self {
            Self::Silent(s) => {
                let lsn = s.silent.lsn.parse::<u64>().map_err(|_| {
                    format!(
                        "payload.silent.lsn must be a decimal integer, got {:?}",
                        s.silent.lsn
                    )
                })?;
                Ok(PushPayload::Silent {
                    table: s.silent.table,
                    lsn: Lsn::new(lsn),
                })
            }
            Self::Visible(v) => Ok(PushPayload::Visible {
                title: v.visible.title,
                body: v.visible.body,
                category: v.visible.category,
                data: v.visible.data,
                options: v.visible.options,
            }),
        }
    }

    /// Field caps for the visible variant (audit finding 2) — the DTO owns
    /// its own bounds so every caller gets them for free.
    fn validate_caps(&self) -> Result<(), String> {
        if let Self::Visible(v) = self {
            check_len("payload.visible.title", &v.visible.title, MAX_TITLE_LEN)?;
            check_len("payload.visible.body", &v.visible.body, MAX_BODY_LEN)?;
            if let Some(category) = &v.visible.category {
                check_len("payload.visible.category", category, MAX_CATEGORY_LEN)?;
            }
            // Reserved-key and size discipline lives in the rails crate —
            // the daemon and nostos-server's own config parser must agree on
            // what a rail will actually carry.
            nostos_infra::push::validate_data(&v.visible.data)
                .map_err(|e| format!("payload.visible.data: {e}"))?;
            nostos_infra::push::validate_options(&v.visible.options)
                .map_err(|e| format!("payload.visible.options: {e}"))?;
        }
        Ok(())
    }
}

/// priority: low | high, default low. Accepted and validated for
/// forward-compat; the rails derive wire priority from the payload variant
/// (silent = low, visible = high) — they expose no knob to override.
/// ponytail: a priority override seam lands when/if a rail grows one; the
/// field then rides SendJob instead of being dropped here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Priority {
    #[default]
    Low,
    High,
}

// Field caps (audit finding 2, plan task 4.1): enforced at the DTO layer
// after deserialize, answering 400 — a hostile body must not ride the
// handler into the coalescer or a rail unbounded. Lengths are chars
// (matches serde's view); metadata is capped on its SERIALIZED byte size.
/// Visible title cap.
const MAX_TITLE_LEN: usize = 256;
/// Visible body cap.
const MAX_BODY_LEN: usize = 1024;
/// Send-path token cap — the SAME 2048 the registry path enforces
/// (8..=2048): the two must agree, or a token POST /v1/tokens accepted
/// (Web Push subscription JSON — endpoint + p256dh + auth — commonly runs
/// 400-700 chars and can exceed smaller caps) would 400 only at send
/// time. APNs (64 hex) and FCM (~150-180) sit far below either bound.
const MAX_SEND_TOKEN_LEN: usize = 2048;
/// Caller collapse-key override cap.
const MAX_COLLAPSE_KEY_LEN: usize = 256;
/// Visible category cap.
const MAX_CATEGORY_LEN: usize = 128;
/// Serialized metadata cap, bytes.
const MAX_METADATA_BYTES: usize = 4096;

fn check_len(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.chars().count() > max {
        Err(format!("field '{field}' exceeds the {max}-character cap"))
    } else {
        Ok(())
    }
}

/// POST /v1/send body.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    token: String,
    /// Rail mode (contract 0.2.0, plan pin 2.0; role-gated since contract
    /// 0.3.0 / plan task 4.1): present AND the token not in this daemon's
    /// registry => dispatch directly on the named rail — registry-free
    /// delegation, NOW restricted to Rail-role keys (the ":rail" suffix in
    /// NOSTOS_PUSHD_API_KEYS) so a Standard tenant key cannot push to any
    /// token it happens to know. A REGISTERED token ignores the field: the
    /// registry row's platform wins, because the daemon registry is the
    /// source of truth for tokens it owns (dual registration is exactly
    /// the drift-prone second registry ADR-0037 §1 rejects) — and the
    /// registry path is open to BOTH roles.
    platform: Option<Platform>,
    payload: SendPayloadDto,
    collapse_key: Option<String>,
    /// Validated on deserialize (invalid enum values are a 400) but not
    /// otherwise read in v1 — see the enum's doc for the seam.
    #[serde(default)]
    #[allow(dead_code)]
    priority: Priority,
    /// Echoed into the receipt (push-LSN correlation channel). Map, not
    /// Value: the contract pins type: object.
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

impl SendRequest {
    /// DTO-layer field caps (audit finding 2) -> 400.
    fn validate(&self) -> Result<(), String> {
        check_len("token", &self.token, MAX_SEND_TOKEN_LEN)?;
        if let Some(collapse_key) = &self.collapse_key {
            check_len("collapse_key", collapse_key, MAX_COLLAPSE_KEY_LEN)?;
        }
        self.payload.validate_caps()?;
        if let Some(metadata) = &self.metadata {
            let serialized = serde_json::Value::Object(metadata.clone());
            let bytes = serde_json::to_vec(&serialized)
                .map_err(|e| format!("metadata is not serializable: {e}"))?;
            if bytes.len() > MAX_METADATA_BYTES {
                return Err(format!(
                    "serialized metadata is {} bytes — over the {MAX_METADATA_BYTES}-byte cap",
                    bytes.len()
                ));
            }
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct SendAcceptedBody {
    push_id: String,
    status: &'static str,
}

/// POST /v1/send — queue one token-addressed push (202 + push_id).
///
/// Ordering per the brief + audit closeout (plan task 4.1): rate limit
/// (429 — before any parsing, the cheap shed) -> parse (400) -> field caps
/// (400) -> registry lookup (404 when the token is unregistered AND no
/// platform field — rail mode) -> rail-mode role gate (403 for a Standard
/// key) -> rail configured check BEFORE enqueueing (503) -> pending-gate
/// admission (429 on the coalescer's key ceiling) -> bounded try_send (503
/// on full). No priority use yet — see [Priority].
async fn send(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Extension(role): Extension<KeyRole>,
    raw: Bytes,
) -> Result<(StatusCode, Json<SendAcceptedBody>), ApiError> {
    if let Err(rejected) = state.send_limiter.try_acquire(&tenant.0) {
        return Err(err_retry_after(
            "send rate limit exceeded for this tenant — retry later",
            rejected.retry_after_secs,
        ));
    }
    let body: SendRequest = serde_json::from_slice(&raw)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;
    body.validate()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid request: {e}")))?;
    let payload = body
        .payload
        .into_push_payload()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    let platform = resolve_platform(&state, &tenant.0, &body.token, body.platform, &role).await?;
    if !state.rails.configured(platform) {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "push rail for platform '{}' is not configured on this daemon",
                platform.as_str()
            ),
        ));
    }
    let push_id = uuid::Uuid::new_v4().to_string();
    let job = SendJob {
        tenant_id: tenant.0.clone(),
        token: body.token.clone(),
        platform,
        push_id: push_id.clone(),
        payload,
        collapse_key: body.collapse_key,
        metadata: body.metadata.map(serde_json::Value::Object),
    };
    // Pending-key admission (audit finding 2): a NEW (tenant, token) key
    // past the coalescer's ceiling is refused here; joins for an
    // already-open key always pass. Admission and try_send have no await
    // between them, so a cancelled request cannot leak a gate slot; a full
    // channel releases the slot it just took.
    let job_key = (tenant.0, body.token);
    if !state.gate.lock().expect("pending gate").admit(&job_key) {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "coalescer pending-key ceiling reached — retry",
        ));
    }
    state.sender.try_send(job).map_err(|_| {
        state.gate.lock().expect("pending gate").release(&job_key);
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "coalescer queue full — retry",
        )
    })?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SendAcceptedBody {
            push_id,
            status: "accepted",
        }),
    ))
}

/// Platform resolution (contract 0.3.0): the registry row wins for a
/// registered token; rail mode (unregistered + platform field) is the
/// Rail-role-only delegation path; unregistered + no field is a 404.
/// Shared by /v1/send and /v1/send/batch (contract 0.4.0) so the role gate
/// can never drift between the two routes.
async fn resolve_platform(
    state: &AppState,
    tenant: &str,
    token: &str,
    platform_field: Option<Platform>,
    role: &KeyRole,
) -> Result<Platform, ApiError> {
    let record = state.store.lookup_token(tenant, token).await.map_err(|e| {
        tracing::error!(error = %e, "token lookup failed");
        internal("token registry unavailable")
    })?;
    match record {
        Some(record) => Ok(record.platform),
        None => match platform_field {
            Some(_) if *role != KeyRole::Rail => {
                // Audit finding 1: rail mode is the trusted-delegator path —
                // a Standard tenant key must not dispatch to tokens outside
                // this daemon's registry.
                Err(err(
                    StatusCode::FORBIDDEN,
                    "rail mode (unregistered token + platform field) requires a :rail-role \
                     API key",
                ))
            }
            Some(platform) => Ok(platform),
            None => Err(err(
                StatusCode::NOT_FOUND,
                "token not registered for this tenant",
            )),
        },
    }
}

// ------------------------------------------------------------ batch send

/// Contract cap (0.4.0): max items per /v1/send/batch request — 100 × the
/// ~7.5KB worst-case per-item fields stays well under the axum body limit.
/// Note the effective ceiling is also the tenant's send-burst bucket: a
/// batch larger than NOSTOS_PUSHD_SEND_BURST always 429s (all-or-nothing).
const MAX_BATCH_ITEMS: usize = 100;

/// POST /v1/send/batch body — items ride the SAME SendRequest DTO (same
/// caps, same deny_unknown_fields) as /v1/send.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchSendRequest {
    items: Vec<SendRequest>,
}

/// One slot of the batch response, in request order: `push_id` when the
/// item was admitted, `error` when it wasn't.
#[derive(serde::Serialize)]
struct BatchSendItemResult {
    index: usize,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    push_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(serde::Serialize)]
struct BatchSendAcceptedBody {
    results: Vec<BatchSendItemResult>,
}

/// POST /v1/send/batch — up to 100 token-addressed pushes (contract 0.4.0,
/// plan v1.1 pin). Two phases:
///
/// Phase 1 is ATOMIC — nothing is admitted or sent on a failure: parse +
/// item-count cap (400) -> ONE non-draining acquire_n on the tenant bucket
/// (429 for the whole batch; a single-send's parse-before-rate order is
/// inverted here because N is unknowable before parsing — the body stays
/// axum-size-capped, and the bucket is still checked before any registry
/// I/O) -> per-item validate + platform resolution + rail-configured check
/// (400/403/404/503 naming the FIRST failing index).
///
/// Phase 2 admits per item (pending-gate + bounded try_send) with per-item
/// outcomes in request order: 202 when >=1 item was admitted; 503 only when
/// EVERY item failed admission — nothing left the daemon, so the whole
/// batch is safe to retry. Note the n tokens are NOT refunded on phase-2
/// admission failures (an admission attempt costs its token, same as
/// /v1/send) — a retried batch re-pays. Already-admitted items are NOT
/// rolled back when a later item fails admission: they will legitimately
/// send, and the per-item results say exactly which to retry.
async fn send_batch(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Extension(role): Extension<KeyRole>,
    raw: Bytes,
) -> Result<(StatusCode, Json<BatchSendAcceptedBody>), ApiError> {
    let body: BatchSendRequest = serde_json::from_slice(&raw)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;
    // Effective ceiling: a batch larger than the tenant's bucket (burst)
    // can NEVER acquire n tokens, so reject it at 400 here instead of a
    // guaranteed 429 (batches of 51..=100 were unreachable at the default
    // burst of 50 — caught in review 2026-08-17).
    let effective_cap = MAX_BATCH_ITEMS.min(state.send_limiter.burst_for(&tenant.0) as usize);
    if body.items.is_empty() || body.items.len() > effective_cap {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "items must contain 1..={effective_cap} entries (got {}; cap is min({MAX_BATCH_ITEMS}, send burst))",
                body.items.len()
            ),
        ));
    }
    // One token PER ITEM, atomically: an insufficient bucket 429s the whole
    // batch and keeps every token (try_acquire_n never partially drains).
    let n = u32::try_from(body.items.len()).unwrap_or(u32::MAX);
    if let Err(rejected) = state.send_limiter.try_acquire_n(&tenant.0, n) {
        return Err(err_retry_after(
            "send rate limit exceeded for this tenant — retry later (batches are all-or-nothing)",
            rejected.retry_after_secs,
        ));
    }
    // Phase 1 (atomic): validate + resolve + rail-check every item BEFORE
    // any admission — any failure aborts the batch with zero sends AND
    // refunds the n reserved tokens (zero sends attempted; /v1/send charges
    // 1 token for the same failure — a batch must not charge n).
    let phase1: Result<Vec<_>, ApiError> = async {
        let mut resolved = Vec::with_capacity(body.items.len());
        for (index, item) in body.items.into_iter().enumerate() {
            if let Err(e) = item.validate() {
                Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("item {index}: invalid request: {e}"),
                ))?;
            }
            let SendRequest {
                token,
                platform,
                payload,
                collapse_key,
                metadata,
                ..
            } = item;
            let payload = payload.into_push_payload().map_err(|e| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("item {index}: invalid payload: {e}"),
                )
            })?;
            let platform = resolve_platform(&state, &tenant.0, &token, platform, &role)
                .await
                .map_err(|(status, _headers, Json(v)): (StatusCode, HeaderMap, Json<serde_json::Value>)| {
                    let msg = v
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("invalid item")
                        .to_string();
                    err(status, format!("item {index}: {msg}"))
                })?;
            if !state.rails.configured(platform) {
                Err(err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "item {index}: push rail for platform '{}' is not configured on this daemon",
                        platform.as_str()
                    ),
                ))?;
            }
            resolved.push((index, token, platform, payload, collapse_key, metadata));
        }
        Ok(resolved)
    }
    .await;
    let resolved = match phase1 {
        Ok(r) => r,
        Err(e) => {
            state.send_limiter.release_n(&tenant.0, n);
            return Err(e);
        }
    };
    // Phase 2: per-item admission, outcomes in request order.
    let mut results = Vec::with_capacity(resolved.len());
    let mut accepted = 0usize;
    for (index, token, platform, payload, collapse_key, metadata) in resolved {
        let push_id = uuid::Uuid::new_v4().to_string();
        let job = SendJob {
            tenant_id: tenant.0.clone(),
            token: token.clone(),
            platform,
            push_id: push_id.clone(),
            payload,
            collapse_key,
            metadata: metadata.map(serde_json::Value::Object),
        };
        // Admission and try_send have no await between them (the /v1/send
        // discipline): a dropped request cannot leak a gate slot; a full
        // channel releases the slot it just took.
        let key = (tenant.0.clone(), token);
        let outcome = if state.gate.lock().expect("pending gate").admit(&key) {
            if state.sender.try_send(job).is_err() {
                state.gate.lock().expect("pending gate").release(&key);
                Err("coalescer_queue_full")
            } else {
                Ok(())
            }
        } else {
            Err("coalescer_pending_ceiling")
        };
        match outcome {
            Ok(()) => {
                accepted += 1;
                results.push(BatchSendItemResult {
                    index,
                    status: "accepted",
                    push_id: Some(push_id),
                    error: None,
                });
            }
            Err(code) => results.push(BatchSendItemResult {
                index,
                status: "rejected",
                push_id: None,
                error: Some(code),
            }),
        }
    }
    let status = if accepted == 0 {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(BatchSendAcceptedBody { results })))
}

// --------------------------------------------------------------- receipts

#[derive(serde::Deserialize)]
struct ReceiptsQuery {
    #[serde(default)]
    since: i64,
    #[serde(default = "default_limit")]
    limit: u64,
}

fn default_limit() -> u64 {
    100
}

#[derive(serde::Serialize)]
struct ReceiptDto {
    seq: i64,
    push_id: String,
    token: String,
    outcome: crate::store::Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
    provider_ts: String,
}

#[derive(serde::Serialize)]
struct ReceiptsBody {
    receipts: Vec<ReceiptDto>,
}

/// GET /v1/receipts?since=&limit= — the tenant-scoped slice of the
/// append-only log, seq ascending. limit clamps into 1..=1000 (the
/// contract's maximum); a limit of 0 would mean "ask nothing", which is
/// served as the default page instead of an empty forever-cursor.
async fn receipts(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Query(q): Query<ReceiptsQuery>,
) -> Result<Json<ReceiptsBody>, ApiError> {
    let limit = u32::try_from(q.limit.clamp(1, 1000)).unwrap_or(100);
    let rows = state
        .store
        .list_receipts(&tenant.0, q.since, limit)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "receipt list failed");
            internal("receipt log unavailable")
        })?;
    Ok(Json(ReceiptsBody {
        receipts: rows
            .into_iter()
            .map(|r| ReceiptDto {
                seq: r.seq,
                push_id: r.push_id,
                token: r.token,
                outcome: r.outcome,
                detail: r.detail,
                metadata: r.metadata,
                provider_ts: r.provider_ts,
            })
            .collect(),
    }))
}
