//! # nostos-server
//!
//! The composition root. Reads config, constructs the concrete adapters
//! (`InMemorySessionStore`, `FakeReplicator`/`PgReplicator`, WebSocket
//! transport), injects them into the application-layer use-cases
//! (`SessionManager`, `FanOutService`), and binds axum.
//!
//! This is the **only** binary that knows how the adapters are wired —
//! swapping `FakeReplicator` for `PgReplicator` (the `NOSTOS_REPLICATOR` env) is
//! a one-line change here, with zero edits to domain/application code. That's
//! the hexagonal payoff (ADR-0001).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use nostos_application::ports::{Metrics, SessionStore};
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use clap::Parser;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

/// Command-line / env configuration for the sync server.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "nostos-server",
    version,
    about = "Nostos local-first sync server"
)]
pub struct Config {
    /// Bind address.
    #[arg(long, env = "NOSTOS_BIND", default_value = "0.0.0.0:8800")]
    bind: String,

    /// WebSocket path clients connect to.
    #[arg(long, env = "NOSTOS_WS_PATH", default_value = "/sync")]
    ws_path: String,

    /// Per-session bounded buffer depth (backpressure).
    #[arg(long, env = "NOSTOS_SESSION_BUFFER", default_value_t = 1024)]
    session_buffer: usize,

    /// Replicator mode: "fake" (synthetic generator) or "pg" (real Postgres).
    /// "pg" requires the `pg` feature, which is on by default (disable with
    /// `--no-default-features`). Runtime default stays "fake" so zero-setup
    /// `cargo run` keeps working.
    #[arg(long, env = "NOSTOS_REPLICATOR", default_value = "fake")]
    replicator: String,

    /// Postgres URL for the real replicator (`NOSTOS_REPLICATOR=pg`).
    /// Empty by default — selecting `pg` without setting `NOSTOS_PG_URL` fails
    /// fast with an actionable error (see the replicator match below).
    #[arg(long, env = "NOSTOS_PG_URL", default_value = "")]
    pg_url: String,

    /// Comma-separated list of tables clients may write to over the sync socket
    /// (ADR-0013 write-back v1). Exact-match allowlist — a table not listed
    /// here can never reach the SQL builder. Empty (default) = no tables
    /// writable; clients get a clear "table not writable" error. Example:
    /// `tasks,notes`. Only meaningful under `NOSTOS_REPLICATOR=pg` (the fake
    /// replicator has no source database, so writes return
    /// "write-back requires pg replicator" even when allowlisted).
    #[arg(long, env = "NOSTOS_WRITE_TABLES", default_value = "")]
    write_tables: String,

    /// Logical-replication slot name.
    #[arg(long, env = "NOSTOS_PG_SLOT", default_value = "cairn_slot")]
    pg_slot: String,

    /// Publication name.
    #[arg(long, env = "NOSTOS_PG_PUBLICATION", default_value = "cairn_pub")]
    pg_publication: String,

    /// Log filter (RUST_LOG-style).
    #[arg(long, env = "NOSTOS_LOG", default_value = "info,nostos=debug")]
    log: String,

    /// Licensed tier for the concurrent-device cap. OSS self-host defaults to
    /// `enterprise` (unlimited); a managed Cloud deploy stamps the licensed
    /// tier here. One of: hobby, pro, scale, enterprise.
    #[arg(long, env = "NOSTOS_TIER", default_value = "enterprise")]
    tier: String,

    /// /sync authentication mode: "none" (anonymous — OSS dev default) or
    /// "supabase-jwt" (HS256-verify a Supabase JWT). A managed multi-tenant
    /// deploy MUST set "supabase-jwt"; "none" refuses to inject tenant filters
    /// so it is single-tenant only (ADR-0010).
    #[arg(long, env = "NOSTOS_SYNC_AUTH", default_value = "none")]
    sync_auth: String,

    /// The legacy HS256 shared secret used to verify Supabase JWTs at /sync.
    /// Ignored unless `NOSTOS_SYNC_AUTH=supabase-jwt`. Matches Supabase's
    /// `JWT_SECRET` (the project's GoTrue signing key) — only present on
    /// projects created before 2025-10-01 or that haven't migrated off it.
    /// At least one of this or `NOSTOS_SUPABASE_URL`/`NOSTOS_SUPABASE_JWKS_URL`
    /// must be set when `NOSTOS_SYNC_AUTH=supabase-jwt` (ADR-0010 addendum).
    #[arg(long, env = "NOSTOS_SUPABASE_JWT_SECRET", default_value = "")]
    supabase_jwt_secret: String,

    /// The Supabase project URL (e.g. `https://xyzco.supabase.co`), used to
    /// derive the JWKS URL (`<url>/auth/v1/.well-known/jwks.json`) for
    /// RS256/ES256/EdDSA verification — Supabase's default signing mode for
    /// projects created since 2025-10-01. Ignored unless
    /// `NOSTOS_SYNC_AUTH=supabase-jwt`. Superseded by
    /// `NOSTOS_SUPABASE_JWKS_URL` if both are set.
    #[arg(long, env = "NOSTOS_SUPABASE_URL", default_value = "")]
    supabase_url: String,

    /// Explicit JWKS URL, overriding the one derived from
    /// `NOSTOS_SUPABASE_URL`. Use this for a non-standard gateway/proxy in
    /// front of Supabase's auth endpoint. Ignored unless
    /// `NOSTOS_SYNC_AUTH=supabase-jwt`.
    #[arg(long, env = "NOSTOS_SUPABASE_JWKS_URL", default_value = "")]
    supabase_jwks_url: String,

    /// The tenant column server-enforced on every predicate (e.g. "org_id").
    /// When set with `NOSTOS_SYNC_AUTH=supabase-jwt`, the server ANDs
    /// `<column> = <principal.tenant_id>` into every subscription so the client
    /// cannot read another tenant's rows (ADR-0011). Defaults to "org_id".
    #[arg(long, env = "NOSTOS_TENANT_COLUMN", default_value = "org_id")]
    tenant_column: String,

    /// Allowed CORS origins for browser clients, comma-separated (e.g.
    /// "https://app.example.com,http://localhost:3000"). Empty (default) =
    /// permissive (any origin) for local dev; set explicitly for production.
    #[arg(long, env = "NOSTOS_CORS_ORIGINS", default_value = "")]
    cors_origins: String,

    /// WAL-bloat protection: the maximum LSN-gap (in WAL bytes) a live client
    /// may lag behind the head of the stream before it is evicted. A client
    /// exceeding this is disconnected; it reconnects + re-syncs from a fresh
    /// checkpoint — trading a controlled replay window for source-DB safety.
    /// `0` (default) = eviction OFF (no client is ever dropped for lag). A
    /// production deploy MUST set this AND `--pg-slot-wal-keep-size` to protect
    /// the primary's disk (ADR-0016).
    #[arg(long, env = "NOSTOS_SLOT_MAX_LAG", default_value_t = 0)]
    slot_max_lag: u64,

    /// Postgres `max_slot_wal_keep_size` for the replication slot (MB). Caps how
    /// much WAL a lagging slot may retain on the primary before Postgres
    /// itself invalidates the slot — the database-level backstop for WAL bloat.
    /// `0` (default) = Postgres's built-in default (unbounded). Set alongside
    /// `--slot-max-lag` in production (ADR-0016).
    #[arg(long, env = "NOSTOS_PG_SLOT_WAL_KEEP_SIZE", default_value_t = 0)]
    pg_slot_wal_keep_size: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    init_tracing(&cfg.log);

    // ---- construct the adapters (the infra ring) ----
    // Coerce to the trait object once so both use-cases share one store.
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());

    // ---- inject into the application use-cases ----
    // The licensed tier gates the concurrent-device cap. OSS self-host defaults
    // to Enterprise (unlimited); a managed deploy sets NOSTOS_TIER=hobby|pro|scale.
    let tier = match cfg.tier.as_str() {
        "hobby" => nostos_domain::Tier::Hobby,
        "pro" => nostos_domain::Tier::Pro,
        "scale" => nostos_domain::Tier::Scale,
        _ => nostos_domain::Tier::Enterprise,
    };
    info!(?tier, devices_cap = tier.device_cap(), "licensed tier");
    let manager = Arc::new(SessionManager::new(Arc::clone(&store), tier));

    // ---- /sync authentication (ADR-0010) ----
    // The OSS self-host default is `none` (anonymous — single-tenant dev). A
    // managed deploy sets `supabase-jwt` + the secret; without it the server
    // cannot enforce tenant scoping and refuses to inject predicates.
    let auth: Arc<dyn nostos_application::ports::SyncAuth> = match cfg.sync_auth.as_str() {
        "supabase-jwt" => {
            // Config resolution (ADR-0010 addendum): at least one of the
            // legacy HS256 secret or a JWKS source must be set. Explicit
            // NOSTOS_SUPABASE_JWKS_URL wins over one derived from
            // NOSTOS_SUPABASE_URL. Both a secret and a JWKS source may be set
            // at once — each token's header `alg` then picks the verifier
            // (HS256 -> secret, RS256/ES256/EdDSA -> JWKS); neither is ever
            // checked against the other's key material.
            let secret = (!cfg.supabase_jwt_secret.is_empty())
                .then(|| cfg.supabase_jwt_secret.as_bytes().to_vec());
            let jwks_url = if !cfg.supabase_jwks_url.is_empty() {
                Some(cfg.supabase_jwks_url.clone())
            } else if !cfg.supabase_url.is_empty() {
                Some(format!(
                    "{}/auth/v1/.well-known/jwks.json",
                    cfg.supabase_url.trim_end_matches('/')
                ))
            } else {
                None
            };
            if secret.is_none() && jwks_url.is_none() {
                anyhow::bail!(
                    "NOSTOS_SYNC_AUTH=supabase-jwt requires at least one of \
                     NOSTOS_SUPABASE_JWT_SECRET (legacy HS256) or \
                     NOSTOS_SUPABASE_URL/NOSTOS_SUPABASE_JWKS_URL (RS256/ES256/EdDSA)"
                );
            }
            info!(
                hs256 = secret.is_some(),
                jwks = jwks_url.is_some(),
                "sync auth: supabase-jwt (tenant-enforced)"
            );
            Arc::new(nostos_infra::SupabaseJwtAuth::from_config(secret, jwks_url))
        }
        "none" => {
            warn!(
                "sync auth: NONE — /sync is unauthenticated. Single-tenant/dev only; \
                 set NOSTOS_SYNC_AUTH=supabase-jwt for any multi-tenant deploy."
            );
            Arc::new(nostos_infra::AllowAnonymous::new())
        }
        other => anyhow::bail!(
            "unknown NOSTOS_SYNC_AUTH value: {other} (expected 'none' or 'supabase-jwt')"
        ),
    };

    // Aggregate metrics shared between the fan-out service (writer) and the
    // /metrics endpoint (reader). The session gauge is updated on connect/
    // disconnect by the manager — for now we snapshot the store count on read.
    let metrics = Arc::new(nostos_application::ports::Metrics::new());
    // WAL-bloat protection: OFF by default (slot_max_lag=0); a deploy that sets
    // NOSTOS_SLOT_MAX_LAG opts into evicting clients that lag past it (ADR-0016).
    let eviction = if cfg.slot_max_lag > 0 {
        nostos_application::EvictionPolicy::new(cfg.slot_max_lag)
    } else {
        nostos_application::EvictionPolicy::disabled()
    };
    let fanout = Arc::new(
        FanOutService::new(Arc::clone(&store))
            .with_metrics(Arc::clone(&metrics))
            .with_eviction(eviction),
    );

    // ---- start the replicator → fan-out driver ----
    // The extractor lifts named columns out of an event's payload so predicates
    // (which match on column equality) can be evaluated. For the FakeReplicator
    // the payload is opaque bytes, so we return `Any` (table-only matching). For
    // the PgReplicator the payload is a small JSON object {col:val}, so we parse
    // it and return real values — enabling filter predicates like org_id=acme.
    match cfg.replicator.as_str() {
        "fake" => {
            let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(u64::MAX));
            let fanout_drv = Arc::clone(&fanout);
            let drv = tokio::spawn(async move {
                let extract = |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> {
                    Some(ColumnValue::Any)
                };
                let outcome = fanout_drv.run(&mut repl, extract).await;
                info!(?outcome, "replicator stream ended");
            });
            std::mem::forget(drv);
            info!("replicator: FakeReplicator (synthetic, unbounded)");
        }
        "pg" => {
            #[cfg(feature = "pg")]
            {
                use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
                if cfg.pg_url.trim().is_empty() {
                    anyhow::bail!(
                        "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set. \
                         Set NOSTOS_PG_URL, e.g. after: \
                         docker compose -f docker/docker-compose.yml up -d"
                    );
                }
                let mut pg_cfg =
                    PgReplicatorConfig::from_url(&cfg.pg_url, &cfg.pg_slot, &cfg.pg_publication)
                        .context("invalid NOSTOS_PG_URL")?;
                pg_cfg.max_slot_wal_keep_size_mb = cfg.pg_slot_wal_keep_size;
                if cfg.pg_slot_wal_keep_size > 0 {
                    info!(
                        keep_size_mb = cfg.pg_slot_wal_keep_size,
                        "WAL-bloat backstop: will set max_slot_wal_keep_size on the slot"
                    );
                }
                let mut repl = PgReplicator::new(pg_cfg);
                let fanout_drv = Arc::clone(&fanout);
                let drv = tokio::spawn(async move {
                    // Extract a column from the JSON payload: parse the small
                    // object and return the named field. Cheap (one parse per
                    // candidate event) and keeps predicates honest.
                    let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
                        let payload = e.payload_bytes();
                        let parsed: serde_json::Value = serde_json::from_slice(payload).ok()?;
                        parsed
                            .get(col)
                            .and_then(|v| v.as_str())
                            .map(ColumnValue::text)
                    };
                    let outcome = fanout_drv.run(&mut repl, extract).await;
                    info!(?outcome, "PgReplicator stream ended");
                });
                std::mem::forget(drv);
                info!(
                    slot = %cfg.pg_slot,
                    publication = %cfg.pg_publication,
                    "replicator: PgReplicator (real Postgres logical replication)"
                );
            }
            #[cfg(not(feature = "pg"))]
            {
                warn!(
                    "NOSTOS_REPLICATOR=pg but this binary was built without the `pg` feature. \
                     Rebuild with `cargo build -p nostos-server --features pg`. Falling back to fake."
                );
                let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(u64::MAX));
                let fanout_drv = Arc::clone(&fanout);
                let drv = tokio::spawn(async move {
                    let extract = |_e: &ReplicationEvent, _col: &str| Some(ColumnValue::Any);
                    let outcome = fanout_drv.run(&mut repl, extract).await;
                    info!(?outcome, "replicator stream ended (fallback fake)");
                });
                std::mem::forget(drv);
            }
        }
        other => {
            anyhow::bail!("unknown NOSTOS_REPLICATOR value: {other} (expected 'fake' or 'pg')");
        }
    }

    // ---- build the axum router + transport ----
    // Tenant column is enforced only under supabase-jwt auth — the anonymous
    // mode has no principal to scope with (see ADR-0011).
    let tenant_col = if cfg.sync_auth == "supabase-jwt" {
        Some(cfg.tenant_column.as_str())
    } else {
        None
    };
    let mut state_builder = SyncRouterState::new(Arc::clone(&manager), Arc::clone(&auth))
        .with_buffer(cfg.session_buffer);
    if let Some(col) = tenant_col {
        state_builder = state_builder.with_tenant_column(col);
    }

    // ---- write-back adapter (ADR-0013) ----
    // The writable-table allowlist is enforced by the transport FIRST (a
    // single trust-boundary gate), then again by PgWriteBack as
    // defense-in-depth. Under `NOSTOS_REPLICATOR=pg` (feature `pg`) we inject a
    // real PgWriteBack connected to the source; otherwise NoWriteBack returns
    // a clear "write-back requires pg replicator" error. The allowlist is
    // always set on the state so the transport's gate is uniform.
    let write_tables = nostos_infra::parse_allowlist(&cfg.write_tables);
    #[cfg(feature = "pg")]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = if cfg.replicator == "pg" {
        if cfg.pg_url.trim().is_empty() {
            anyhow::bail!(
                "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back). \
                 Set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d"
            );
        }
        info!(tables = ?write_tables, "write-back: PgWriteBack (real source)");
        Arc::new(nostos_infra::PgWriteBack::new(
            &cfg.pg_url,
            write_tables.clone(),
        ))
    } else {
        info!("write-back: NoWriteBack (fake replicator — writes return pg-required error)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    #[cfg(not(feature = "pg"))]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = {
        let _ = &cfg.pg_url; // unused without the pg feature
        info!("write-back: NoWriteBack (binary built without `pg` feature)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    state_builder = state_builder
        .with_write_back(Arc::clone(&write_back))
        .with_write_tables(write_tables);
    let state = state_builder;

    // CORS: explicit origins in production, permissive for local dev (the
    // empty-default case). Web clients need this to reach /sync from a browser.
    let cors = if cfg.cors_origins.is_empty() {
        tower_http::cors::CorsLayer::permissive()
    } else {
        let origins: Vec<_> = cfg
            .cors_origins
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        info!(?origins, "CORS: explicit origins");
        tower_http::cors::CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers(tower_http::cors::Any)
            .allow_credentials(true)
    };

    let app = axum::Router::new()
        .route(&cfg.ws_path, get(sync_handler))
        .route("/healthz", get(healthz))
        .route(
            "/metrics",
            get({
                let m = Arc::clone(&metrics);
                let store_for_gauge = Arc::clone(&store);
                move || metrics_handler(m.clone(), store_for_gauge.clone())
            }),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("invalid bind address: {}", cfg.bind))?;
    info!(%addr, ws_path = %cfg.ws_path, "Nostos sync server listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    // Graceful drain: on SIGTERM/Ctrl-C, axum stops accepting new connections
    // and waits for in-flight ones. The ack-driven slot model (ADR-0009) means
    // the last confirmed LSN is already what every live client acked — no
    // unflushed progress to lose. The replicator's keepalive loop will advance
    // the slot one final time on the next status interval before the task ends.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")))
        .try_init();
}

// ---- health + metrics endpoints (ADR: operability, T1-6/T1-7) ----

/// `GET /healthz` — liveness/readiness. Returns the live session count and
/// tier, both cheap to read (O(1) atomic on the store). A load balancer polls
/// this to decide whether to route traffic.
async fn healthz(State(state): State<SyncRouterState>) -> Json<serde_json::Value> {
    let sessions = state.manager.session_count().await;
    Json(serde_json::json!({
        "status": "ok",
        "sessions": sessions,
    }))
}

/// `GET /metrics` — Prometheus text exposition format, hand-rolled (the
/// workspace intentionally stays dependency-free for metrics per the
/// ponytail-audit cuts). Counters are aggregate throughput from the fan-out
/// service; the sessions gauge is snapshotted from the store.
async fn metrics_handler(metrics: Arc<Metrics>, store: Arc<dyn SessionStore>) -> String {
    let snap = metrics.snapshot();
    let sessions = store.len().await;
    format!(
        "# HELP cairn_events_matched_total Events whose predicate matched ≥1 session.\n\
         # TYPE cairn_events_matched_total counter\n\
         cairn_events_matched_total {matched}\n\
         # HELP cairn_events_delivered_total Events accepted by a session sink.\n\
         # TYPE cairn_events_delivered_total counter\n\
         cairn_events_delivered_total {delivered}\n\
         # HELP cairn_events_dropped_total Events dropped (full buffer / dedup / closed).\n\
         # TYPE cairn_events_dropped_total counter\n\
         cairn_events_dropped_total {dropped}\n\
         # HELP cairn_live_sessions Current live sync sessions.\n\
         # TYPE cairn_live_sessions gauge\n\
         cairn_live_sessions {sessions}\n",
        matched = snap.matched,
        delivered = snap.delivered,
        dropped = snap.dropped,
        sessions = sessions,
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received Ctrl-C, shutting down"),
        () = terminate => info!("received SIGTERM, shutting down"),
    }
}
