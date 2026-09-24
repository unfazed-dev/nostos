//! Browser-origin policy: the CORS layer and the `/sync` origin allow-list parser.

use anyhow::Context;
use tracing::info;

/// Builds the `/rules`/`/sync`/`/schema` CORS layer from `NOSTOS_CORS_ORIGINS`.
///
/// Empty ⇒ `CorsLayer::permissive()` (local dev, no credentials). Non-empty
/// ⇒ an explicit origin allow-list with a **fixed** header list, never
/// `tower_http::cors::Any`: tower-http's `ensure_usable_cors_rules` rejects
/// `allow_headers(Any)` combined with `allow_credentials(true)` inside
/// `Layer::layer` — i.e. at router construction / server boot, not on first
/// request — so that combination panics the server for *any* non-empty
/// `NOSTOS_CORS_ORIGINS`, independent of whether the origins parse. The admin
/// panel (`web/src/routes/admin/rules/+page.svelte`) sends `authorization`
/// and `content-type`; those two are the only headers any route needs.
///
/// An origin that fails to parse is a startup error, not a silently dropped
/// entry — a typo'd origin used to vanish from the allow-list without a log,
/// which looks identical to (and is easy to mistake for) an all-origins CORS
/// lockout.
///
/// Methods must include `PUT` — the admin panel's own `PUT /rules` save is
/// otherwise blocked by CORS the moment `NOSTOS_CORS_ORIGINS` is configured,
/// even though the route itself is reachable and correctly gated. `DELETE`
/// for the same reason: the SDKs deregister push tokens on sign-out
/// (ADR-0037 `DELETE /push-tokens/{token}`) from browser clients.
/// Split a comma-separated origin list, dropping blanks and trimming space.
/// Empty input ⇒ empty vec ⇒ the caller's check stays off.
pub(crate) fn parse_origin_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub(crate) fn build_cors_layer(cors_origins: &str) -> anyhow::Result<tower_http::cors::CorsLayer> {
    if cors_origins.is_empty() {
        return Ok(tower_http::cors::CorsLayer::permissive());
    }

    let mut origins = Vec::new();
    for raw in cors_origins.split(',') {
        let trimmed = raw.trim();
        let origin: axum::http::HeaderValue = trimmed
            .parse()
            .with_context(|| format!("NOSTOS_CORS_ORIGINS: invalid origin {trimmed:?}"))?;
        origins.push(origin);
    }
    info!(?origins, "CORS: explicit origins");

    Ok(tower_http::cors::CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ])
        .allow_credentials(true))
}

/// C1 regression: tower-http 0.5.2's `ensure_usable_cors_rules` runs inside
/// `Layer::layer`, i.e. when the layer is attached to a router — server
/// boot, not first request. Before the fix, `build_cors_layer` returned
/// `allow_headers(Any)` + `allow_credentials(true)` for any non-empty
/// `NOSTOS_CORS_ORIGINS`, which panicked as soon as `.layer(cors)` ran.
/// `non_empty_origins_survive_router_construction` reproduces the exact call
/// shape (`Router::new()....layer(cors)`) used in `main()`; it panics on the
/// pre-fix code and passes once `allow_headers` is a fixed list.
#[cfg(test)]
mod cors_tests {
    use super::build_cors_layer;

    #[test]
    fn non_empty_origins_survive_router_construction() {
        let cors = build_cors_layer("https://example.com")
            .expect("a single well-formed origin must build");

        // This is what used to panic: attaching the layer is where
        // tower-http's `ensure_usable_cors_rules` assert fires, not the
        // builder chain above.
        let router: axum::Router = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(cors);
        drop(router);
    }

    #[test]
    fn empty_origins_stay_permissive() {
        // Local-dev default path must be untouched by the fix.
        let _cors = build_cors_layer("").expect("empty NOSTOS_CORS_ORIGINS never fails to build");
    }

    #[test]
    fn unparseable_origin_fails_loudly_instead_of_vanishing() {
        // Secondary defect fixed alongside C1: the old
        // `.filter_map(|s| s.trim().parse().ok())` silently dropped any
        // origin that failed to parse, so a typo'd operator-supplied origin
        // produced an empty allow-list with no error and no log line. A
        // raw control character (not permitted in an HTTP header value) is
        // enough to make `HeaderValue::from_str` fail.
        let err = build_cors_layer("https://good.example.com,bad\u{7}origin")
            .expect_err("an origin that fails HeaderValue parsing must error, not vanish");
        assert!(
            err.to_string().contains("NOSTOS_CORS_ORIGINS"),
            "error should name the offending config, got: {err}"
        );
    }
}
