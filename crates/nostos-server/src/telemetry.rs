//! Tracing setup and the credential-safe request span.

/// Request span that records the URI **path only**, never the query string.
///
/// Browsers cannot set an `Authorization` header on a WebSocket handshake, so
/// `/sync` also accepts the bearer token as `?token=<jwt>` (see `AuthQuery` in
/// nostos-infra's transport). tower-http's `DefaultMakeSpan` records the whole
/// URI, which writes that live credential into every request span — and from
/// there into stdout, any log aggregator, and anything tailing the container.
/// Reverse proxies keep their own access logs, so this does not fix the whole
/// class; it stops Nostos from being the one that leaks it.
pub(crate) fn redacted_request_span(req: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    tracing::info_span!(
        "request",
        method = %req.method(),
        path = %req.uri().path(),
        version = ?req.version(),
    )
}

pub(crate) fn init_tracing(filter: &str) {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")))
        .try_init();
}
