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
    /// "pg" requires building with the `pg` feature (`--features pg`).
    #[arg(long, env = "NOSTOS_REPLICATOR", default_value = "fake")]
    replicator: String,

    /// Postgres URL for the real replicator (`NOSTOS_REPLICATOR=pg`).
    #[arg(
        long,
        env = "NOSTOS_PG_URL",
        default_value = "postgresql://cairn:cairn@localhost:5433/cairn"
    )]
    pg_url: String,

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

    /// The HS256 secret used to verify Supabase JWTs at /sync. Required when
    /// `NOSTOS_SYNC_AUTH=supabase-jwt`; ignored otherwise. Matches Supabase's
    /// `JWT_SECRET` (the project's GoTrue signing key).
    #[arg(long, env = "NOSTOS_SUPABASE_JWT_SECRET", default_value = "")]
    supabase_jwt_secret: String,

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
            if cfg.supabase_jwt_secret.is_empty() {
                anyhow::bail!(
                    "NOSTOS_SYNC_AUTH=supabase-jwt requires NOSTOS_SUPABASE_JWT_SECRET to be set"
                );
            }
            info!("sync auth: supabase-jwt (HS256, tenant-enforced)");
            Arc::new(nostos_infra::SupabaseJwtAuth::new(
                cfg.supabase_jwt_secret.as_bytes().to_vec(),
            ))
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
    let fanout =
        Arc::new(FanOutService::new(Arc::clone(&store)).with_metrics(Arc::clone(&metrics)));

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
                let pg_cfg =
                    PgReplicatorConfig::from_url(&cfg.pg_url, &cfg.pg_slot, &cfg.pg_publication)
                        .context("invalid NOSTOS_PG_URL")?;
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
