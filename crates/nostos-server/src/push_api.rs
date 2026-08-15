//! REST push-token registration (ADR-0037 §3, plan task 3.1).
//!
//! Pinned contract:
//!
//! - `POST /push-tokens` body `{"platform":"fcm"|"apns"|"webpush","token":"…"}`
//!   → `204` on success.
//! - `DELETE /push-tokens/{token}` → `204` (idempotent — a no-op delete is
//!   still a success).
//!
//! Both routes use the SAME JWT path as `/sync` (the `SyncAuth` port):
//! `Authorization: Bearer <jwt>` → `Principal`. `tenant_id` / `account_id`
//! are stamped SERVER-side from the authenticated principal (ADR-0018
//! discipline — a client-attested tenant on a token row is an
//! exfiltration-adjacent bug): the body DTO is `deny_unknown_fields`, so a
//! client-sent `tenant_id`/`account_id` field fails deserialization with a
//! 4xx before any row is built. Anonymous principals are rejected (401) —
//! there is no identity to stamp.
//!
//! Ordering mirrors `put_rules_handler`: the auth gate runs before the body
//! is parsed (raw `Bytes`, not axum's `Json<T>` extractor), so an
//! unauthenticated caller with a malformed body learns nothing but the 401.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use nostos_application::ports::SyncAuth;
use nostos_infra::PushTokenRegistry;

/// Per-route state, merged onto the main router with its own `with_state`.
#[derive(Clone)]
pub struct PushApiState {
    pub auth: Arc<dyn SyncAuth>,
    pub registry: Arc<dyn PushTokenRegistry>,
    /// The operator's tenant column (`NOSTOS_TENANT_COLUMN`) — decides
    /// whether `Principal::tenant_scope` applies for the stamping.
    pub tenant_column: Option<String>,
}

/// `POST /push-tokens` body. `deny_unknown_fields` IS the
/// client-attestation rejection: any `tenant_id`/`account_id` field a client
/// sends is an unknown field ⇒ 4xx, never a value that reaches a row.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterPushToken {
    platform: String,
    token: String,
}

/// The rails the registry's `platform` column may name (token_store.rs).
const PLATFORMS: [&str; 3] = ["fcm", "apns", "webpush"];
/// Sanity cap on the client-supplied token string — APNs tokens are 64 hex
/// chars, FCM ~152, a Web Push subscription endpoint a URL; anything past
/// this is garbage, not a token.
const MAX_TOKEN_LEN: usize = 2048;

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// The server-stamped `(account_id, tenant_id)` for the caller. `None` maps
/// to 401: no/invalid bearer token, or the anonymous principal
/// (`NOSTOS_SYNC_AUTH=none`) — there is no identity to stamp a token row
/// with, so push registration requires an authenticated deploy. The tenant
/// value comes from `Principal::tenant_scope` (the same seam the write path
/// uses) when a tenant column is configured, else the principal's
/// authenticated tenant claim.
async fn authenticate(state: &PushApiState, token: &str) -> Option<(String, String)> {
    let principal = state.auth.authenticate(token).await?;
    if principal.is_anonymous() {
        return None;
    }
    let tenant_id = principal
        .tenant_scope(state.tenant_column.as_deref())
        .map_or_else(
            || principal.tenant_id.clone(),
            |scope| scope.value.to_string(),
        );
    Some((principal.account_id, tenant_id))
}

/// `POST /push-tokens` — register the caller's device token.
pub async fn post_push_token(
    State(state): State<PushApiState>,
    headers: axum::http::HeaderMap,
    raw_body: Bytes,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    // 1. Auth before anything touches the body.
    let bearer = bearer_token(&headers)
        .map(str::to_string)
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    let (account_id, tenant_id) = authenticate(&state, &bearer).await.ok_or_else(|| {
        err(
            StatusCode::UNAUTHORIZED,
            "push-token registration requires an authenticated principal",
        )
    })?;

    // 2. CSRF stance (same as PUT /rules): bearer-header auth has no ambient
    // credential, so the one enforceable defence is rejecting non-JSON bodies.
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        return Err(err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        ));
    }

    // 3. Parse — deny_unknown_fields makes any client-attested
    // tenant/account field a 400 here, before a row exists.
    let body: RegisterPushToken = serde_json::from_slice(&raw_body).map_err(|e| {
        err(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid JSON body: {e} (tenant_id/account_id are stamped server-side \
                 from the authenticated principal and must not be sent)"
            ),
        )
    })?;

    // 4. Validate the client-controlled fields.
    let token = body.token.trim();
    if !PLATFORMS.contains(&body.platform.as_str()) {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "unknown platform {:?} (expected one of fcm|apns|webpush)",
                body.platform
            ),
        ));
    }
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("token must be 1..={MAX_TOKEN_LEN} chars"),
        ));
    }

    // 5. Server-side stamping (ADR-0018): the principal's own account and
    // tenant — computed in `authenticate`, never read from the body.
    state
        .registry
        .upsert(&body.platform, token, &account_id, &tenant_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "push-token upsert failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token registry unavailable",
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /push-tokens/{token}` — sign-out deregistration. Owner-scoped:
/// only the caller's own row disappears; deleting a token that has migrated
/// to (or was registered by) another principal is a 204 no-op.
pub async fn delete_push_token(
    State(state): State<PushApiState>,
    Path(token): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let bearer = bearer_token(&headers)
        .map(str::to_string)
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    let (account_id, tenant_id) = authenticate(&state, &bearer).await.ok_or_else(|| {
        err(
            StatusCode::UNAUTHORIZED,
            "push-token deregistration requires an authenticated principal",
        )
    })?;
    state
        .registry
        .delete_for_owner(&tenant_id, &account_id, &token)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "push-token delete failed");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token registry unavailable",
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nostos_domain::Principal;
    use nostos_infra::{InMemoryTokenRegistry, PushTokenRegistry};

    /// Always resolves to one fixed principal — the authenticated test path.
    struct FixedAuth(Principal);

    #[async_trait]
    impl SyncAuth for FixedAuth {
        async fn authenticate(&self, _token: &str) -> Option<Principal> {
            Some(self.0.clone())
        }
    }

    fn state_for(auth: Arc<dyn SyncAuth>) -> PushApiState {
        PushApiState {
            auth,
            registry: Arc::new(InMemoryTokenRegistry::new()),
            tenant_column: Some("org_id".into()),
        }
    }

    fn authed_state_with() -> (PushApiState, Arc<InMemoryTokenRegistry>) {
        let registry = Arc::new(InMemoryTokenRegistry::new());
        let state = PushApiState {
            auth: Arc::new(FixedAuth(Principal::new("u1", "t1"))),
            registry: registry.clone(),
            tenant_column: Some("org_id".into()),
        };
        (state, registry)
    }

    fn headers_with_bearer() -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer jwt-x".parse().unwrap(),
        );
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        h
    }

    fn no_auth_headers() -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        h
    }

    fn body(v: &serde_json::Value) -> Bytes {
        Bytes::from(v.to_string())
    }

    #[tokio::test]
    async fn missing_bearer_is_401() {
        let (state, _) = authed_state_with();
        let res = post_push_token(
            State(state),
            no_auth_headers(),
            body(&serde_json::json!({"platform":"fcm","token":"t"})),
        )
        .await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn anonymous_principal_is_401() {
        // NOSTOS_SYNC_AUTH=none mints anonymous principals — there is no
        // identity to stamp, so registration must refuse.
        let state = state_for(Arc::new(nostos_infra::AllowAnonymous::new()));
        let res = post_push_token(
            State(state),
            headers_with_bearer(),
            body(&serde_json::json!({"platform":"fcm","token":"t"})),
        )
        .await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn client_attested_tenant_is_rejected_4xx() {
        let (state, registry) = authed_state_with();
        for field in ["tenant_id", "account_id"] {
            let mut v = serde_json::json!({"platform":"fcm","token":"t"});
            v[field] = serde_json::json!("attacked");
            let res = post_push_token(State(state.clone()), headers_with_bearer(), body(&v)).await;
            assert_eq!(
                res.unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "a client-attested {field} must be a 4xx"
            );
        }
        assert!(
            registry.list_by_tenant("t1").await.unwrap().is_empty(),
            "nothing may be registered from a rejected body"
        );
    }

    #[tokio::test]
    async fn unknown_platform_is_422() {
        let (state, _) = authed_state_with();
        let res = post_push_token(
            State(state),
            headers_with_bearer(),
            body(&serde_json::json!({"platform":"sms","token":"t"})),
        )
        .await;
        assert_eq!(res.unwrap_err().0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn post_stamps_server_side_and_returns_204() {
        let (state, registry) = authed_state_with();
        let res = post_push_token(
            State(state),
            headers_with_bearer(),
            body(&serde_json::json!({"platform":"apns","token":"dev-9"})),
        )
        .await
        .expect("valid registration");
        assert_eq!(res, StatusCode::NO_CONTENT);

        // The row is stamped with the SERVER-side identity (u1/t1 from the
        // authenticated principal — tenant via tenant_scope's value, which
        // for this fixture equals the principal's tenant claim).
        let rows = registry.list_by_account("t1", "u1").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform, "apns");
        assert_eq!(rows[0].token, "dev-9");
    }

    #[tokio::test]
    async fn delete_is_owner_scoped_and_idempotent() {
        let (state, registry) = authed_state_with();
        post_push_token(
            State(state.clone()),
            headers_with_bearer(),
            body(&serde_json::json!({"platform":"fcm","token":"dev-1"})),
        )
        .await
        .expect("register");

        // Another principal's deregistration is a 204 no-op: the row stays.
        let other = PushApiState {
            auth: Arc::new(FixedAuth(Principal::new("u2", "t1"))),
            registry: state.registry.clone(),
            tenant_column: state.tenant_column.clone(),
        };
        let res =
            delete_push_token(State(other), Path("dev-1".into()), headers_with_bearer()).await;
        assert_eq!(res.expect("204"), StatusCode::NO_CONTENT);
        assert_eq!(registry.list_by_account("t1", "u1").await.unwrap().len(), 1);

        // The owner's deregistration removes it; repeating is a no-op 204.
        let res = delete_push_token(
            State(state.clone()),
            Path("dev-1".into()),
            headers_with_bearer(),
        )
        .await;
        assert_eq!(res.expect("204"), StatusCode::NO_CONTENT);
        assert!(registry
            .list_by_account("t1", "u1")
            .await
            .unwrap()
            .is_empty());
        let res =
            delete_push_token(State(state), Path("dev-1".into()), headers_with_bearer()).await;
        assert_eq!(res.expect("204"), StatusCode::NO_CONTENT);
    }
}
