use super::session::run_session;
use super::state::SyncRouterState;
use super::{MAX_WS_FRAME_BYTES, MAX_WS_MESSAGE_BYTES};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
#[cfg(doc)]
use nostos_application::ports::SyncAuth;
#[cfg(doc)]
use nostos_domain::Principal;
use serde::Deserialize;

/// Query-string auth fallback — browsers can't set Authorization on a WS
/// handshake, so `?token=` is the supported path for web clients.
#[derive(Debug, Deserialize)]
pub struct AuthQuery {
    #[serde(default)]
    pub token: Option<String>,
}

/// Axum handler: `GET /sync` → authenticate → WebSocket upgrade.
///
/// Reads the bearer token from the `Authorization` header or `?token=` query
/// param, resolves it to a [`Principal`] via [`SyncAuth`], and rejects with
/// HTTP 401 (no upgrade) on failure. The principal is threaded into the
/// upgraded session so predicates can be server-enforced.
///
/// Marked `async` because axum's `Handler` trait requires it; the body doesn't
/// await before upgrade (the auth check is synchronous-ish), so the lint is
/// allowed.
#[allow(clippy::unused_async)]
pub async fn sync_handler(
    ws: WebSocketUpgrade,
    State(state): State<SyncRouterState>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
) -> Response {
    // Origin allowlist (opt-in) BEFORE auth: a rejected origin should never
    // reach the verifier, and on an anonymous deployment auth would wave it
    // through anyway — which is precisely the case this guards.
    if !origin_allowed(&headers, &state.allowed_origins) {
        return forbidden_origin();
    }
    // Token from header OR `?token=` (browsers can't set WS handshake headers).
    // An empty/missing token is passed to the adapter as "" — `AllowAnonymous`
    // accepts it (returns the anonymous principal), real verifiers reject it.
    let token = bearer_token(&headers).or(query.token).unwrap_or_default();
    let principal = state.auth.authenticate(&token).await;
    let Some(principal) = principal else {
        return unauthorized();
    };
    // ADR-0029 §Decision-4 (live-socket): arm the close-on-expiry deadline from
    // the handshake token's `exp`. `None` ⇒ no deadline — the OSS `sync_auth:
    // none` default and Phase-0 no-`exp` tokens stay open, exactly as before.
    let exp = crate::auth::token_exp(&token);
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_FRAME_BYTES)
        .on_upgrade(move |socket| run_session(socket, state, principal, exp))
}

/// Pull a bearer token off the Authorization header (`Bearer <token>`).
///
/// `pub(crate)`: the iroh accept loop (`crate::iroh_sync`) applies the same
/// header policy to the handshake request it captures.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    let t = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    Some(t.to_string())
}

fn unauthorized() -> Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        "nostos: authentication required for /sync",
    )
        .into_response()
}

fn forbidden_origin() -> Response {
    // 403, not 401: the caller's credentials are not the problem and retrying
    // with a different token will not help.
    (
        axum::http::StatusCode::FORBIDDEN,
        "nostos: origin not allowed for /sync",
    )
        .into_response()
}

/// Is this upgrade permitted by the origin allowlist?
///
/// Empty allowlist ⇒ always `true` (the check is off). Otherwise an `Origin`
/// header, if present, must match one of the entries exactly. Absent `Origin`
/// ⇒ `true`: that is a native client, and only browsers both send this header
/// and are prevented from forging it. See [`SyncRouterState::allowed_origins`]
/// for why rejecting on absence would break every native client while stopping
/// nobody.
fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    match headers.get(axum::http::header::ORIGIN) {
        None => true,
        // A non-UTF-8 Origin cannot match any configured entry, so it is
        // refused rather than treated as absent.
        Some(value) => value
            .to_str()
            .is_ok_and(|origin| allowed.iter().any(|a| a == origin)),
    }
}

#[cfg(test)]
mod origin_allowlist_tests {
    use super::{origin_allowed, HeaderMap};

    fn with_origin(origin: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
        h
    }

    #[test]
    fn empty_allowlist_is_off_and_admits_everything() {
        // The default. Every deployment that upgrades without setting
        // NOSTOS_WS_ORIGINS must keep working exactly as before.
        assert!(origin_allowed(&HeaderMap::new(), &[]));
        assert!(origin_allowed(&with_origin("https://evil.example"), &[]));
    }

    #[test]
    fn configured_allowlist_admits_listed_and_refuses_unlisted() {
        let allowed = vec!["https://app.example".to_string()];
        assert!(origin_allowed(
            &with_origin("https://app.example"),
            &allowed
        ));
        assert!(!origin_allowed(
            &with_origin("https://evil.example"),
            &allowed
        ));
    }

    #[test]
    fn a_native_client_sending_no_origin_still_connects() {
        // This is the assertion that keeps the 30-odd integration tests and
        // every `nostos-client`/`nostos-bench` binary alive: tokio-tungstenite
        // sends no Origin at all. Rejecting on absence would break all of them
        // while stopping no attacker, since only browsers are forced to send a
        // truthful Origin in the first place.
        let allowed = vec!["https://app.example".to_string()];
        assert!(origin_allowed(&HeaderMap::new(), &allowed));
    }

    #[test]
    fn match_is_exact_not_a_prefix_or_suffix() {
        let allowed = vec!["https://app.example".to_string()];
        // The classic allowlist bypasses: a registrable-suffix lookalike and a
        // subdomain-prefixed impostor must both fail.
        assert!(!origin_allowed(
            &with_origin("https://app.example.evil.com"),
            &allowed
        ));
        assert!(!origin_allowed(
            &with_origin("https://notapp.example"),
            &allowed
        ));
        // Scheme and port are part of an origin, so they must be part of the
        // comparison too.
        assert!(!origin_allowed(
            &with_origin("http://app.example"),
            &allowed
        ));
        assert!(!origin_allowed(
            &with_origin("https://app.example:8443"),
            &allowed
        ));
    }

    #[test]
    fn non_utf8_origin_is_refused_not_treated_as_absent() {
        let allowed = vec!["https://app.example".to_string()];
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(!origin_allowed(&h, &allowed));
    }
}
