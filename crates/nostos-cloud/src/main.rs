//! `nostos-cloud` — the Nostos Cloud control plane binary.
//!
//! Composition root: reads config (env), constructs the [`CloudStore`] +
//! [`CloudState`], wires the axum router, and serves the JSON API + the
//! embedded admin SPA + marketing landing page.
//!
//! ## Run
//! ```sh
//! NOSTOS_CLOUD_BIND=0.0.0.0:9100 \
//! NOSTOS_LICENSE_SECRET=change-me \
//! [STRIPE_SECRET_KEY=sk_test_...] [STRIPE_WEBHOOK_SECRET=whsec_...] \
//! [STRIPE_PRICE_PRO=price_...] \
//! cargo run -p nostos-cloud
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use nostos_cloud::license::Tier;
use nostos_cloud::routes::{checkout_ok, router, CloudState};
use nostos_cloud::store::CloudStore;
use clap::Parser;
use tracing::info;

/// Embedded marketing landing page (Task F).
const LANDING_HTML: &str = include_str!("../static/landing.html");
/// Embedded admin SPA (Task E).
const ADMIN_HTML: &str = include_str!("../static/admin.html");

#[derive(Debug, Clone, Parser)]
#[command(name = "nostos-cloud", version, about = "Nostos Cloud control plane")]
pub struct Config {
    #[arg(long, env = "NOSTOS_CLOUD_BIND", default_value = "0.0.0.0:9090")]
    bind: String,

    /// HMAC secret for signing license tokens. Required.
    #[arg(long, env = "NOSTOS_LICENSE_SECRET")]
    license_secret: String,

    /// SQLite database path. `:memory:` for ephemeral dev.
    #[arg(long, env = "NOSTOS_CLOUD_DB", default_value = "nostos-cloud.db")]
    db: String,

    /// Stripe secret key (`sk_...`). If unset, billing endpoints 503.
    #[arg(long, env = "STRIPE_SECRET_KEY")]
    stripe_secret_key: Option<String>,

    /// Stripe webhook signing secret (`whsec_...`). Required to accept webhooks.
    #[arg(long, env = "STRIPE_WEBHOOK_SECRET")]
    stripe_webhook_secret: Option<String>,

    /// Stripe Price ids per tier (env: STRIPE_PRICE_PRO, STRIPE_PRICE_SCALE, ...).
    #[arg(long, env = "STRIPE_PRICE_PRO")]
    price_pro: Option<String>,
    #[arg(long, env = "STRIPE_PRICE_SCALE")]
    price_scale: Option<String>,
    #[arg(long, env = "STRIPE_PRICE_ENTERPRISE")]
    price_enterprise: Option<String>,

    /// Public base URL this cloud is reachable at (for Stripe redirect URLs).
    #[arg(
        long,
        env = "NOSTOS_CLOUD_PUBLIC_URL",
        default_value = "http://localhost:9100"
    )]
    public_base_url: String,

    /// Supabase project JWT secret (HS256). When set, `Authorization: Bearer
    /// <jwt>` is accepted on authed routes alongside the session cookie. When
    /// unset, only the session-cookie (OSS self-host) path is accepted.
    #[arg(long, env = "SUPABASE_JWT_SECRET")]
    supabase_jwt_secret: Option<String>,

    #[arg(long, env = "NOSTOS_LOG", default_value = "info,nostos_cloud=debug")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    init_tracing(&cfg.log);

    let store = CloudStore::open(&cfg.db).context("open cloud db")?;
    let mut price_ids = Vec::new();
    if let Some(p) = cfg.price_pro {
        price_ids.push((Tier::Pro, p));
    }
    if let Some(p) = cfg.price_scale {
        price_ids.push((Tier::Scale, p));
    }
    if let Some(p) = cfg.price_enterprise {
        price_ids.push((Tier::Enterprise, p));
    }

    let state = CloudState {
        store,
        license_secret: Arc::from(cfg.license_secret.as_bytes()),
        stripe_secret_key: cfg.stripe_secret_key,
        stripe_webhook_secret: cfg.stripe_webhook_secret,
        price_ids: Arc::new(price_ids),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("build http client")?,
        public_base_url: cfg.public_base_url.clone(),
        jwt_verifier: cfg.supabase_jwt_secret.as_ref().map(|s| {
            Arc::new(nostos_cloud::auth::Hs256Verifier::new(s.as_bytes().to_vec()))
                as Arc<dyn nostos_cloud::auth::JwtVerifier>
        }),
    };

    let app = Router::new()
        // static: marketing landing at /, admin SPA at /admin
        .route("/", get(|| async { Html(LANDING_HTML) }))
        .route("/admin", get(|| async { Html(ADMIN_HTML) }))
        .route("/admin/", get(|| async { Html(ADMIN_HTML) }))
        .route("/v1/projects/:id/checkout/ok", get(checkout_ok))
        .merge(router(state))
        .layer(tower_cookies::CookieManagerLayer::new());

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("invalid bind: {}", cfg.bind))?;
    info!(%addr, "Nostos Cloud control plane listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
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
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install term handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => info!("Ctrl-C, shutting down"),
        () = term => info!("SIGTERM, shutting down"),
    }
}
