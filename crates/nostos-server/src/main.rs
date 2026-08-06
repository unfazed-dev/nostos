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
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use nostos_application::ports::{Metrics, SchemaDescriptor, SchemaSource, SessionStore, TableStat};
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent, SyncMode};
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

    /// Op-log writer's bounded internal channel depth (ADR-0025 slice 2). The
    /// fan-out loop `try_send`s each event into this buffer; a background task
    /// drains + flushes to `cairn_oplog`. On full, the entry is dropped (the
    /// resume path falls back to snapshot-reconcile for the gap — correct, but
    /// a capacity signal). Default 4096; raise if
    /// `cairn_oplog_dropped_total` is non-zero under sustained load. Only
    /// meaningful under `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_OPLOG_BUFFER", default_value_t = 4096)]
    oplog_buffer: usize,

    /// Op-log retention window in seconds (ADR-0025 slice 5). Rows older than
    /// this are aged out by the compactor. A client whose offline gap exceeds
    /// the window falls back to snapshot-reconcile (slice 1, the safety net).
    /// Default 1h.
    #[arg(long, env = "NOSTOS_OPLOG_RETENTION_SECS", default_value_t = 3600)]
    oplog_retention_secs: u64,

    /// Op-log compaction tick period in seconds (ADR-0025 slice 5). The
    /// compactor collapses duplicate ops per (table_name, pk) + ages out rows
    /// past the retention window. Default 5min.
    #[arg(long, env = "NOSTOS_OPLOG_COMPACT_INTERVAL_SECS", default_value_t = 300)]
    oplog_compact_interval_secs: u64,

    /// Replicator mode: "fake" (synthetic generator) or "pg" (real Postgres).
    /// "pg" requires the `pg` feature, which is on by default (disable with
    /// `--no-default-features`). Runtime default stays "fake" so zero-setup
    /// `cargo run` keeps working.
    #[arg(long, env = "NOSTOS_REPLICATOR", default_value = "fake")]
    replicator: String,

    /// Fake-replicator emission rate, events/second. `0` = unbounded.
    ///
    /// A10: the default is *paced*, not unbounded. An unbounded synthetic
    /// stream is pure load with no observer — it saturated any interactive
    /// session that outlived a few seconds (ADR-0027 finding). The benchmark
    /// builds its own config (`nostos-bench`), so the measured ceiling is
    /// untouched; set `0` here to firehose deliberately.
    #[arg(long, env = "NOSTOS_FAKE_EPS", default_value_t = 20)]
    fake_events_per_sec: u64,

    /// Fake-replicator distinct primary keys. `0` = monotonic (grows forever).
    ///
    /// Client apply is an upsert on `(table, pk)`, so a bounded key space
    /// bounds the *table* — which keeps a full-table watch snapshot O(1) in
    /// session length. Pacing alone only slows the growth.
    #[arg(long, env = "NOSTOS_FAKE_KEYS", default_value_t = 50)]
    fake_distinct_keys: u64,

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

    /// Comma-separated `table:column` pairs naming the JSONB columns that hold
    /// add-wins OR-sets (ADR-0030). Writes to these tables merge element-wise
    /// server-side instead of clobbering; an OR-set write to an unconfigured
    /// table is rejected client-side. Empty (default) = no OR-set columns (LWW
    /// only). Example: `tasks:tags,notes:labels`. Only meaningful under
    /// `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_OR_SET_COLUMNS", default_value = "")]
    or_set_columns: String,

    /// Path to the sync-rules file (ADR-0031). Missing file = `all` mode.
    #[arg(long, env = "NOSTOS_RULES_FILE", default_value = "nostos_rules.toml")]
    rules_file: String,

    /// Coalesce the per-event ack-progress (slot-advance) scan: recompute the
    /// slowest acked LSN every N events instead of every event. `1` (default) =
    /// exact ADR-0009 per-event cadence. `>1` cuts the O(sessions) scan N× — the
    /// lever for >5k-client deploys (safe: acks are monotonic, so a cached min
    /// never overshoots the safe-to-flush LSN; at most N events of extra WAL
    /// retention). Example: `16` or `32` for high client counts.
    #[arg(long, env = "NOSTOS_ACK_PROGRESS_INTERVAL", default_value = "1")]
    ack_progress_interval: u32,

    /// Logical-replication slot name.
    #[arg(long, env = "NOSTOS_PG_SLOT", default_value = "cairn_slot")]
    pg_slot: String,

    /// Publication name.
    #[arg(long, env = "NOSTOS_PG_PUBLICATION", default_value = "cairn_pub")]
    pg_publication: String,

    /// Log filter (RUST_LOG-style).
    #[arg(long, env = "NOSTOS_LOG", default_value = "info,nostos=debug")]
    log: String,

    /// Licensed tier for the concurrent-device cap (the OSS / fallback path).
    /// OSS self-host defaults to `enterprise` (unlimited); a managed Cloud deploy
    /// usually presents a signed `NOSTOS_LICENSE` instead (see below), in which
    /// case this value is ignored. One of: hobby, pro, scale, enterprise.
    #[arg(long, env = "NOSTOS_TIER", default_value = "enterprise")]
    tier: String,

    /// Signed license token from Nostos Cloud (`<payload>.<sig>`). When present,
    /// the server verifies it with `NOSTOS_LICENSE_SECRET`; the token's tier +
    /// `device_cap` then become authoritative (managed mode). Empty (default) =
    /// OSS self-host, and `NOSTOS_TIER` is used instead. A presented-but-invalid
    /// license is fatal — the server refuses to start rather than silently
    /// downgrading to the unlimited OSS default (ADR-0006 trust boundary).
    #[arg(long, env = "NOSTOS_LICENSE", default_value = "", hide = true)]
    license: String,

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
    // ---- resolve the licensed tier (ADR-0006 trust boundary) ----
    // OSS self-host: no NOSTOS_LICENSE → fall back to NOSTOS_TIER (default
    // `enterprise` = unlimited). Managed deploy: presents a signed
    // NOSTOS_LICENSE; the token's tier + device_cap are authoritative, and a
    // presented-but-invalid token is FATAL (no silent downgrade to OSS default).
    let fallback_tier = match cfg.tier.as_str() {
        "hobby" => nostos_domain::Tier::Hobby,
        "pro" => nostos_domain::Tier::Pro,
        "scale" => nostos_domain::Tier::Scale,
        _ => nostos_domain::Tier::Enterprise,
    };
    // NOSTOS_LICENSE_SECRET is env-only by design (NOT a clap flag): it signs
    // every license a cloud deploy mints, so it must never land on argv / `ps`.
    let license_secret = std::env::var("NOSTOS_LICENSE_SECRET").unwrap_or_default();
    let entitlement =
        nostos_license::resolve_entitlement(&cfg.license, license_secret.as_bytes(), fallback_tier)
            .context("NOSTOS_LICENSE verification failed — refusing to start")?;
    info!(
        tier = ?entitlement.tier,
        devices_cap = entitlement.device_cap,
        project_id = %entitlement.project_id,
        licensed = !cfg.license.is_empty(),
        "entitlement resolved"
    );
    let manager = Arc::new(SessionManager::with_device_cap(
        Arc::clone(&store),
        entitlement.tier,
        entitlement.device_cap,
    ));

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
    // Op-log writer (ADR-0025 slice 2): persisted op-log for in-window
    // reconnect replay. Only under `NOSTOS_REPLICATOR=pg` — the fake replicator
    // has no source database to durably write to (the bench drives a
    // RecordingOpLogWriter directly). Shares the metrics handle so `/metrics`
    // surfaces the drop + flush-failure counters.
    #[cfg(feature = "pg")]
    let op_log: Option<Arc<dyn nostos_application::ports::OpLogWriter>> = if cfg.replicator == "pg" {
        Some(Arc::new(nostos_infra::PgOpLogWriter::new(
            &cfg.pg_url,
            Some(cfg.tenant_column.clone()),
            cfg.oplog_buffer,
            Some(Arc::clone(&metrics)),
        )))
    } else {
        None
    };
    // Retain a clone of the op-log writer so we can drain it on graceful
    // shutdown (after axum returns) — the original Arc moves into the
    // FanOutService below. ADR-0025 slice-6 follow-up.
    #[cfg(feature = "pg")]
    let op_log_shutdown = op_log.clone();

    // Op-log compactor (ADR-0025 slice 5): bounds cairn_oplog growth via
    // periodic collapse (keep latest op per (table_name, pk) — a trailing
    // delete survives as a tombstone) + retention (age out old rows). Only
    // under NOSTOS_REPLICATOR=pg. Detached background task (runs until process
    // exit). The compactor's swept-row count surfaces in `/metrics`.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let _compactor = nostos_infra::PgOpLogCompactor::new(
            &cfg.pg_url,
            cfg.oplog_retention_secs,
            cfg.oplog_compact_interval_secs,
            Arc::clone(&metrics),
        );
    }

    let fanout = Arc::new({
        let builder = FanOutService::new(Arc::clone(&store))
            .with_metrics(Arc::clone(&metrics))
            .with_eviction(eviction);
        // Attach the op-log when built (cfg-gated so the non-pg build never
        // references the (absent) PgOpLogWriter type).
        #[cfg(feature = "pg")]
        let builder = match op_log {
            Some(w) => builder.with_op_log(w),
            None => builder,
        };
        builder.with_ack_progress_every(cfg.ack_progress_interval)
    });

    // ---- start the replicator → fan-out driver ----
    // The extractor lifts named columns out of an event's payload so predicates
    // (which match on column equality) can be evaluated. For the FakeReplicator
    // the payload is opaque bytes, so we return `Any` (table-only matching). For
    // the PgReplicator the payload is a small JSON object {col:val}, so we parse
    // it and return real values — enabling filter predicates like org_id=acme.
    //
    // C6 (late-append P1): retain the pg replicator's JoinHandle so we can
    // abort it BEFORE draining the op-log at shutdown — prevents the silent
    // late-append-into-final-flush race (ghost row on matching-epoch reconnect;
    // see oplog.rs `drain_boundary_late_append_during_final_flush_is_lost`).
    // The fake branch intentionally detaches (`mem::forget`): it's the bench
    // path, the op-log is pg-only, and FakeReplicator has no producer window.
    #[cfg(feature = "pg")]
    let mut repl_handle: Option<tokio::task::JoinHandle<()>> = None;
    match cfg.replicator.as_str() {
        "fake" => {
            let mut repl = FakeReplicator::new(
                FakeReplicatorConfig::small(u64::MAX)
                    .paced(cfg.fake_events_per_sec)
                    .recycling_keys(cfg.fake_distinct_keys),
            );
            let fanout_drv = Arc::clone(&fanout);
            let drv = tokio::spawn(async move {
                let extract = |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> {
                    Some(ColumnValue::Any)
                };
                let outcome = fanout_drv.run(&mut repl, extract).await;
                info!(?outcome, "replicator stream ended");
            });
            std::mem::forget(drv);
            info!(
                events_per_sec = cfg.fake_events_per_sec,
                distinct_keys = cfg.fake_distinct_keys,
                "replicator: FakeReplicator (synthetic; 0 = unbounded)"
            );
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
                let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
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
                repl_handle = Some(drv);
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
                let mut repl = FakeReplicator::new(
                    FakeReplicatorConfig::small(u64::MAX)
                        .paced(cfg.fake_events_per_sec)
                        .recycling_keys(cfg.fake_distinct_keys),
                );
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
        .with_buffer(cfg.session_buffer)
        .with_metrics(Arc::clone(&metrics));
    if let Some(col) = tenant_col {
        state_builder = state_builder.with_tenant_column(col);
    }

    // ---- sync-rules ruleset (ADR-0031) ----
    // A malformed/invalid file must not silently degrade to "sync everything"
    // — bail loudly instead of falling back.
    let ruleset = match nostos_infra::rules_file::load(std::path::Path::new(&cfg.rules_file)) {
        Ok(Some(raw)) => {
            let compiled = nostos_application::ActiveRuleset::compile(&raw)
                .context("nostos_rules.toml failed to compile")?;
            info!(
                sync_mode = compiled.mode().as_str(),
                tables = compiled.synced_tables().len(),
                checksum = format!("{:x}", compiled.checksum()),
                "sync rules loaded"
            );
            compiled
        }
        Ok(None) => {
            info!("no nostos_rules.toml found; sync_mode=all (zero-config default)");
            nostos_application::ActiveRuleset::all_mode()
        }
        Err(e) => return Err(e).context("failed to load nostos_rules.toml"),
    };
    let ruleset_mode = ruleset.mode();
    let (rules_tx, rules_changed) = tokio::sync::watch::channel(ruleset.checksum());
    let rules_shared = Arc::new(tokio::sync::RwLock::new(ruleset));
    state_builder = state_builder.with_rules(Arc::clone(&rules_shared), rules_changed);

    // ---- `all`-mode startup warning (ADR-0031, Task 13) ----
    // sync_mode = "all" means every replicated row reaches every authorised
    // client (still tenant-scoped — see the principal-scoping path — but
    // unscoped within a tenant). That's the right zero-config default, but an
    // operator who never opts into narrower rules deserves a loud heads-up
    // rather than finding out from an OOM. Row counts are estimates
    // (`pg_class.reltuples`, never `count(*)`) so this stays cheap at boot.
    if ruleset_mode == SyncMode::All {
        let stats: Vec<TableStat> = {
            #[cfg(feature = "pg")]
            {
                if cfg.replicator == "pg" {
                    use nostos_application::ports::TableStatsSource;
                    let src = nostos_infra::PgTableStats::new(&cfg.pg_url, &cfg.pg_publication);
                    match src.table_stats().await {
                        Ok(stats) => stats,
                        Err(e) => {
                            // A stats-fetch failure must not abort boot — the
                            // warning still fires, just without numbers.
                            warn!("could not estimate table sizes: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    // The fake replicator has no database to introspect.
                    Vec::new()
                }
            }
            #[cfg(not(feature = "pg"))]
            {
                Vec::new()
            }
        };
        warn!("{}", format_all_mode_warning(&stats));
    }

    // ---- write-back adapter (ADR-0013) ----
    // The writable-table allowlist is enforced by the transport FIRST (a
    // single trust-boundary gate), then again by PgWriteBack as
    // defense-in-depth. Under `NOSTOS_REPLICATOR=pg` (feature `pg`) we inject a
    // real PgWriteBack connected to the source; otherwise NoWriteBack returns
    // a clear "write-back requires pg replicator" error. The allowlist is
    // always set on the state so the transport's gate is uniform.
    let write_tables = nostos_infra::parse_allowlist(&cfg.write_tables);
    let or_set_columns = nostos_infra::parse_or_set_columns(&cfg.or_set_columns);
    #[cfg(feature = "pg")]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = if cfg.replicator == "pg" {
        if cfg.pg_url.trim().is_empty() {
            anyhow::bail!(
                "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back). \
                 Set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d"
            );
        }
        info!(tables = ?write_tables, or_set_columns = ?or_set_columns, "write-back: PgWriteBack (real source)");
        Arc::new(
            nostos_infra::PgWriteBack::new(&cfg.pg_url, write_tables.clone())
                .with_or_set_columns(or_set_columns.clone()),
        )
    } else {
        info!("write-back: NoWriteBack (fake replicator — writes return pg-required error)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    #[cfg(not(feature = "pg"))]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = {
        let _ = &cfg.pg_url; // unused without the pg feature
        let _ = &or_set_columns; // unused without the pg feature
        info!("write-back: NoWriteBack (binary built without `pg` feature)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    state_builder = state_builder
        .with_write_back(Arc::clone(&write_back))
        .with_write_tables(write_tables);

    // ---- snapshot-on-subscribe adapter (ADR-0014) ----
    // Under `NOSTOS_REPLICATOR=pg` (feature `pg`) inject a real `PgSnapshotter`
    // so a freshly-subscribing client receives the table's pre-existing rows
    // before live fan-out (PowerSync parity — closes the "Flutter app shows 1
    // of 5 rows" gap). Otherwise `snapshotter` stays `None` (the default set in
    // `SyncRouterState::new`) and subscribe-time snapshots are skipped.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        // pg_url is already known non-empty here — the write-back block above
        // bailed on an empty NOSTOS_PG_URL under the same `replicator == "pg"`.
        let snapshotter: Arc<dyn nostos_application::ports::SnapshotSource> =
            Arc::new(nostos_infra::PgSnapshotter::new(&cfg.pg_url));
        state_builder = state_builder.with_snapshotter(snapshotter);
        info!("snapshot-on-subscribe: PgSnapshotter (real source)");
    }

    // ---- op-log replay-on-reconnect adapter (ADR-0025 slice 4b) ----
    // Under `NOSTOS_REPLICATOR=pg` inject a `PgOpLogReader` so a reconnecting
    // client with a matching epoch + an in-window `resume_lsn` gets its offline
    // gap replayed from `cairn_oplog` instead of a full snapshot. Otherwise
    // `oplog_reader` stays `None` → reconnect always takes the snapshot path
    // (slice-1 reconcile remains the correctness floor either way).
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let reader: Arc<dyn nostos_application::ports::OpLogSource> =
            Arc::new(nostos_infra::PgOpLogReader::new(&cfg.pg_url));
        state_builder = state_builder.with_oplog_reader(reader);
        info!("op-log replay: PgOpLogReader (real source)");
    }

    // Guard (Fix A): NOSTOS_PG_URL set while replicator != "pg" is almost always
    // a misconfiguration — snapshot-on-subscribe (ADR-0014) stays OFF and a
    // freshly-subscribing client silently receives NONE of the table's
    // pre-existing rows (the "5 in Postgres, only 1 shows in the app" symptom).
    // The common cause is a fixture/.env that sets NOSTOS_PG_URL but omits
    // NOSTOS_REPLICATOR=pg. Fail loudly at startup instead of degrading silently.
    if cfg.replicator != "pg" && !cfg.pg_url.trim().is_empty() {
        // C10: BAIL (not warn) — a warn still let the server start degraded,
        // causing the silent "connected but lists empty" symptom (snapshot-
        // on-subscribe ADR-0014 stays OFF; clients receive no pre-existing
        // rows). Failing loudly at startup makes the misconfiguration
        // undiscoverable-by-accident. See docs/OPERATING.md §1.1(a).
        anyhow::bail!(
            "NOSTOS_PG_URL is set but NOSTOS_REPLICATOR={:?} is not 'pg' — \
             snapshot-on-subscribe (ADR-0014) is OFF, so clients would silently \
             receive none of the table's pre-existing rows on connect. \
             Set NOSTOS_REPLICATOR=pg, or unset NOSTOS_PG_URL.",
            cfg.replicator
        );
    }

    // ---- typed-schema endpoint adapter (WS1) ----
    // Under `NOSTOS_REPLICATOR=pg` inject a `PgSchemaSource` so `GET /schema`
    // can serve the publication's tables/columns/affinities for the Flutter
    // SDK's auto-schema (PowerSync-style redesign, Option-C). Otherwise
    // `schema_source` stays `None` and `GET /schema` returns 404.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let schema_source: Arc<dyn SchemaSource> = Arc::new(nostos_infra::PgSchemaSource::new(
            &cfg.pg_url,
            &cfg.pg_publication,
        ));
        state_builder = state_builder.with_schema_source(schema_source);
        info!(publication = %cfg.pg_publication, "schema endpoint: PgSchemaSource");
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
        // WS1: typed schema for client auto-schema. v2: add auth here (and to
        // /rules below) if a managed deploy wants to hide publication metadata.
        .route("/schema", get(schema))
        // ADR-0031: the active ruleset, read-only. Task 20 adds `.put()` here
        // behind an admin token; GET stays on the same v1-unauthenticated /
        // v2-gated-together policy as /schema (see the note above).
        .route("/rules", get(rules_handler))
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

    // ---- sync-rules hot reload (ADR-0031 D3, Task 14) ----
    // No engine restart: the watcher polls the same file loaded at boot and,
    // on an actual checksum change, swaps the shared ruleset + notifies every
    // live socket's cheap `watch::Receiver::changed()` arm (transport.rs).
    let (rules_shutdown_tx, rules_shutdown_rx) = tokio::sync::watch::channel(false);
    let rules_watch_handle = tokio::spawn(watch_rules(
        std::path::PathBuf::from(&cfg.rules_file),
        rules_shared,
        std::time::Duration::from_secs(5),
        rules_shutdown_rx,
        rules_tx,
    ));

    // Graceful drain: on SIGTERM/Ctrl-C, axum stops accepting new connections
    // and waits for in-flight ones. The ack-driven slot model (ADR-0009) means
    // the last confirmed LSN is already what every live client acked — no
    // unflushed progress to lose. The replicator's keepalive loop will advance
    // the slot one final time on the next status interval before the task ends.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    // ADR-0026 fix A (C6 late-append P1): stop ingesting new replication
    // changes BEFORE draining the op-log, so no append can race into the final
    // flush and be silently lost (ghost row on matching-epoch reconnect). The
    // detached replicator is the producer; the op-log drain is the consumer —
    // stop the producer first.
    #[cfg(feature = "pg")]
    if let Some(h) = repl_handle.take() {
        h.abort();
    }
    // ADR-0025 slice-6 follow-up: drain the op-log writer's in-flight batch so
    // a SIGTERM doesn't drop the last ≤BATCH_MAX entries mid-INSERT (those
    // clients would otherwise fall back to snapshot-reconcile on reconnect).
    #[cfg(feature = "pg")]
    {
        if let Some(w) = op_log_shutdown {
            w.shutdown().await;
        }
    }
    let _ = rules_shutdown_tx.send(true);
    let _ = rules_watch_handle.await;
    Ok(())
}

/// Poll `path` every `poll_interval` and swap the shared ruleset when its
/// canonical checksum changes (ADR-0031 D3 — no engine restart). A malformed
/// or unreadable file is logged and skipped: the previous ruleset stays
/// authoritative, never silently widened. `rules_tx` both stores the current
/// checksum (read back each tick to detect no-op reloads) and wakes every
/// live socket's `write_loop` select arm (`crates/nostos-infra/src/
/// transport.rs`) so it can re-verify its own subscriptions.
async fn watch_rules(
    path: std::path::PathBuf,
    rules: Arc<tokio::sync::RwLock<nostos_application::ActiveRuleset>>,
    poll_interval: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    rules_tx: tokio::sync::watch::Sender<u64>,
) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(poll_interval) => {}
            res = shutdown.changed() => {
                match res {
                    Ok(()) if *shutdown.borrow() => return,
                    Ok(()) => continue,
                    Err(_) => return, // sender dropped; nothing left to watch for
                }
            }
        }
        let loaded = match nostos_infra::rules_file::load(&path) {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                warn!(path = %path.display(), "nostos_rules.toml missing on reload poll; keeping previous ruleset");
                continue;
            }
            Err(e) => {
                warn!(error = %e, path = %path.display(), "nostos_rules.toml reload failed to load; keeping previous ruleset");
                continue;
            }
        };
        let compiled = match nostos_application::ActiveRuleset::compile(&loaded) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "nostos_rules.toml reload failed to compile; keeping previous ruleset");
                continue;
            }
        };
        let new_checksum = compiled.checksum();
        if new_checksum == *rules_tx.borrow() {
            continue; // canonical form unchanged (e.g. only whitespace edited)
        }
        info!(
            sync_mode = compiled.mode().as_str(),
            tables = compiled.synced_tables().len(),
            old_checksum = format!("{:x}", *rules_tx.borrow()),
            new_checksum = format!("{:x}", new_checksum),
            "sync rules reloaded"
        );
        // Swap BEFORE notifying: a session woken by `rules_tx.send` must see
        // the new ruleset when it reads `rules`, never a stale one.
        *rules.write().await = compiled;
        let _ = rules_tx.send(new_checksum);
    }
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")))
        .try_init();
}

// ---- `all`-mode startup warning (ADR-0031, Task 13) ----

/// Render the `sync_mode = "all"` guardrail banner. Pure formatting so it is
/// testable without a database. `None` row estimates render as "unknown"
/// (Postgres reports `reltuples = -1` for a never-analyzed table).
fn format_all_mode_warning(stats: &[TableStat]) -> String {
    let mut lines = vec![
        "WARNING: sync_mode = \"all\" — every replicated row reaches every authorised client."
            .to_string(),
    ];

    // Unknown estimates are never folded into `total` — a missing planner
    // stat must not silently masquerade as zero rows.
    let mut total: u64 = 0;
    for stat in stats {
        let table = &stat.table;
        let line = match stat.estimated_rows {
            Some(n) => {
                total += n;
                let est = format_thousands(n);
                format!("  {table}    ~{est} rows")
            }
            None => format!("  {table}    unknown rows (never analyzed)"),
        };
        lines.push(line);
    }

    let count = stats.len();
    let plural = if count == 1 { "" } else { "s" };
    let total_fmt = format_thousands(total);
    lines.push(format!(
        "  {count} table{plural}, ~{total_fmt} rows estimated."
    ));
    lines.push("  This is the zero-config development default. For production, run".to_string());
    lines.push("  `nostos rules init` and switch sync_mode to \"toggles\".".to_string());

    lines.join("\n")
}

/// Thousands-grouped decimal rendering (`12400` → `"12,400"`), for the
/// `all`-mode startup banner's row-count estimates.
fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
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

/// `GET /schema` — the publication's typed schema (WS1): tables, columns, and
/// SQLite affinities, so the Flutter SDK can auto-build typed tables without a
/// hand-written `Schema`. v1 is unauthenticated (schema is publication-wide
/// metadata, not tenant-scoped rows; row isolation is the read-path predicate's
/// job — ADR-0011/0018). Returns 404 when no `SchemaSource` is wired (the fake
/// / no-`pg` path) and 503 on a transient backend error.
async fn schema(
    State(state): State<SyncRouterState>,
) -> Result<Json<SchemaDescriptor>, StatusCode> {
    let src = state.schema_source.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    src.fetch().await.map(Json).map_err(|e| {
        warn!(error = %e, "schema fetch failed");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

/// `GET /rules` — what the server is ENFORCING right now (ADR-0031). The read
/// half of the route; Task 20 adds `.put()` on the same path, so this handler
/// is read-only, the *route* is not.
///
/// Auth parity with `/schema`: v1 is unauthenticated. `/rules` discloses
/// strictly less than `/schema` already does — table names plus scope column
/// text, no columns, no types — so it matches `/schema`'s policy exactly; if
/// `/schema` is ever gated, gate `/rules` identically in the same commit.
///
/// Never echoes claim *values* — `scope_text` only ever renders column
/// names, operators, and `claims.<name>` references. No principal data, no
/// row counts.
async fn rules_handler(State(state): State<SyncRouterState>) -> Json<serde_json::Value> {
    let ruleset = state.rules.read().await;
    let slot_epoch = state
        .metrics
        .slot_epoch
        .load(std::sync::atomic::Ordering::Relaxed);
    let checksum = ruleset.checksum();
    let sync_epoch = nostos_domain::compose_sync_epoch(slot_epoch, checksum);
    let tables: Vec<_> = ruleset
        .synced_tables()
        .into_iter()
        .map(|table| {
            serde_json::json!({
                "table": table,
                "scope": ruleset.scope_text(table).unwrap_or_default(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "sync_mode": ruleset.mode().as_str(),
        "checksum": format!("0x{checksum:x}"),
        "sync_epoch": format!("0x{sync_epoch:x}"),
        "tables": tables,
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
         # HELP cairn_events_faulted_total Delivery tasks that faulted (panicked / cancelled) — a server fault, NOT slow-client backpressure. Kept distinct from cairn_events_dropped_total so a panic is never mis-attributed as a client drop in the 0%-drops figure. Alert on any increase.\n\
         # TYPE cairn_events_faulted_total counter\n\
         cairn_events_faulted_total {faulted}\n\
         # HELP cairn_live_sessions Current live sync sessions.\n\
         # TYPE cairn_live_sessions gauge\n\
         cairn_live_sessions {sessions}\n\
         # HELP cairn_slot_wal_status Replication-slot health gauge (0=healthy, 1=reserved, 2=lost, 3=recreated). 'lost' means silent-data-loss risk; alert on cairn_slot_recreated_total. ADR-0009.\n\
         # TYPE cairn_slot_wal_status gauge\n\
         cairn_slot_wal_status {slot_wal_status}\n\
         # HELP cairn_replication_lag_bytes Current WAL lsn minus slot restart_lsn, in bytes. 0 when slot is missing/unknown.\n\
         # TYPE cairn_replication_lag_bytes gauge\n\
         cairn_replication_lag_bytes {replication_lag_bytes}\n\
         # HELP cairn_slot_recreated_total Number of times the replication slot was dropped + re-created from a missing/lost state. Each increment is a potential silent-data-loss window; alert on any increase.\n\
         # TYPE cairn_slot_recreated_total counter\n\
         cairn_slot_recreated_total {slot_recreated_total}\n\
         # HELP cairn_oplog_dropped_total Op-log entries dropped (writer buffer full). The resume path falls back to snapshot-reconcile for the gap. ADR-0025.\n\
         # TYPE cairn_oplog_dropped_total counter\n\
         cairn_oplog_dropped_total {oplog_dropped}\n\
         # HELP cairn_oplog_flush_failed_total Op-log batch flushes that failed (PG error / connection lost). Batch lost; resume falls back to snapshot-reconcile. ADR-0025.\n\
         # TYPE cairn_oplog_flush_failed_total counter\n\
         cairn_oplog_flush_failed_total {oplog_flush_failed}\n\
         # HELP cairn_slot_epoch Monotonic epoch bumped on every replication-slot (re)creation. A client whose last-seen epoch differs must full-snapshot (cannot backfill from a recreated slot's dead lineage). ADR-0025.\n\
         # TYPE cairn_slot_epoch gauge\n\
         cairn_slot_epoch {slot_epoch}\n\
         # HELP cairn_oplog_compacted_rows_total Rows swept by op-log compaction (collapse duplicates to latest op per (table_name, pk) + age out rows past the retention window). ADR-0025 slice 5.\n\
         # TYPE cairn_oplog_compacted_rows_total counter\n\
         cairn_oplog_compacted_rows_total {oplog_compacted_rows}\n",
        matched = snap.matched,
        delivered = snap.delivered,
        dropped = snap.dropped,
        faulted = snap.faulted,
        sessions = sessions,
        slot_wal_status = snap.slot_wal_status.as_gauge_int(),
        replication_lag_bytes = snap.replication_lag_bytes,
        slot_recreated_total = snap.slot_recreated_total,
        oplog_dropped = snap.oplog_dropped,
        oplog_flush_failed = snap.oplog_flush_failed,
        slot_epoch = snap.slot_epoch,
        oplog_compacted_rows = snap.oplog_compacted_rows,
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

#[cfg(test)]
mod all_mode_warning_tests {
    use super::{format_all_mode_warning, TableStat};

    #[test]
    fn warning_lists_tables_and_total() {
        let stats = vec![
            TableStat {
                table: "tasks".to_string(),
                estimated_rows: Some(12_400),
            },
            TableStat {
                table: "notes".to_string(),
                estimated_rows: Some(340),
            },
        ];

        let out = format_all_mode_warning(&stats);

        assert!(out.starts_with("WARNING: sync_mode = \"all\""));
        assert!(out.contains("tasks"));
        assert!(out.contains("~12,400 rows"));
        assert!(out.contains("notes"));
        assert!(out.contains("~340 rows"));
        // 12,400 + 340 = 12,740.
        assert!(out.contains("2 tables, ~12,740 rows estimated."));
        assert!(out.contains("nostos rules init"));
    }

    #[test]
    fn unknown_estimate_renders_unknown() {
        let stats = vec![
            TableStat {
                table: "tasks".to_string(),
                estimated_rows: Some(100),
            },
            TableStat {
                table: "audit".to_string(),
                estimated_rows: None,
            },
        ];

        let out = format_all_mode_warning(&stats);

        assert!(out.contains("audit    unknown rows (never analyzed)"));
        // audit's unknown estimate is excluded from the total — only tasks's
        // 100 rows are counted, even though both tables are counted.
        assert!(out.contains("2 tables, ~100 rows estimated."));
        // No minus sign immediately followed by a digit anywhere (e.g. the
        // reltuples = -1 "never analyzed" sentinel must never leak through).
        // The banner's own prose ("zero-config") legitimately contains a
        // hyphen, so this checks for "-<digit>", not for any hyphen.
        assert!(
            !out.chars()
                .zip(out.chars().skip(1))
                .any(|(a, b)| a == '-' && b.is_ascii_digit()),
            "no negative number may appear: {out}"
        );
    }

    #[test]
    fn empty_stats_still_warns() {
        let out = format_all_mode_warning(&[]);

        assert!(out.starts_with("WARNING: sync_mode = \"all\""));
        assert!(out.contains("0 tables, ~0 rows estimated."));
    }
}

/// `watch_rules`'s own error-handling and swap/notify ordering, exercised
/// directly (no live socket, no `nostos-infra` test server — that boundary is
/// covered by `crates/nostos-infra/tests/rules_reload.rs` instead). A short
/// `poll_interval` plus one bounded sleep gives every case at least one poll
/// tick before shutdown is signaled; `watch_rules` re-`select!`s the
/// shutdown channel every loop iteration, so it stops promptly once signaled.
#[cfg(test)]
mod watch_rules_tests {
    use super::watch_rules;
    use nostos_application::ActiveRuleset;
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{watch, RwLock};

    fn toggles_rules(tables: Vec<TableRule>) -> SyncRules {
        SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables,
            hand: Vec::new(),
        }
    }

    /// A fresh path under `std::env::temp_dir()`, unique for this test
    /// binary's lifetime. `nostos-server` has no `uuid` dependency (only
    /// `nostos-infra`'s own tests use it) — pid + a monotonic counter is
    /// enough to avoid collisions here.
    fn temp_rules_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nostos-watch-rules-test-{tag}-{}-{n}.toml",
            std::process::id()
        ))
    }

    /// Spawn `watch_rules` against `path`, let it run for a handful of
    /// 5ms poll ticks, then signal shutdown and join.
    async fn run_a_few_ticks(
        path: std::path::PathBuf,
        rules: Arc<RwLock<ActiveRuleset>>,
        rules_tx: watch::Sender<u64>,
    ) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(watch_rules(
            path,
            rules,
            Duration::from_millis(5),
            shutdown_rx,
            rules_tx,
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = shutdown_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn checksum_unchanged_reload_does_not_notify() {
        let rules = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let path = temp_rules_path("unchanged");
        nostos_infra::rules_file::save(&path, &rules).unwrap();

        let compiled = ActiveRuleset::compile(&rules).unwrap();
        let checksum = compiled.checksum();
        let shared = Arc::new(RwLock::new(compiled));
        let (tx, rx) = watch::channel(checksum); // seeded to match what's on disk
                                                 // Keep a sender clone alive in the test: `watch_rules` is handed its
                                                 // own clone and drops it when it returns, and a `Receiver` can't
                                                 // distinguish "sender dropped, no notify" from "sender dropped after
                                                 // notifying" once the channel is fully closed — `has_changed()`
                                                 // reports `Err` either way. Holding one clone open past that point
                                                 // keeps the channel open so `has_changed()` reflects the real state.
        let _tx_keepalive = tx.clone();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            !rx.has_changed().unwrap_or(false),
            "an on-disk file whose compiled checksum matches what's already loaded must never notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            checksum,
            "a checksum-unchanged reload must never touch the shared ruleset"
        );
    }

    #[tokio::test]
    async fn malformed_file_leaves_shared_state_untouched_and_does_not_notify() {
        let rules = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let compiled = ActiveRuleset::compile(&rules).unwrap();
        let checksum = compiled.checksum();
        let shared = Arc::new(RwLock::new(compiled));
        let (tx, rx) = watch::channel(checksum);
        let _tx_keepalive = tx.clone(); // see comment in checksum_unchanged_reload_does_not_notify

        let path = temp_rules_path("malformed");
        std::fs::write(&path, "this is not valid toml [[[").unwrap();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            !rx.has_changed().unwrap_or(false),
            "a malformed rules file must never notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            checksum,
            "a malformed rules file must never touch the shared ruleset"
        );
    }

    #[tokio::test]
    async fn real_change_swaps_then_notifies() {
        let before = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let before_compiled = ActiveRuleset::compile(&before).unwrap();
        let before_checksum = before_compiled.checksum();
        let shared = Arc::new(RwLock::new(before_compiled));
        let (tx, rx) = watch::channel(before_checksum);
        let _tx_keepalive = tx.clone(); // see comment in checksum_unchanged_reload_does_not_notify

        let after = toggles_rules(vec![
            TableRule {
                table: "tasks".into(),
                sync: true,
                scope: None,
            },
            TableRule {
                table: "notes".into(),
                sync: true,
                scope: None,
            },
        ]);
        let after_compiled = ActiveRuleset::compile(&after).unwrap();
        let after_checksum = after_compiled.checksum();
        assert_ne!(
            before_checksum, after_checksum,
            "test fixture bug: the two rulesets must actually differ"
        );

        let path = temp_rules_path("real-change");
        nostos_infra::rules_file::save(&path, &after).unwrap();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            rx.has_changed().unwrap_or(false),
            "a real checksum change must notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            after_checksum,
            "a real reload must swap the shared ruleset to the newly compiled rules"
        );
    }
}

#[cfg(test)]
mod rules_handler_tests {
    use super::rules_handler;
    use axum::extract::State;
    use axum::response::Json;
    use nostos_application::{ActiveRuleset, SessionManager};
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use nostos_infra::store::InMemorySessionStore;
    use nostos_infra::transport::SyncRouterState;
    use nostos_infra::AllowAnonymous;
    use std::sync::Arc;

    fn state_with(ruleset: ActiveRuleset) -> SyncRouterState {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn nostos_application::ports::SyncAuth> = Arc::new(AllowAnonymous::new());
        let (_tx, rx) = tokio::sync::watch::channel(ruleset.checksum());
        SyncRouterState::new(manager, auth)
            .with_rules(Arc::new(tokio::sync::RwLock::new(ruleset)), rx)
    }

    #[tokio::test]
    async fn rules_handler_reports_mode_and_tables() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![
                TableRule {
                    table: "tasks".to_string(),
                    sync: true,
                    scope: Some("owner_id = claims.sub".to_string()),
                },
                TableRule {
                    table: "projects".to_string(),
                    sync: true,
                    scope: Some("org_id = claims.org_id".to_string()),
                },
            ],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let checksum = ruleset.checksum();
        let state = state_with(ruleset);

        let Json(body) = rules_handler(State(state)).await;

        assert_eq!(body["sync_mode"], "toggles");
        assert_eq!(body["checksum"], format!("0x{checksum:x}"));
        let tables = body["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        assert!(tables.contains(&serde_json::json!({
            "table": "tasks",
            "scope": "owner_id = claims.sub",
        })));
        assert!(tables.contains(&serde_json::json!({
            "table": "projects",
            "scope": "org_id = claims.org_id",
        })));
    }

    #[tokio::test]
    async fn rules_handler_under_all_mode_lists_no_tables() {
        let state = state_with(ActiveRuleset::all_mode());

        let Json(body) = rules_handler(State(state)).await;

        assert_eq!(body["sync_mode"], "all");
        assert_eq!(body["tables"], serde_json::json!([]));
    }
}
