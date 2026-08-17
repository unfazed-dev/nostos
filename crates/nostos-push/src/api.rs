//! REST surface — the OpenAPI contract at docs/api/nostos-pushd.yaml,
//! implemented EXACTLY (plan task 1.4 for tokens, 1.5 for send; receipts
//! per pin 0.4). Route discipline mirrors nostos-server push_api.rs:
//! deny_unknown_fields DTOs, auth ordering handled by middleware before
//! any handler runs, owner-scoped deletes, and the {"error": string}
//! error shape on every non-2xx this crate produces.
//!
//! Bodies are parsed from raw Bytes (not the Json extractor) so a
//! malformed body gets THIS crate's 400 error shape, not axum's default
//! rejection body.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{delete, get, post};
use axum::Router;
use nostos_domain::Lsn;
use nostos_infra::push::PushPayload;
use tokio::sync::mpsc;

use crate::auth::{auth_middleware, ApiKeys, TenantId};
use crate::coalescer::SendJob;
use crate::rail::Rails;
use crate::store::{Platform, Store};

/// Shared route state. Cloned per request; cheap (Arcs + a channel sender).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub rails: Rails,
    pub api_keys: ApiKeys,
    /// Bounded coalescer inbox — try_send only.
    pub sender: mpsc::Sender<SendJob>,
}

/// The contract's error body everywhere: {"error": string}.
type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn internal(message: &str) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Wire the whole router: /v1/healthz unauthenticated, every other /v1
/// route behind the bearer middleware (which stamps the tenant).
pub fn build_router(state: AppState) -> Router {
    let open = Router::new()
        .route("/v1/healthz", get(healthz))
        .with_state(state.clone());
    let authed = Router::new()
        .route("/v1/tokens", post(register_token))
        .route("/v1/tokens/:token", delete(delete_token))
        .route("/v1/send", post(send))
        .route("/v1/receipts", get(receipts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    open.merge(authed)
}

// ---------------------------------------------------------------- healthz

#[derive(serde::Serialize)]
struct HealthzBody {
    status: &'static str,
    rails: crate::rail::RailsHealth,
}

/// GET /v1/healthz — unauthenticated liveness + which rails from_env() built.
async fn healthz(State(state): State<AppState>) -> Json<HealthzBody> {
    Json(HealthzBody {
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

/// POST /v1/tokens — register (upsert) the caller's device token.
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
    state
        .store
        .upsert_token(&tenant.0, token, body.platform, body.account_tag.as_deref())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "token upsert failed");
            internal("token registry unavailable")
        })?;
    Ok((
        StatusCode::CREATED,
        Json(TokenRegisteredBody { registered: true }),
    ))
}

/// DELETE /v1/tokens/{token} — deregister. Idempotent 204 for the caller's
/// own row (or a row that never existed); 404 when the token belongs to
/// another tenant (oracle-safe — the response distinguishes nothing the
/// caller could not learn from their own sends).
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
        Ok(DeleteOutcome::Deleted | DeleteOutcome::Missing) => Ok(StatusCode::NO_CONTENT),
        Ok(DeleteOutcome::Foreign) => Err(err(
            StatusCode::NOT_FOUND,
            "token not registered for this tenant",
        )),
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
            }),
        }
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

/// POST /v1/send body.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    token: String,
    /// Rail mode (contract 0.2.0, plan pin 2.0): present AND the token not
    /// in this daemon's registry => dispatch directly on the named rail —
    /// registry-free delegation for trusted delegators (the sync side's
    /// RemoteNotifier). A REGISTERED token ignores the field: the registry
    /// row's platform wins, because the daemon registry is the source of
    /// truth for tokens it owns (dual registration is exactly the
    /// drift-prone second registry ADR-0037 §1 rejects).
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

#[derive(serde::Serialize)]
struct SendAcceptedBody {
    push_id: String,
    status: &'static str,
}

/// POST /v1/send — queue one token-addressed push (202 + push_id).
///
/// Ordering per the brief: parse (400) -> registry lookup (404 when the
/// token is unregistered AND no platform field — rail mode, contract 0.2.0)
/// -> rail configured check BEFORE enqueueing (503) -> bounded try_send (503
/// on full). No priority use yet — see [Priority].
async fn send(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    raw: Bytes,
) -> Result<(StatusCode, Json<SendAcceptedBody>), ApiError> {
    let body: SendRequest = serde_json::from_slice(&raw)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;
    let payload = body
        .payload
        .into_push_payload()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid payload: {e}")))?;
    // Platform resolution (contract 0.2.0): the registry row wins for a
    // registered token; rail mode (unregistered + platform field) dispatches
    // directly with no registry row; unregistered + no field is today's 404.
    let record = state
        .store
        .lookup_token(&tenant.0, &body.token)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "token lookup failed");
            internal("token registry unavailable")
        })?;
    let platform = match record {
        Some(record) => record.platform,
        None => body.platform.ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                "token not registered for this tenant",
            )
        })?,
    };
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
        tenant_id: tenant.0,
        token: body.token,
        platform,
        push_id: push_id.clone(),
        payload,
        collapse_key: body.collapse_key,
        metadata: body.metadata.map(serde_json::Value::Object),
    };
    state.sender.try_send(job).map_err(|_| {
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
