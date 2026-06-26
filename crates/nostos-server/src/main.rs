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
use axum::routing::get;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use clap::Parser;
use tower_http::trace::TraceLayer;
use tracing::info;
#[cfg(not(feature = "pg"))]
use tracing::warn;

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
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

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
    let state = SyncRouterState::new(Arc::clone(&manager)).with_buffer(cfg.session_buffer);
    let app = axum::Router::new()
        .route(&cfg.ws_path, get(sync_handler))
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
