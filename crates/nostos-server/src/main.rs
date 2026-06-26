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
use nostos_domain::ColumnValue;
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
    /// "pg" requires the `pg` feature on nostos-infra.
    #[arg(long, env = "NOSTOS_REPLICATOR", default_value = "fake")]
    replicator: String,

    /// Log filter (RUST_LOG-style).
    #[arg(long, env = "NOSTOS_LOG", default_value = "info,nostos=debug")]
    log: String,
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
    let manager = Arc::new(SessionManager::new(Arc::clone(&store)));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    // ---- start the replicator → fan-out driver ----
    // For Week 1 the default is the FakeReplicator. It generates synthetic WAL
    // events that flow through the *real* fan-out pipeline. The PgReplicator
    // (behind the `pg` feature) lands in Week 2.
    match cfg.replicator.as_str() {
        "fake" => {
            // Generate a modest stream so the server has something to push to
            // any connected client. The benchmark drives its own FakeReplicator
            // at higher rates directly through the FanOutService.
            let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(u64::MAX));
            let fanout_drv = Arc::clone(&fanout);
            // Week-1 server extractor: the synthetic payload is opaque bytes,
            // so we cannot extract real columns. Return Any → predicates match
            // purely on table (the index prune). Real column extraction arrives
            // with the PgReplicator (which parses the tuple image).
            let drv = tokio::spawn(async move {
                let extract = |_e: &nostos_domain::ReplicationEvent,
                               _col: &str|
                 -> Option<ColumnValue> { Some(ColumnValue::Any) };
                let outcome = fanout_drv.run(&mut repl, extract).await;
                info!(?outcome, "replicator stream ended");
            });
            // The FakeReplicator with u64::MAX events never ends in practice;
            // hold the join handle so it isn't dropped.
            std::mem::forget(drv);
            info!("replicator: FakeReplicator (synthetic, unbounded)");
        }
        "pg" => {
            warn!("PG replicator requires the `pg` feature on nostos-infra; not compiled in for Week 1. Falling back to fake.");
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
