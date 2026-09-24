//! # nostos-pushd — the composition root binary (ADR-0038 §1, plan 1.1)
//!
//! Reads config, builds the registry store + rails from the shared env
//! contract, spawns the coalescer and the retention sweeper, and binds
//! axum. The only place the concrete adapters are wired — swapping the
//! SQLite store for the v1.1 PgStore is a construction-site change here,
//! zero edits in the route/coalescer layers (the hexagonal payoff).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};

use nostos_push::auth::ApiKeys;
use nostos_push::coalescer::{self, CoalescerLimits};
use nostos_push::config::{Config, DEFAULT_DB, LEGACY_DB};
use nostos_push::limit::SendRateLimiter;
use nostos_push::rail::Rails;
use nostos_push::store::SqliteStore;
// The v1.1 Postgres registry — only imported in `pg` builds.
#[cfg(feature = "pg")]
use nostos_push::store::PgStore;
use nostos_push::{build_router, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cfg = nostos_infra::env::parse::<Config>();
    // ADR-0046: a default-path deployment keeps its pre-rename registry.
    if cfg.db == DEFAULT_DB {
        let db = nostos_infra::config_path::resolve(std::path::Path::new(""), DEFAULT_DB, LEGACY_DB);
        cfg.db = db.display().to_string();
    }
    init_tracing();

    // Fail fast (pin 0.2): a daemon with no usable key list must never
    // start serving an auth surface that can only answer 401.
    let mut api_keys = ApiKeys::parse(&cfg.api_keys).context("invalid NOSTOS_PUSHD_API_KEYS")?;
    info!(tenants = api_keys.len(), "API keys loaded from env");

    // Registry store (ADR-0038 §4): Postgres when NOSTOS_PUSHD_DATABASE_URL
    // is set (v1.1, feature `pg`), the SQLite default otherwise. Either
    // way the Arc<dyn Store> seam means zero edits below this line.
    #[cfg(feature = "pg")]
    let store: Arc<dyn nostos_push::store::Store> = match cfg.database_url.as_deref() {
        Some(url) => Arc::new(
            PgStore::open(url)
                .await
                .context("opening the Postgres pushd registry (NOSTOS_PUSHD_DATABASE_URL)")?,
        ),
        None => sqlite_store(&cfg)?,
    };
    // The env var names a store this build cannot provide — fail fast with
    // the fix rather than silently downgrading to SQLite.
    #[cfg(not(feature = "pg"))]
    let store: Arc<dyn nostos_push::store::Store> = match cfg.database_url.as_deref() {
        Some(_) => anyhow::bail!(
            "NOSTOS_PUSHD_DATABASE_URL is set, but this nostos-pushd was built without the \
             Postgres registry; rebuild with `--features pg` or unset it to use SQLite \
             (NOSTOS_PUSHD_DB)"
        ),
        None => sqlite_store(&cfg)?,
    };

    // Rails configure themselves from their own env vars (plan task 1.7);
    // a misconfigured rail aborts the boot rather than failing per-send.
    let rails = Rails::from_env().context("push rail configuration")?;
    let health = rails.health();
    // Log the registry kind, never the URL (it can carry credentials).
    let db_desc = match cfg.database_url.as_deref() {
        Some(_) => "postgres (NOSTOS_PUSHD_DATABASE_URL)",
        None => cfg.db.as_str(),
    };
    info!(
        apns = health.apns,
        fcm = health.fcm,
        webpush = health.webpush,
        db = %db_desc,
        debounce_ms = cfg.debounce_ms,
        receipt_retention_secs = cfg.receipt_retention_secs,
        "nostos-pushd starting"
    );

    // Store-backed keys (B2): hashed-at-rest rows in the registry (managed
    // via `nostos push key add/list/revoke`), merged OVER the env bootstrap —
    // the store wins on a tenant collision (managed surface). Per-tenant
    // limit overrides ride the same rows into the send limiter.
    match store.list_api_keys().await {
        Ok(stored) if stored.is_empty() => {}
        Ok(stored) => {
            info!(keys = stored.len(), "API keys loaded from registry store");
            api_keys.merge_stored(stored);
        }
        Err(e) => warn!(error = %e, "registry API-key load failed — continuing with env keys"),
    }
    let limiter_overrides = api_keys.limit_overrides();
    if !limiter_overrides.is_empty() {
        info!(
            tenants = limiter_overrides.len(),
            "per-tenant send limits active"
        );
    }

    coalescer::spawn_retention_sweeper(Arc::clone(&store), cfg.receipt_retention_secs);
    let limits = CoalescerLimits {
        pending_keys_max: cfg.pending_keys_max,
        losers_max: cfg.losers_max,
    };
    let retry = coalescer::RetryPolicy {
        max_attempts: cfg.retry_max_attempts,
        delay: Duration::from_millis(cfg.retry_delay_ms),
    };
    let coalescer = coalescer::spawn_coalescer(
        Arc::clone(&store),
        rails.clone(),
        Duration::from_millis(cfg.debounce_ms),
        limits,
        retry,
    );
    let state = AppState {
        store,
        rails,
        api_keys,
        sender: coalescer.tx.clone(),
        send_limiter: Arc::new(SendRateLimiter::with_overrides(
            cfg.send_rate_per_sec,
            cfg.send_burst,
            limiter_overrides,
        )),
        gate: coalescer.gate.clone(),
    };

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("invalid bind address: {}", cfg.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "nostos-pushd listening");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

/// The default SQLite registry (pin 0.3) — the untouched v1.0 behavior.
fn sqlite_store(cfg: &Config) -> anyhow::Result<Arc<dyn nostos_push::store::Store>> {
    Ok(Arc::new(SqliteStore::open(&cfg.db).with_context(|| {
        format!("opening pushd database {}", cfg.db)
    })?))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();
}

/// SIGTERM / Ctrl-C — the coalescer's channel closes with the state (all
/// sender clones dropped at process exit), which triggers its final drain.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
