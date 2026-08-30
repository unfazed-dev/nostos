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

mod admin_auth;
mod ingest;
mod push_api;

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

    /// ADR-0040: emit `resync_required` to a client whose stream shed
    /// events (buffer full), so it clears local state and reconciles.
    #[arg(long, env = "NOSTOS_RESYNC_SIGNAL", default_value_t = false)]
    resync_signal: bool,

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

    /// Comma-separated `table:col` pairs naming PN-Counter CRDT columns
    /// (ADR-0030 addendum). Writes to these tables merge per-replica elementwise
    /// max server-side (state-based CRDT). Empty (default) = no counter columns.
    /// Example: `counts:value`. Only meaningful under `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_COUNTER_COLUMNS", default_value = "")]
    counter_columns: String,

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

    /// Per-table push configuration (ADR-0037 §1 amendment + §2, plan 2.4).
    /// `;`-separated entries; each entry is one of
    ///
    /// - `table` — silent doorbell (content-free wake),
    /// - `table:silent` — the same, explicit,
    /// - `table:visible:<title>:<body>` — a visible notification; `{col}`
    ///   in title/body statically interpolates the triggering row's column
    ///   value (no expression language). A missing column interpolates the
    ///   empty string.
    /// - `table:liveactivity:<json>` — EXPERIMENTAL (plan 6.4): the JSON
    ///   object is the ActivityKit `content-state`; `{col}` in its string
    ///   leaves interpolates the same way, and updates ride APNs
    ///   priority 5 to tokens registered with platform
    ///   `apns-liveactivity`.
    ///
    /// Colons cannot appear inside title/body and semicolons cannot appear
    /// anywhere in an entry (they separate entries — including inside a
    /// liveactivity JSON template). Tables listed here doorbell the tenant's
    /// fully-offline accounts; every other table only doorbells via matched
    /// sessions. Example:
    /// `tasks;orders:visible:New order:Order {id} placed;deliveries:liveactivity:{"status":"{status}"}`.
    /// Empty (default) = push off beyond the matched-account path. Table
    /// names must match `^[a-z_][a-z0-9_]*$` (ADR-0013 identifier discipline).
    #[arg(long, env = "NOSTOS_PUSH_TABLES", default_value = "")]
    push_tables: String,

    /// Push coalescer debounce window in milliseconds (ADR-0037 §4): bursts
    /// of hints to one account collapse to ONE push per window. Default 2s.
    #[arg(long, env = "NOSTOS_PUSH_DEBOUNCE_MS", default_value_t = 2000)]
    push_debounce_ms: u64,

    /// Remote nostos-pushd origin for push DELEGATION (ADR-0038 §3, plan
    /// task 2.3), e.g. `http://127.0.0.1:8090`. Both this AND
    /// `NOSTOS_PUSH_REMOTE_KEY` must be set (exactly one of the two is a
    /// config error). When set, `RemoteNotifier` replaces the embedded
    /// `PushRouter`: presence/token resolution/templates stay here, the
    /// daemon is the coalescing rail + receipt log.
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_URL", default_value = "")]
    push_remote_url: String,

    /// Bearer API key for the remote daemon (a tenant key from its
    /// `NOSTOS_PUSHD_API_KEYS`). Sent as `Authorization: Bearer <key>` on
    /// every delegated send and receipts poll.
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_KEY", default_value = "")]
    push_remote_key: String,

    /// Optional state-file path for the delegation receipts cursor
    /// (ADR-0038 §3 restart-resume). When set (and delegation is active),
    /// the receipts poll persists its `since` cursor across nostos-server
    /// restarts: loaded at startup (missing file = fresh start at 0),
    /// written back atomically (tmp+rename) at most once per second.
    /// Unset = in-memory cursor: a restart replays the daemon's receipt
    /// log (metrics-only skew — delivery state is monotonicity-guarded).
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_STATE_PATH", default_value = "")]
    push_remote_state_path: String,

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

    /// Sync transport (ADR-0041): "ws" (default — serve /sync over HTTP/WS on
    /// NOSTOS_BIND) or "iroh" (serve sync sessions natively over an iroh
    /// endpoint, printing the QR-native iroh:// dial URL; the HTTP ops
    /// surface still binds NOSTOS_BIND). Requires a build
    /// with `--features iroh`.
    #[arg(long, env = "NOSTOS_TRANSPORT", default_value = "ws")]
    transport: String,

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

    // ---- admin auth (Task 21, ADR-0031 addendum): NOSTOS_ADMIN_TOKEN gates
    // PUT /rules. Env-only by design (NOT a clap flag), same reasoning as
    // NOSTOS_LICENSE_SECRET below: it must never land on argv / `ps`. Unset
    // -> the route 404s (fail-closed, see admin_auth.rs). Set-but-short ->
    // refuse to start rather than serve a guessable admin route; the
    // message reports the length only, never the token itself.
    if let Some(token) = admin_auth::admin_token_from_env() {
        let len = token.expose().len();
        if len < admin_auth::MIN_ADMIN_TOKEN_LEN {
            anyhow::bail!(
                "NOSTOS_ADMIN_TOKEN is set but only {len} chars (minimum {}) — refusing to \
                 start rather than serve a guessable admin route on PUT /rules",
                admin_auth::MIN_ADMIN_TOKEN_LEN
            );
        }
        info!("admin auth: NOSTOS_ADMIN_TOKEN set — PUT /rules enabled");
    } else {
        info!("admin auth: NOSTOS_ADMIN_TOKEN unset — PUT /rules disabled (404)");
    }

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

    // ---- tenant column (used by the push wiring below AND the WS transport
    // state further down). Tenant column is enforced only under supabase-jwt
    // auth — the anonymous mode has no principal to scope with (see
    // ADR-0011). An *empty* `NOSTOS_TENANT_COLUMN=` is the explicit opt-out
    // (single-tenant deploys scoping per-table via nostos_rules.toml
    // instead): before this guard, the empty string was passed through as a
    // real column name, injecting `"" = <tenant>` into every predicate — a
    // column no row has, so every authenticated subscription silently
    // snapshot/streamed zero rows.
    // Fail-closed config gate (audit 2026-08-17 M1): `tenant_column == "id"`
    // INVERTS the write-path upsert guard into a tautology — the ON CONFLICT
    // guard becomes `WHERE "id" = EXCLUDED."id"`, true by definition on
    // any conflict, so a cross-tenant upsert silently overwrites the victim's
    // row (ADR-0018 bypassed; patch/delete still fail closed, which is what
    // made the old comment's fail-closed story look right). The v1 PK
    // convention is the column named "id", so that name can never be a
    // tenant column: bail loudly at boot instead of shipping the inversion.
    if cfg.sync_auth == "supabase-jwt" && cfg.tenant_column == "id" {
        anyhow::bail!(
            "NOSTOS_TENANT_COLUMN=id is invalid: \"id\" is the primary-key column \
             (v1 convention), and using it as the tenant column silently \
             disables cross-tenant upsert protection — pick a real tenant \
             column (e.g. org_id) or set NOSTOS_TENANT_COLUMN empty to opt out"
        );
    }
    let tenant_col = if cfg.sync_auth == "supabase-jwt" && !cfg.tenant_column.is_empty() {
        Some(cfg.tenant_column.as_str())
    } else {
        None
    };
    if cfg.sync_auth == "supabase-jwt" && cfg.tenant_column.is_empty() {
        tracing::info!(
            "NOSTOS_TENANT_COLUMN is empty — tenant scoping disabled; \
             use nostos_rules.toml scopes for per-table row filtering"
        );
    }

    // ---- ADR-0037 push doorbell (plan 1.3 + 2.4) ----
    // Rails from env (`from_env` per rail: `Ok(None)` = unconfigured); the
    // per-table config from NOSTOS_PUSH_TABLES; the token registry — Pg under
    // pg mode, in-memory otherwise so the REST surface still works in dev
    // builds (no persistence across restarts in fake mode).
    let push_cfg =
        parse_push_tables(&cfg.push_tables, tenant_col).context("invalid NOSTOS_PUSH_TABLES")?;
    let rails = nostos_infra::RailSet::from_env().context("push rail configuration")?;
    #[cfg(feature = "pg")]
    let push_registry: std::sync::Arc<dyn nostos_infra::PushTokenRegistry> =
        if cfg.replicator == "pg" {
            info!("push tokens: PgTokenStore (real registry)");
            Arc::new(nostos_infra::PgTokenStore::new(&cfg.pg_url))
        } else {
            info!(
            "push tokens: in-memory registry (fake replicator — registrations are not persisted)"
        );
            Arc::new(nostos_infra::InMemoryTokenRegistry::new())
        };
    #[cfg(not(feature = "pg"))]
    let push_registry: std::sync::Arc<dyn nostos_infra::PushTokenRegistry> =
        Arc::new(nostos_infra::InMemoryTokenRegistry::new());
    // The notifier — PRECEDENCE (ADR-0038 §3, plan task 2.3); the decision
    // itself is [`push_wiring`] below so the precedence is unit-tested.
    let wiring = push_wiring(
        &cfg.push_remote_url,
        &cfg.push_remote_key,
        !(rails.is_empty() && push_cfg.tables.tables.is_empty()),
    )
    .map_err(anyhow::Error::msg)
    .context("invalid push delegation config")?;
    let push_notifier: Arc<dyn nostos_application::ports::PushNotifier> = if wiring
        == PushWiring::Remote
    {
        info!(
            url = %cfg.push_remote_url,
            tables = push_cfg.tables.tables.len(),
            live_activity_tables = push_cfg.live_activities.len(),
            "push: RemoteNotifier delegation to nostos-pushd active (embedded PushRouter skipped)"
        );
        let receipts_state = (!cfg.push_remote_state_path.trim().is_empty())
            .then(|| std::path::PathBuf::from(cfg.push_remote_state_path.clone()));
        Arc::new(nostos_infra::push::remote::RemoteNotifier::new(
            &cfg.push_remote_url,
            &cfg.push_remote_key,
            Arc::clone(&push_registry),
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_cfg.tables.clone(),
                live_activities: push_cfg.live_activities.clone(),
            },
            Arc::clone(&metrics),
            receipts_state,
        ))
    } else if wiring == PushWiring::Noop {
        info!("push: off (no rails configured, no NOSTOS_PUSH_TABLES)");
        Arc::new(nostos_application::ports::NoopNotifier)
    } else {
        if rails.is_empty() {
            warn!(
                "NOSTOS_PUSH_TABLES is set but no push rail is configured — \
                     hints enqueue but no provider can deliver"
            );
        }
        info!(
            tables = push_cfg.tables.tables.len(),
            live_activity_tables = push_cfg.live_activities.len(),
            debounce_ms = cfg.push_debounce_ms,
            "push: PushRouter coalescer active"
        );
        Arc::new(nostos_infra::PushRouter::new(
            Arc::new(rails),
            Arc::clone(&push_registry),
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_cfg.tables.clone(),
                live_activities: push_cfg.live_activities,
            },
            std::time::Duration::from_millis(cfg.push_debounce_ms),
            Arc::clone(&metrics),
        ))
    };

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
        builder
            .with_ack_progress_every(cfg.ack_progress_interval)
            .with_push_tables(push_cfg.tables)
            .with_push_notifier(push_notifier)
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
    // Driver-liveness flag (M6): flipped when the replicator→fan-out driver
    // task exits on its own; folded into /healthz. Wired into state below.
    let driver_dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // B2 mirror (ADR-0042): the ingest half of the channel replicator, built
    // in the `mirror` arm below and handed to the /ingest route mount. The
    // handle implements SnapshotSource, so the snapshotter injection reuses
    // the same object — one buffer, two views.
    let mut mirror_ingest: Option<ingest::IngestState> = None;
    match cfg.replicator.as_str() {
        "fake" => {
            let mut repl = FakeReplicator::new(
                FakeReplicatorConfig::small(u64::MAX)
                    .paced(cfg.fake_events_per_sec)
                    .recycling_keys(cfg.fake_distinct_keys),
            );
            let fanout_drv = Arc::clone(&fanout);
            let dead = Arc::clone(&driver_dead);
            let drv = tokio::spawn(async move {
                let extract = |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> {
                    Some(ColumnValue::Any)
                };
                let outcome = fanout_drv.run(&mut repl, extract).await;
                tracing::error!(
                    ?outcome,
                    "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                );
                dead.store(true, std::sync::atomic::Ordering::Relaxed);
                info!(?outcome, "replicator stream ended");
            });
            std::mem::forget(drv);
            info!(
                events_per_sec = cfg.fake_events_per_sec,
                distinct_keys = cfg.fake_distinct_keys,
                "replicator: FakeReplicator (synthetic; 0 = unbounded)"
            );
        }
        "mirror" => {
            // B2 (ADR-0042): the desktop-sidecar mirror — the engine is the
            // single writer and POSTs row events to /ingest; this arm drains
            // the channel into the SAME fan-out pipeline (the adapter-swap
            // payoff main.rs:8-11 advertises). No PG anywhere: the snapshot
            // comes from the handle's in-memory buffer.
            if !cfg.pg_url.trim().is_empty() {
                anyhow::bail!(
                    "NOSTOS_REPLICATOR=mirror but NOSTOS_PG_URL is set — the mirror \
                     keeps its own in-memory read model and must not point at \
                     Postgres. Unset NOSTOS_PG_URL."
                );
            }
            let (handle, mut repl) = nostos_infra::MirrorHandle::open();
            mirror_ingest = Some(ingest::IngestState { handle });
            let fanout_drv = Arc::clone(&fanout);
            let dead = Arc::clone(&driver_dead);
            let drv = tokio::spawn(async move {
                // Mirror payloads are the engine's tuple-image JSON — same
                // typed extraction as the pg path (ADR-0037 plan 1.4).
                let extract =
                    |e: &ReplicationEvent, col: &str| extract_typed_column(e.payload_bytes(), col);
                let outcome = fanout_drv.run(&mut repl, extract).await;
                tracing::error!(
                    ?outcome,
                    "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                );
                dead.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            std::mem::forget(drv);
            info!("replicator: MirrorHandle (engine mirror-out via POST /ingest)");
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
                let dead = Arc::clone(&driver_dead);
                let drv = tokio::spawn(async move {
                    // Extract a column from the JSON payload: parse the small
                    // object and return the named field. Typed (ADR-0037 plan
                    // 1.4): JSON scalars keep their type — a bare `5` yields
                    // `Number(5)`, not "absent" — so predicates over numeric/
                    // bool columns no longer match wider than intended.
                    let extract = |e: &ReplicationEvent, col: &str| {
                        extract_typed_column(e.payload_bytes(), col)
                    };
                    let outcome = fanout_drv.run(&mut repl, extract).await;
                    tracing::error!(
                        ?outcome,
                        "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                    );
                    dead.store(true, std::sync::atomic::Ordering::Relaxed);
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
            anyhow::bail!(
                "unknown NOSTOS_REPLICATOR value: {other} (expected 'fake', 'pg', or 'mirror')"
            );
        }
    }

    // ---- build the axum router + transport ----
    let mut state_builder = SyncRouterState::new(Arc::clone(&manager), Arc::clone(&auth))
        .with_buffer(cfg.session_buffer)
        .with_resync_signal(cfg.resync_signal)
        .with_metrics(Arc::clone(&metrics));
    if let Some(col) = tenant_col {
        state_builder = state_builder.with_tenant_column(col);
    }

    // ---- sync-rules ruleset (ADR-0031) ----
    // A malformed/invalid file must not silently degrade to "sync everything"
    // — bail loudly instead of falling back.
    let ruleset = match nostos_infra::rules_file::load(std::path::Path::new(&cfg.rules_file)) {
        Ok(Some(raw)) => {
            // P5: name the loaded streams at boot — the boot-time template
            // validation (design §2) is only observable if the operator can
            // SEE which stream definitions passed it.
            let stream_names: Vec<&str> = raw.streams.iter().map(|s| s.name.as_str()).collect();
            let stream_count = raw.streams.len();
            let compiled = nostos_application::ActiveRuleset::compile(&raw)
                .context("nostos_rules.toml failed to compile")?;
            info!(
                sync_mode = compiled.mode().as_str(),
                tables = compiled.synced_tables().len(),
                streams = stream_count,
                stream_names = ?stream_names,
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
    // Captured before the ruleset moves into `rules_shared`: the boot-time
    // tenant-column audit (below) needs the synced-table list once.
    let rules_tables: Vec<String> = ruleset
        .synced_tables()
        .into_iter()
        .map(str::to_string)
        .collect();
    let (rules_tx, rules_changed) = tokio::sync::watch::channel(ruleset.checksum());
    let rules_shared = Arc::new(tokio::sync::RwLock::new(ruleset));
    state_builder = state_builder
        .with_rules(Arc::clone(&rules_shared), rules_changed, rules_tx.clone())
        .with_rules_file_path(std::path::PathBuf::from(&cfg.rules_file));

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
    let counter_columns = nostos_infra::parse_counter_columns(&cfg.counter_columns);
    #[cfg(feature = "pg")]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = if cfg.replicator == "pg" {
        if cfg.pg_url.trim().is_empty() {
            anyhow::bail!(
                "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back). \
                 Set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d"
            );
        }
        info!(tables = ?write_tables, or_set_columns = ?or_set_columns, counter_columns = ?counter_columns, "write-back: PgWriteBack (real source)");
        Arc::new(
            nostos_infra::PgWriteBack::new(&cfg.pg_url, write_tables.clone())
                .with_or_set_columns(or_set_columns.clone())
                .with_counter_columns(counter_columns.clone()),
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

    // B2 mirror (ADR-0042): the handle's in-memory buffer IS the snapshot
    // source — a freshly-subscribing client sees pre-ingest rows (and the
    // shared LSN allocator keeps snapshot bands unique against live events).
    if let Some(ing) = &mirror_ingest {
        let snapshotter: Arc<dyn nostos_application::ports::SnapshotSource> =
            Arc::new(ing.handle.clone());
        state_builder = state_builder.with_snapshotter(snapshotter);
        info!("snapshot-on-subscribe: MirrorHandle (in-memory mirror buffer)");
    }

    // ---- boot-time tenant-column audit (2026-08-27 incident guard) ----
    // A tenant-column deploy whose tables lack the column produced only
    // swallowed snapshot errors and starving shops — and the clap default
    // (`org_id`) made that the config you get by UNSETTING the env var.
    // One catalog scan at boot names every affected table so the operator
    // reads the diagnosis instead of probing raw WS frames for an hour.
    // All-mode has no table list to audit (`synced_tables()` is empty), so
    // the guard only fires under toggles/hand rules — where the table set
    // is known. Non-fatal by design: a deliberately-global table is a
    // LEGITIMATE shape (see `scope_if_column_present`); the audit informs,
    // it does not block boot. An audit failure itself (PG unreachable for
    // the catalog read) also only warns — the real snapshotter will surface
    // connectivity loudly on its own.
    #[cfg(feature = "pg")]
    if let Some(col) = tenant_col {
        if cfg.replicator == "pg" && !rules_tables.is_empty() {
            match nostos_infra::snapshot_source::audit_tenant_column(&cfg.pg_url, col, &rules_tables)
                .await
            {
                Ok(audit) => {
                    for table in &audit.columnless {
                        warn!(
                            table = %table, tenant_column = %col,
                            "synced table has NO tenant column: snapshots skip the \
                             tenant clause (safe), but the LIVE predicate references \
                             the column and will match NOTHING for tenant-scoped \
                             subscribers — post-seed changes to this table will not \
                             stream under a tenant deploy. If this table is meant to \
                             be global, this is fine (snapshot-first delivery); if \
                             not, add the column or fix NOSTOS_TENANT_COLUMN"
                        );
                    }
                    for table in &audit.missing {
                        warn!(
                            table = %table,
                            "ruleset syncs a table that does not exist in PG \
                             (public schema) — usually a typo in nostos_rules.toml"
                        );
                    }
                    if audit.columnless.is_empty() && audit.missing.is_empty() {
                        info!(
                            tenant_column = %col,
                            tables = rules_tables.len(),
                            "tenant-column audit: every synced table carries the column"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "tenant-column audit failed to read the catalog; continuing boot");
                }
            }
        }
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
        state_builder = state_builder.with_driver_dead(Arc::clone(&driver_dead));
        info!(publication = %cfg.pg_publication, "schema endpoint: PgSchemaSource");
    }
    let state = state_builder;
    // ADR-0041: cloned for the iroh accept loop (transport block below); the
    // router consumes the original via `.with_state`. Clone is Arcs — cheap.
    #[cfg(feature = "iroh")]
    let state_for_transport = state.clone();

    // CORS: explicit origins in production, permissive for local dev (the
    // empty-default case). Web clients need this to reach /sync from a browser.
    let cors = build_cors_layer(&cfg.cors_origins)?;

    let app = axum::Router::new()
        .route(&cfg.ws_path, get(sync_handler))
        .route("/healthz", get(healthz))
        // WS1: typed schema for client auto-schema. v2: add auth here (and to
        // /rules below) if a managed deploy wants to hide publication metadata.
        .route("/schema", get(schema))
        // ADR-0031: the active ruleset. GET stays on the same
        // v1-unauthenticated / v2-gated-together policy as /schema (see the
        // note above). PUT mutates it and is gated by NOSTOS_ADMIN_TOKEN
        // (Task 21): the route stays registered here on the same path (axum
        // has no per-method route table), but `put_rules_handler` returns a
        // literal 404 — before touching headers or body — whenever the
        // token is unset, so an unauthenticated caller observes exactly
        // what a genuinely unmounted route would look like.
        .route("/rules", get(rules_handler).put(put_rules_handler))
        .route(
            "/metrics",
            get({
                let m = Arc::clone(&metrics);
                let store_for_gauge = Arc::clone(&store);
                move || metrics_handler(m.clone(), store_for_gauge.clone())
            }),
        )
        .layer(cors.clone())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
        // ADR-0037 §3 (plan 3.1): push-token registration, same JWT auth as
        // /sync, own state (registry + auth + tenant column) — merged after
        // `.with_state` so the two state types stay separate. The CORS +
        // trace layers are re-applied here because `.layer` only covers
        // routes registered on the router at call time; without this, a
        // browser SDK's cross-origin POST /push-tokens would be blocked.
        .merge(
            axum::Router::new()
                .route(
                    "/push-tokens",
                    axum::routing::post(push_api::post_push_token),
                )
                .route(
                    "/push-tokens/:token",
                    axum::routing::delete(push_api::delete_push_token),
                )
                .with_state(push_api::PushApiState {
                    auth: Arc::clone(&auth),
                    registry: Arc::clone(&push_registry),
                    tenant_column: tenant_col.map(str::to_string),
                })
                .layer(cors)
                .layer(TraceLayer::new_for_http()),
        );

    // B2 mirror (ADR-0042): the engine's write door. Mounted ONLY under
    // NOSTOS_REPLICATOR=mirror — no handle, no route (the mount-time shape of
    // PUT /rules's fail-closed unset-token 404).
    let app = if let Some(ing) = mirror_ingest {
        app.merge(
            axum::Router::new()
                .route("/ingest", axum::routing::post(ingest::post_ingest))
                .with_state(ing)
                .layer(build_cors_layer(&cfg.cors_origins)?)
                .layer(TraceLayer::new_for_http()),
        )
    } else {
        app
    };

    // ---- transport selection (ADR-0041) ----
    // "ws" (default): sync sessions ride HTTP/WS on NOSTOS_BIND. "iroh": an
    // iroh endpoint serves the sync session core NATIVELY (D6 — the accept
    // loop runs the WebSocket handshake on each QUIC stream and hands it to
    // transport::run_session; no loopback hop). The printed QR-native
    // iroh:// URL is what clients dial. The HTTP surface (healthz/metrics/
    // schema/rules/push REST, plus the /sync upgrade route for any
    // direct-dialing ws client) binds NOSTOS_BIND in BOTH modes.
    #[cfg(feature = "iroh")]
    if cfg.transport == "iroh" {
        // ADR-0041 D8: env-only operator knob — read only in iroh builds
        // under NOSTOS_TRANSPORT=iroh; kept out of clap so non-iroh binaries
        // don't advertise a knob they can't use (docs/OPERATING.md §1/§9).
        // main.rs reads the env, infra stays env-free. Some(url) =
        // self-hosted relay replaces the n0 default fleet.
        let relay_url = nostos_infra::iroh_sync::parse_relay_url(
            std::env::var("NOSTOS_IROH_RELAY_URL").ok().as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(url) = &relay_url {
            info!(relay = %url, "iroh sync: NOSTOS_IROH_RELAY_URL set — self-hosted relay replaces the n0 default fleet");
        }
        let endpoint = nostos_infra::iroh_sync::bind_sync_endpoint(relay_url)
            .await
            .context("iroh sync endpoint bind failed")?;
        let url = endpoint.url(&cfg.ws_path);
        info!(
            transport = "iroh",
            http_bind = %cfg.bind,
            dial_url = %url,
            "Nostos sync server listening — clients dial the iroh:// URL"
        );
        tokio::spawn(async move {
            endpoint.serve_sessions(state_for_transport).await;
        });
    }
    #[cfg(not(feature = "iroh"))]
    if cfg.transport == "iroh" {
        anyhow::bail!(
            "NOSTOS_TRANSPORT=iroh but this server was built without the iroh feature \
             (cargo build -p nostos-server --features iroh)"
        );
    }

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("invalid bind address: {}", cfg.bind))?;
    info!(%addr, ws_path = %cfg.ws_path, transport = %cfg.transport, "Nostos HTTP surface listening");
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
            // Bound the drain (audit 2026-08-17 M8): the flush task may be
            // inside a PG connect/execute, and a partitioned PG during
            // SIGTERM must not hang graceful shutdown forever. The
            // statement_timeout (30s) already bounds the statements; 35s
            // here is the last backstop — on expiry we log and exit, and
            // the dropped tail is recovered by snapshot-reconcile.
            if tokio::time::timeout(std::time::Duration::from_secs(35), w.shutdown())
                .await
                .is_err()
            {
                tracing::warn!(
                    "op-log drain exceeded 35s during shutdown; exiting — unflushed tail \
                     rows reconcile via snapshot on reconnect"
                );
            }
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

/// Typed column extraction for the pg streaming path AND the B2 mirror path
/// (ADR-0037 plan 1.4): the payload's JSON scalars keep their type —
/// `{"priority":5}` yields [`ColumnValue::Number`] — instead of the old
/// string-only read, which made numeric/bool columns look absent and let
/// `Ne`/`Not(Eq)` predicates over them match wider than intended. Delegates
/// to the canonical `extract_json_column` mapping (ADR-0019) so streaming
/// predicates and the snapshot path can never drift. Pure JSON — never
/// pg-gated (the mirror arm runs featureless).
fn extract_typed_column(payload: &[u8], col: &str) -> Option<ColumnValue> {
    nostos_infra::replicator::extract_json_column(payload)?(col)
}

// ---- push-tables config parsing (ADR-0037 §1 amendment + §2, plan 2.4) ----

/// `^[a-z_][a-z0-9_]*$` — the ADR-0013 identifier shape, char-class edition
/// (the regex itself lives behind the write-back adapter; a config parser
/// doesn't need it).
fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('_' | 'a'..='z'))
        && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `NOSTOS_PUSH_TABLES` parse output: the application-layer [`PushTables`]
/// plus the infra-side Live Activity content-state templates (plan task 6.4,
/// experimental). A `liveactivity` entry appears in BOTH maps: the
/// `PushTables` row is a placeholder `Visible` — the only variant the
/// application layer attaches tuple bytes for (`fanout.rs:337`) — and the
/// router consults `live_activities` FIRST, so the placeholder never
/// renders. ponytail: placeholder coupling; the upgrade is a real
/// `PushTemplate::LiveActivity` variant when the application crate accepts
/// new variants again.
#[derive(Debug, Default)]
struct PushTablesConfig {
    tables: nostos_application::ports::PushTables,
    live_activities: std::collections::HashMap<String, serde_json::Value>,
}

/// Parse `NOSTOS_PUSH_TABLES` into the per-table push configuration (see the
/// `Config::push_tables` help) injected into both `FanOutService`
/// (tenant-wide hints) and `PushRouter` (template resolution). Format:
/// `;`-separated entries of `table`, `table:silent`,
/// `table:visible:<title>:<body>`, or `table:liveactivity:<json>` where
/// `<json>` is a JSON object whose string leaves may carry `{col}` static
/// interpolation placeholders (they become the ActivityKit `content-state`).
/// Invalid input is a startup error — a typo'd table silently not pushing is
/// the failure mode this refuses to allow.
fn parse_push_tables(raw: &str, tenant_column: Option<&str>) -> anyhow::Result<PushTablesConfig> {
    use nostos_application::ports::{PushTables, PushTemplate};

    let mut tables = std::collections::HashMap::new();
    let mut live_activities = std::collections::HashMap::new();
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, ':');
        let table = parts.next().unwrap_or_default().trim().to_string();
        if table.is_empty() {
            anyhow::bail!("NOSTOS_PUSH_TABLES: empty table name in entry {entry:?}");
        }
        if !is_plain_identifier(&table) {
            anyhow::bail!(
                "NOSTOS_PUSH_TABLES: table name {table:?} must match ^[a-z_][a-z0-9_]*$ (ADR-0013)"
            );
        }
        let template = match parts.next() {
            None => PushTemplate::Silent,
            Some(rest) => {
                let (mode, args) = match rest.split_once(':') {
                    Some((m, a)) => (m.trim(), Some(a)),
                    None => (rest.trim(), None),
                };
                match (mode, args) {
                    ("silent", None) => PushTemplate::Silent,
                    ("silent", Some(_)) => {
                        anyhow::bail!(
                            "NOSTOS_PUSH_TABLES: \"silent\" entries take no title/body: {entry:?}"
                        )
                    }
                    ("visible", Some(title_body)) => {
                        // Title runs to the next ':'; body keeps any further
                        // colons (the old `splitn(4)` remainder semantics).
                        match title_body.split_once(':') {
                            Some((title, body)) => PushTemplate::Visible {
                                title: title.trim().to_string(),
                                body: body.trim().to_string(),
                                category: None,
                            },
                            None => anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: \"visible\" entries need a title and a body: \
                                 table:visible:<title>:<body> (got {entry:?})"
                            ),
                        }
                    }
                    ("visible", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"visible\" entries need a title and a body: \
                         table:visible:<title>:<body> (got {entry:?})"
                    ),
                    // Action push (ADR-0037 §2): a visible notification whose
                    // banner carries the client-registered `category`'s action
                    // buttons. Category BEFORE title keeps parsing unambiguous
                    // (body stays the greedy remainder and may contain colons).
                    // The category is the contract between the operator's rule
                    // and the app's registered `UNNotificationCategory` /
                    // Android local-notification actions — enforce identifier
                    // discipline so typos fail at startup, not in production.
                    ("action", Some(cat_title_body)) => match cat_title_body.split_once(':') {
                        Some((category, title_body)) => {
                            let category = category.trim();
                            if !is_plain_identifier(category) {
                                anyhow::bail!(
                                    "NOSTOS_PUSH_TABLES: action category {category:?} must \
                                         match ^[a-z_][a-z0-9_]*$ in {entry:?}"
                                );
                            }
                            match title_body.split_once(':') {
                                Some((title, body)) => PushTemplate::Visible {
                                    title: title.trim().to_string(),
                                    body: body.trim().to_string(),
                                    category: Some(category.to_string()),
                                },
                                None => anyhow::bail!(
                                    "NOSTOS_PUSH_TABLES: \"action\" entries need a category, \
                                         a title and a body: \
                                         table:action:<category>:<title>:<body> (got {entry:?})"
                                ),
                            }
                        }
                        None => anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: \"action\" entries need a category, a title \
                                 and a body: table:action:<category>:<title>:<body> (got {entry:?})"
                            ),
                    },
                    ("action", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"action\" entries need a category, a title and a \
                         body: table:action:<category>:<title>:<body> (got {entry:?})"
                    ),
                    ("liveactivity", Some(tpl)) => {
                        let tpl = tpl.trim();
                        let value: serde_json::Value = serde_json::from_str(tpl).map_err(|e| {
                            anyhow::anyhow!(
                                "NOSTOS_PUSH_TABLES: liveactivity template for {table:?} is not \
                                 valid JSON: {e}"
                            )
                        })?;
                        if !value.is_object() {
                            anyhow::bail!(
                                "NOSTOS_PUSH_TABLES: liveactivity template for {table:?} must be \
                                 a JSON object (the ActivityKit content-state), got {tpl:?}"
                            );
                        }
                        live_activities.insert(table.clone(), value);
                        // Placeholder — see `PushTablesConfig`; the router's
                        // live_activities lookup shadows it before render.
                        PushTemplate::Visible {
                            title: String::new(),
                            body: String::new(),
                            category: None,
                        }
                    }
                    ("liveactivity", None) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: \"liveactivity\" entries need a JSON content-state \
                         template: table:liveactivity:{{\"col\":\"{{col}}\"}} (got {entry:?})"
                    ),
                    (other, _) => anyhow::bail!(
                        "NOSTOS_PUSH_TABLES: unknown mode {other:?} in {entry:?} (expected \
                         silent, visible or liveactivity)"
                    ),
                }
            }
        };
        if tables.insert(table.clone(), template).is_some() {
            anyhow::bail!("NOSTOS_PUSH_TABLES: table {table:?} listed twice");
        }
    }
    Ok(PushTablesConfig {
        tables: PushTables {
            tenant_column: tenant_column.map(str::to_string),
            tables,
        },
        live_activities,
    })
}

/// Which push notifier the composition root wires — the ADR-0038 §3
/// precedence as a pure decision (plan task 2.3):
///
/// 1. `NOSTOS_PUSH_REMOTE_URL` + `NOSTOS_PUSH_REMOTE_KEY` BOTH set ⇒
///    [`PushWiring::Remote`] (delegation; the embedded router is skipped
///    entirely — rails live daemon-side).
/// 2. Exactly one of the two set ⇒ config error (refuse to start).
/// 3. Both unset ⇒ [`PushWiring::Embedded`] when anything can deliver
///    (`embedded_armed`: a rail configured or push tables listed),
///    [`PushWiring::Noop`] otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushWiring {
    Remote,
    Embedded,
    Noop,
}

fn push_wiring(
    remote_url: &str,
    remote_key: &str,
    embedded_armed: bool,
) -> Result<PushWiring, String> {
    let url = !remote_url.trim().is_empty();
    let key = !remote_key.trim().is_empty();
    match (url, key) {
        (true, true) => Ok(PushWiring::Remote),
        (false, false) => Ok(if embedded_armed {
            PushWiring::Embedded
        } else {
            PushWiring::Noop
        }),
        _ => Err("push delegation requires BOTH NOSTOS_PUSH_REMOTE_URL and \
             NOSTOS_PUSH_REMOTE_KEY (exactly one is set)"
            .to_string()),
    }
}

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
fn build_cors_layer(cors_origins: &str) -> anyhow::Result<tower_http::cors::CorsLayer> {
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
/// replicator-driver liveness, both cheap to read (O(1) atomic). A load
/// balancer polls this to decide whether to route traffic. When the
/// replicator→fan-out driver has EXITED (stream end — audit 2026-08-17 M6)
/// the server is a zombie: it accepts `/sync` and serves snapshots but
/// delivers no live events. The endpoint then answers 503 `"degraded"`
/// so the LB drains it. (A driver PANIC is not folded in — the tokio panic
/// hook already screams on stderr; the exit path is the silent one.)
async fn healthz(State(state): State<SyncRouterState>) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.manager.session_count().await;
    let driver_dead = state
        .driver_dead
        .as_ref()
        .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed));
    let (code, status, driver) = if driver_dead {
        (StatusCode::SERVICE_UNAVAILABLE, "degraded", "dead")
    } else {
        (StatusCode::OK, "ok", "live")
    };
    (
        code,
        Json(serde_json::json!({
            "status": status,
            "sessions": sessions,
            "replicator_driver": driver,
        })),
    )
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
    Json(rules_body(&ruleset, &state.metrics))
}

/// Shared response shape for `GET /rules` and a successful `PUT /rules`
/// (Task 20) — the client re-renders from the PUT response without a second
/// fetch, so both routes must produce byte-identical JSON for the same
/// `ActiveRuleset`.
fn rules_body(ruleset: &nostos_application::ActiveRuleset, metrics: &Metrics) -> serde_json::Value {
    let slot_epoch = metrics
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
    serde_json::json!({
        "sync_mode": ruleset.mode().as_str(),
        "checksum": format!("0x{checksum:x}"),
        "sync_epoch": format!("0x{sync_epoch:x}"),
        "tables": tables,
    })
}

/// `PUT /rules` request body — the toggle model, not raw TOML: the server
/// owns serialization so the file shape stays canonical (Task 20).
#[derive(serde::Deserialize)]
struct PutRulesRequest {
    /// `"all" | "toggles" | "hand"` — but `"hand"` is REJECTED here (422).
    /// Hand-written `[[rules]]` are the CLI's surface; the toggle editor must
    /// never rewrite a file it cannot faithfully round-trip.
    sync_mode: String,
    tables: Vec<PutRulesTable>,
}

#[derive(serde::Deserialize)]
struct PutRulesTable {
    table: String,
    sync: bool,
    scope: Option<String>,
}

fn rules_422(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": message.into() })),
    )
}

/// `PUT /rules` — the authenticated entry point (Task 21). Ordering matters
/// and is deliberate: the admin-token check runs before anything else,
/// including Content-Type and JSON parsing, using `HeaderMap` + raw `Bytes`
/// instead of axum's `Json<T>` extractor — `Json<T>` runs before a handler
/// body even starts, so an unauthenticated caller sending a malformed body
/// would get a 400/415 from the extractor and learn the route exists before
/// ever reaching the 404. See `admin_auth.rs` for the gate itself and
/// `apply_put_rules` below for the actual mutation (unchanged from Task 20).
async fn put_rules_handler(
    State(state): State<SyncRouterState>,
    headers: axum::http::HeaderMap,
    raw_body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 1. Token unset -> the route is not mounted. Checked before anything
    // else touches `headers` or `raw_body`.
    let Some(admin_token) = admin_auth::admin_token_from_env() else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        ));
    };

    // 2. Bearer token mismatch -> 401. Constant-time compare, no logging.
    if !admin_auth::AdminAuth::check(&headers, &admin_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        ));
    }

    // 3. CSRF stance (ADR-0031 addendum, Task 21 §2): bearer-header auth has
    // no ambient credential for a cross-site form to ride on, so the one
    // enforceable defence left is rejecting any body that isn't actually
    // JSON — it blocks the simple-form vector outright.
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({ "error": "Content-Type must be application/json" })),
        ));
    }

    let body: PutRulesRequest = serde_json::from_slice(&raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid JSON body: {e}") })),
        )
    })?;

    // Audit "before" snapshot, taken ahead of the mutation. The old-tables
    // read is a second, non-authoritative load purely for the
    // `tables_changed` count below — `apply_put_rules` does its own
    // authoritative load and is the only writer.
    let mode_before = state.rules.read().await.mode().as_str().to_string();
    let checksum_before = state.rules.read().await.checksum();
    let old_tables: Vec<(String, bool, Option<String>)> =
        nostos_infra::rules_file::load(&state.rules_file_path)
            .ok()
            .flatten()
            .map(|r| {
                r.tables
                    .into_iter()
                    .map(|t| (t.table, t.sync, t.scope))
                    .collect()
            })
            .unwrap_or_default();
    let new_tables: Vec<(String, bool, Option<String>)> = body
        .tables
        .iter()
        .map(|t| (t.table.clone(), t.sync, t.scope.clone()))
        .collect();

    let response = apply_put_rules(&state, body).await?;

    // Audit line — success path only (a `?` above returns before this runs
    // on any error). `actor` is a non-secret fingerprint, never the token;
    // `source` is always "api" for now: there is no header distinguishing
    // the web panel from a direct API caller, so both are indistinguishable
    // here (ponytail: add `X-Cairn-Source` if the panel needs separating).
    let mode_after = state.rules.read().await.mode().as_str().to_string();
    let checksum_after = state.rules.read().await.checksum();
    let tables_changed = count_changed_tables(&old_tables, &new_tables);
    let actor = admin_auth::actor_id(&admin_token);
    tracing::info!(
        target: "nostos::audit",
        actor = %actor,
        source = "api",
        mode_before = %mode_before,
        mode_after = %mode_after,
        checksum_before = %format!("0x{checksum_before:x}"),
        checksum_after = %format!("0x{checksum_after:x}"),
        tables_changed = %tables_changed,
        "rules_mutation"
    );

    Ok(response)
}

/// Tables whose (sync, scope) differ between `before` and `after`, plus any
/// table added or removed — a union, not two separate counts, so a table
/// that appears on both sides with a changed scope is counted once.
fn count_changed_tables(
    before: &[(String, bool, Option<String>)],
    after: &[(String, bool, Option<String>)],
) -> usize {
    use std::collections::{BTreeMap, BTreeSet};
    let before_map: BTreeMap<&str, (bool, Option<&str>)> = before
        .iter()
        .map(|(t, s, sc)| (t.as_str(), (*s, sc.as_deref())))
        .collect();
    let after_map: BTreeMap<&str, (bool, Option<&str>)> = after
        .iter()
        .map(|(t, s, sc)| (t.as_str(), (*s, sc.as_deref())))
        .collect();
    let mut changed = BTreeSet::new();
    for (name, v) in &before_map {
        if after_map.get(name) != Some(v) {
            changed.insert(*name);
        }
    }
    for (name, v) in &after_map {
        if before_map.get(name) != Some(v) {
            changed.insert(*name);
        }
    }
    changed.len()
}

/// The actual mutation (Task 20, unchanged): validate → atomically write
/// `nostos_rules.toml` → only then swap the in-process `ActiveRuleset` and
/// notify live sessions. Split out from `put_rules_handler` so the Task 20
/// unit tests below can exercise it directly without going through the
/// Task 21 auth gate, which has its own dedicated tests.
///
/// ponytail: no optimistic concurrency between the CLI editor and PUT
/// /rules — last write wins. Ceiling: a concurrent edit can be silently
/// overwritten. Upgrade path: ETag from the checksum + `If-Match`.
///
/// Ordering (write-then-swap, not the reverse): validate → atomically write
/// `nostos_rules.toml` → only then swap the in-process `ActiveRuleset` and
/// notify live sessions. If the process dies between the write and the swap,
/// the file is truth and startup reloads it — a crash costs a reload, never a
/// divergence between the file and what's enforced. Swapping first could
/// enforce a ruleset that was never persisted.
async fn apply_put_rules(
    state: &SyncRouterState,
    body: PutRulesRequest,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if body.sync_mode == "hand" {
        return Err(rules_422(
            "PUT /rules cannot write sync_mode \"hand\" — hand-written [[rules]] are the \
             CLI's surface; the toggle editor must never rewrite a file it cannot faithfully \
             round-trip. Use `nostos rules edit --mode hand` instead.",
        ));
    }
    let mode = SyncMode::parse(&body.sync_mode).ok_or_else(|| {
        rules_422(nostos_domain::RulesError::UnknownMode(body.sync_mode.clone()).to_string())
    })?;

    let tables: Vec<nostos_domain::TableRule> = body
        .tables
        .into_iter()
        .map(|t| nostos_domain::TableRule {
            table: t.table,
            sync: t.sync,
            scope: t.scope,
        })
        .collect();

    // Truth-switching must never delete an artifact (rules_file.rs): the
    // toggle editor only ever owns `[tables.*]`, so any hand-authored
    // `[[rules]]` already on disk must survive this write untouched. Same
    // for `[streams.*]` (P5): the toggle editor does not own stream
    // definitions either — a PUT must never silently delete them.
    let (hand, streams) = match nostos_infra::rules_file::load(&state.rules_file_path) {
        Ok(Some(existing)) => (existing.hand, existing.streams),
        Ok(None) => (Vec::new(), Vec::new()),
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!(
                        "reading current {}: {e}",
                        state.rules_file_path.display()
                    )
                })),
            ));
        }
    };

    let rules = nostos_domain::SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode,
        tables,
        hand,
        streams,
    };

    // Step 1: validate via the same compile path `nostos rules check` uses —
    // invalid → 422 with that exact error text, nothing touched.
    let compiled =
        nostos_application::ActiveRuleset::compile(&rules).map_err(|e| rules_422(e.to_string()))?;

    // Step 2: atomically write BEFORE swapping in-process state.
    if let Err(e) = nostos_infra::rules_file::save(&state.rules_file_path, &rules) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("writing {}: {e}", state.rules_file_path.display())
            })),
        ));
    }

    // Step 3: only after the write succeeds, swap + notify (Task 14's live-
    // session invalidation runs off this same `watch::Sender` — swap before
    // send, exactly like `watch_rules`, so a woken session never reads stale
    // state). The Task 14 poller dedupes on checksum, so its next tick over
    // this same file is a no-op — no double invalidation.
    let new_checksum = compiled.checksum();
    *state.rules.write().await = compiled;
    let _ = state.rules_tx.send(new_checksum);

    // Step 4: 200 with the same body shape as GET /rules.
    let ruleset = state.rules.read().await;
    Ok(Json(rules_body(&ruleset, &state.metrics)))
}

/// `GET /metrics` — Prometheus text exposition format, hand-rolled (the
/// workspace intentionally stays dependency-free for metrics per the
/// ponytail-audit cuts). Counters are aggregate throughput from the fan-out
/// service; the sessions gauge is snapshotted from the store.
async fn metrics_handler(metrics: Arc<Metrics>, store: Arc<dyn SessionStore>) -> String {
    let snap = metrics.snapshot();
    let sessions = store.len().await;
    // Per-account last-pushed-LSN (plan 3.2 — the push-LSN→client-ack
    // correlation surface), rendered next to the session gauges so an
    // operator can see whether a doorbelled device actually caught up.
    // Label values are escaped (account ids are external data).
    let last_pushed: String = metrics
        .push_last_lsn
        .lock()
        .map(|map| {
            map.iter().fold(String::new(), |mut acc, (account, lsn)| {
                let escaped = account.replace('\\', "\\\\").replace('"', "\\\"");
                let _ = std::fmt::Write::write_fmt(
                    &mut acc,
                    format_args!("cairn_push_last_lsn{{account=\"{escaped}\"}} {lsn}\n"),
                );
                acc
            })
        })
        .unwrap_or_default();
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
         cairn_oplog_compacted_rows_total {oplog_compacted_rows}\n\
         # HELP cairn_push_enqueued_total Push doorbell hints enqueued (one per matched offline account; online accounts suppressed at enqueue time). ADR-0037.\n\
         # TYPE cairn_push_enqueued_total counter\n\
         cairn_push_enqueued_total {push_enqueued}\n\
         # HELP cairn_push_dropped_total Push hints dropped (bounded channel full / consumer gone). Doorbell semantics: a dropped hint loses nothing — the durable LSN checkpoint reconciles. ADR-0037.\n\
         # TYPE cairn_push_dropped_total counter\n\
         cairn_push_dropped_total {push_dropped}\n\
         # HELP cairn_push_sent_total Push sends the rails accepted (2xx). Last-mile delivery stays best-effort; the client's LSN ack is the proof. ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_sent_total counter\n\
         cairn_push_sent_total {push_sent}\n\
         # HELP cairn_push_failed_total Push sends that failed terminally or exhausted their retry (rail fatal/transient). ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_failed_total counter\n\
         cairn_push_failed_total {push_failed}\n\
         # HELP cairn_push_pruned_total Push-token rows pruned (rail reported the target gone, or the owner deregistered). ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_pruned_total counter\n\
         cairn_push_pruned_total {push_pruned}\n\
         # HELP cairn_push_last_lsn Highest doorbell LSN pushed per account — correlate against session acked-LSN to see whether a doorbelled device actually caught up. ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_last_lsn gauge\n\
         {last_pushed}",
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
        push_enqueued = snap.push_enqueued,
        push_dropped = snap.push_dropped,
        push_sent = snap.push_sent,
        push_failed = snap.push_failed,
        push_pruned = snap.push_pruned,
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

/// Typed extraction regression (ADR-0037 plan 1.4): the streaming extractor
/// must preserve JSON scalar types. Pinned here because the old inline
/// `as_str()`-only read made `{"priority":5}` extract as absent, so `Ne`/
/// `Not(Eq)` predicates over numeric columns matched wide and `Eq` under-
/// delivered.
#[cfg(all(test, feature = "pg"))]
mod extract_typed_column_tests {
    use super::extract_typed_column;
    use nostos_domain::{ColumnValue, Predicate};

    #[test]
    fn json_scalars_keep_their_type() {
        let payload = br#"{"org_id":"acme","priority":5,"score":2.5,"active":true}"#;
        assert_eq!(
            extract_typed_column(payload, "org_id"),
            Some(ColumnValue::text("acme"))
        );
        assert_eq!(
            extract_typed_column(payload, "priority"),
            Some(ColumnValue::number(5))
        );
        assert_eq!(
            extract_typed_column(payload, "score"),
            Some(ColumnValue::float(2.5))
        );
        assert_eq!(
            extract_typed_column(payload, "active"),
            Some(ColumnValue::boolean(true))
        );
        assert_eq!(extract_typed_column(payload, "missing"), None);
    }

    #[test]
    fn not_eq_over_numeric_column_no_longer_matches_wide() {
        // Before the fix: `priority` extracted as absent → the inner Eq was
        // false → Not(Eq) matched EVERY row, including priority=5 itself.
        let payload = br#"{"priority":5}"#;
        let not_eq = !Predicate::eq("tasks", "priority", ColumnValue::number(5));
        assert!(!not_eq.matches(|c| extract_typed_column(payload, c)));

        // And the under-delivery side: Ne now sees the value and matches.
        let ne = Predicate::ne("tasks", "priority", ColumnValue::number(7));
        assert!(ne.matches(|c| extract_typed_column(payload, c)));
        let eq = Predicate::eq("tasks", "priority", ColumnValue::number(5));
        assert!(eq.matches(|c| extract_typed_column(payload, c)));
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
            streams: Vec::new(),
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
        let (tx, rx) = tokio::sync::watch::channel(ruleset.checksum());
        SyncRouterState::new(manager, auth).with_rules(
            Arc::new(tokio::sync::RwLock::new(ruleset)),
            rx,
            tx,
        )
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
            streams: vec![],
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

#[cfg(test)]
mod put_rules_handler_tests {
    use super::{apply_put_rules, PutRulesRequest, PutRulesTable};
    use axum::http::StatusCode;
    use axum::response::Json;
    use nostos_application::{ActiveRuleset, SessionManager};
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use nostos_infra::store::InMemorySessionStore;
    use nostos_infra::transport::SyncRouterState;
    use nostos_infra::AllowAnonymous;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A fresh path under `std::env::temp_dir()`, unique for this test
    /// binary's lifetime — mirrors `watch_rules_tests::temp_rules_path`.
    fn temp_rules_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nostos-put-rules-test-{tag}-{}-{n}.toml",
            std::process::id()
        ))
    }

    fn state_with_file(ruleset: ActiveRuleset, path: std::path::PathBuf) -> SyncRouterState {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn nostos_application::ports::SyncAuth> = Arc::new(AllowAnonymous::new());
        let (tx, rx) = tokio::sync::watch::channel(ruleset.checksum());
        SyncRouterState::new(manager, auth)
            .with_rules(Arc::new(tokio::sync::RwLock::new(ruleset)), rx, tx)
            .with_rules_file_path(path)
    }

    fn req(sync_mode: &str, tables: Vec<(&str, bool, Option<&str>)>) -> PutRulesRequest {
        PutRulesRequest {
            sync_mode: sync_mode.to_string(),
            tables: tables
                .into_iter()
                .map(|(table, sync, scope)| PutRulesTable {
                    table: table.to_string(),
                    sync,
                    scope: scope.map(str::to_string),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn put_rules_writes_file_and_swaps_active() {
        let path = temp_rules_path("write-and-swap");
        let state = state_with_file(ActiveRuleset::all_mode(), path.clone());

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let result = apply_put_rules(&state, body).await;
        let Json(response) = result.expect("valid PUT must succeed");

        assert_eq!(response["sync_mode"], "toggles");

        // File on disk reflects the new ruleset.
        let on_disk = nostos_infra::rules_file::load(&path)
            .expect("load")
            .expect("file exists");
        assert_eq!(on_disk.mode, SyncMode::Toggles);
        assert_eq!(on_disk.tables.len(), 1);
        assert_eq!(on_disk.tables[0].table, "tasks");

        // In-process ruleset swapped too.
        let active = state.rules.read().await;
        assert_eq!(active.mode(), SyncMode::Toggles);
        assert_eq!(response["checksum"], format!("0x{:x}", active.checksum()));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn put_rules_rejects_hand_mode() {
        let path = temp_rules_path("reject-hand");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path.clone());

        let body = req("hand", vec![]);
        let result = apply_put_rules(&state, body).await;
        let Err((status, Json(err))) = result else {
            panic!("hand mode must be rejected");
        };

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err["error"].as_str().unwrap().contains("hand"));

        // Nothing touched: no file written, active ruleset unchanged.
        assert!(!path.exists());
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }

    #[tokio::test]
    async fn put_rules_rejects_invalid_scope() {
        let path = temp_rules_path("reject-invalid-scope");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path.clone());

        // Same fixture as the Task 17 CLI test
        // (`check_flags_invalid_scope_with_table_name`): a top-level `OR`
        // scope, which the scope compiler rejects.
        let bad_rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: Some("a = 1 OR b = 2".to_string()),
            }],
            hand: vec![],
            streams: vec![],
        };
        let expected_error = ActiveRuleset::compile(&bad_rules)
            .expect_err("fixture must be invalid")
            .to_string();

        let body = req("toggles", vec![("tasks", true, Some("a = 1 OR b = 2"))]);
        let result = apply_put_rules(&state, body).await;
        let Err((status, Json(err))) = result else {
            panic!("invalid scope must be rejected");
        };

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err["error"], expected_error);

        assert!(!path.exists());
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }

    #[tokio::test]
    async fn put_rules_does_not_double_invalidate() {
        let path = temp_rules_path("no-double-invalidate");
        let state = state_with_file(ActiveRuleset::all_mode(), path.clone());
        let mut rules_changed = state.rules_changed.clone();
        // Mark the current value seen so `changed()` only fires on a real update.
        rules_changed.borrow_and_update();

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let _ = apply_put_rules(&state, body)
            .await
            .expect("valid PUT must succeed");

        // The PUT itself notifies exactly once.
        assert!(rules_changed.has_changed().unwrap());
        rules_changed.borrow_and_update();

        // A watcher poll tick over the SAME file (now on disk with the
        // ruleset the PUT already swapped to) computes the same checksum —
        // it must dedupe, mirroring `watch_rules`'s own guard (`if
        // new_checksum == *rules_tx.borrow() { continue; }`) — no second
        // `send`, so `rules_changed` observes no further change.
        let checksum_on_disk = nostos_infra::rules_file::load(&path)
            .expect("load")
            .expect("file exists")
            .checksum();
        assert_eq!(checksum_on_disk, *state.rules_tx.borrow());
        assert!(!rules_changed.has_changed().unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn put_rules_write_failure_leaves_active_ruleset_untouched() {
        // A path under a directory that does not exist: `save` -> `write_mirror`
        // fails at `File::create` (ENOENT) before ever reaching the swap.
        let path = std::env::temp_dir()
            .join(format!(
                "nostos-put-rules-test-nonexistent-dir-{}",
                std::process::id()
            ))
            .join("nostos_rules.toml");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path);

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let result = apply_put_rules(&state, body).await;
        let Err((status, _)) = result else {
            panic!("write failure must surface as an error");
        };

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }
}

/// The ADR-0038 §3 wiring precedence (plan task 2.3) — see [`push_wiring`].
#[cfg(test)]
mod push_wiring_tests {
    use super::{push_wiring, PushWiring};

    #[test]
    fn both_remote_vars_set_wins_over_everything() {
        // Delegation beats the embedded path even with rails+tables armed:
        // rails live daemon-side when delegating.
        assert_eq!(
            push_wiring("http://127.0.0.1:8090", "tenant-secret", true),
            Ok(PushWiring::Remote)
        );
        assert_eq!(
            push_wiring("http://127.0.0.1:8090", "tenant-secret", false),
            Ok(PushWiring::Remote)
        );
    }

    #[test]
    fn exactly_one_remote_var_is_a_config_error() {
        assert!(push_wiring("http://127.0.0.1:8090", "", true).is_err());
        assert!(push_wiring("", "tenant-secret", false).is_err());
        // Blank-but-set counts as unset (same rule as the rail envs).
        assert!(push_wiring("   ", "", false).is_ok());
    }

    #[test]
    fn unset_falls_back_to_embedded_or_noop() {
        assert_eq!(push_wiring("", "", true), Ok(PushWiring::Embedded));
        assert_eq!(push_wiring("", "", false), Ok(PushWiring::Noop));
    }
}

#[cfg(test)]
mod parse_push_tables_tests {
    use super::{is_plain_identifier, parse_push_tables};
    use nostos_application::ports::PushTemplate;
    use serde_json::json;

    #[test]
    fn parses_action_mode_with_category_and_rejects_bad_categories() {
        let cfg = parse_push_tables(
            "orders:action:order_status:Atlet order update:Your order {id} is {status}:really",
            None,
        )
        .expect("valid action config");
        assert_eq!(
            cfg.tables.get("orders"),
            Some(&PushTemplate::Visible {
                title: "Atlet order update".into(),
                // Body keeps further colons (greedy remainder semantics).
                body: "Your order {id} is {status}:really".into(),
                category: Some("order_status".into()),
            })
        );

        let err = parse_push_tables("orders:action:Order Status:t:b", None);
        assert!(err.is_err(), "category must be a plain identifier");

        let err = parse_push_tables("orders:action:order_status:only-title", None);
        assert!(err.is_err(), "action entries need category, title AND body");

        let err = parse_push_tables("orders:action", None);
        assert!(err.is_err(), "bare action mode is rejected");
    }

    #[test]
    fn parses_silent_default_explicit_and_visible_with_placeholders() {
        let cfg = parse_push_tables(
            "tasks; notes:silent ; orders:visible:New order:Order {id} placed",
            Some("org_id"),
        )
        .expect("valid config");
        assert_eq!(cfg.tables.tenant_column.as_deref(), Some("org_id"));
        assert_eq!(cfg.tables.get("tasks"), Some(&PushTemplate::Silent));
        assert_eq!(cfg.tables.get("notes"), Some(&PushTemplate::Silent));
        assert_eq!(
            cfg.tables.get("orders"),
            Some(&PushTemplate::Visible {
                title: "New order".into(),
                body: "Order {id} placed".into(),
                category: None
            })
        );
        assert_eq!(cfg.tables.get("absent"), None);
        assert!(cfg.live_activities.is_empty());
    }

    #[test]
    fn empty_string_is_an_empty_config() {
        let cfg = parse_push_tables("", None).expect("empty is valid (push off)");
        assert!(cfg.tables.tables.is_empty());
        assert!(cfg.tables.tenant_column.is_none());
        assert!(cfg.live_activities.is_empty());
    }

    #[test]
    fn rejects_bad_modes_missing_body_bad_identifiers_and_duplicates() {
        for bad in [
            "tasks:loud",
            "orders:visible:OnlyTitle",
            "Orders:visible:a:b",
            "tasks;tasks",
            "tasks:silent:extra",
        ] {
            assert!(
                parse_push_tables(bad, None).is_err(),
                "{bad:?} must be rejected at startup"
            );
        }
    }

    #[test]
    fn liveactivity_entry_parses_template_and_placeholders() {
        let cfg = parse_push_tables(
            r#"deliveries:liveactivity:{"status":"{status}","eta_min":"{eta_min}","nested":{"deep":"{x}"}}"#,
            None,
        )
        .expect("valid liveactivity config");
        // The tables map carries the Visible placeholder so fan-out attaches
        // tuple bytes (see PushTablesConfig); the real template is separate.
        assert_eq!(
            cfg.tables.get("deliveries"),
            Some(&PushTemplate::Visible {
                title: String::new(),
                body: String::new(),
                category: None
            })
        );
        assert_eq!(
            cfg.live_activities.get("deliveries"),
            Some(
                &json!({ "status": "{status}", "eta_min": "{eta_min}", "nested": { "deep": "{x}" } })
            )
        );
    }

    #[test]
    fn liveactivity_entries_must_be_a_json_object() {
        for bad in [
            r"deliveries:liveactivity:not json",
            r"deliveries:liveactivity:[1,2]",
            r#"deliveries:liveactivity:"string""#,
            "deliveries:liveactivity",
        ] {
            assert!(
                parse_push_tables(bad, None).is_err(),
                "{bad:?} must be rejected at startup"
            );
        }
    }

    #[test]
    fn identifier_shape_matches_adr0013() {
        for good in ["tasks", "a", "_x", "t1_2"] {
            assert!(is_plain_identifier(good), "{good} should pass");
        }
        for bad in ["Tasks", "1t", "a-b", "", "a b"] {
            assert!(!is_plain_identifier(bad), "{bad} should fail");
        }
    }
}

/// ADR-0037 "the test that matters" — server-side slice (plan 3.3): the real
/// `FanOutService` hint enqueue → the real `PushRouter` coalescer, against a
/// recording fake rail, the in-memory token registry, and the REAL
/// `InMemorySessionStore` (presence = store membership, so a `Dropped`-but-
/// registered session counts as online). The fake replicator's payload is
/// opaque, so the extractor hands the tenant column out directly.
#[cfg(test)]
mod push_e2e_tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use nostos_application::ports::{
        DeliveryDecision, EventSink, Metrics, PushNotifier, PushTables, PushTemplate, SyncAuth,
    };
    use nostos_application::FanOutService;
    use nostos_domain::{
        ColumnValue, Lsn, Predicate, Principal, ReplicationEvent, RowOp, SyncSession,
    };
    use nostos_infra::push::{PushPayload, RailOutcome};
    use nostos_infra::{
        InMemorySessionStore, InMemoryTokenRegistry, PushRouter, PushSink, PushTokenRegistry,
    };

    use super::push_api::{self, PushApiState};

    /// The fake rail: records every send, always reports Delivered.
    struct RecordingRail {
        sends: Mutex<Vec<(String, String, PushPayload)>>, // (platform, token, payload)
        live_sends: Mutex<Vec<(String, String, serde_json::Value)>>, // (token, collapse, state)
    }

    #[async_trait]
    impl PushSink for RecordingRail {
        async fn send(
            &self,
            platform: &str,
            token: &str,
            _collapse_key: &str,
            payload: &PushPayload,
        ) -> RailOutcome {
            self.sends.lock().unwrap().push((
                platform.to_string(),
                token.to_string(),
                payload.clone(),
            ));
            RailOutcome::Delivered
        }

        async fn send_live_activity(
            &self,
            token: &str,
            _collapse_key: &str,
            content_state: &serde_json::Value,
        ) -> RailOutcome {
            self.live_sends.lock().unwrap().push((
                token.to_string(),
                _collapse_key.to_string(),
                content_state.clone(),
            ));
            RailOutcome::Delivered
        }
    }

    /// A slow-client sink: always `Dropped`, still a live session.
    struct DroppingSink;

    #[async_trait]
    impl EventSink for DroppingSink {
        async fn deliver(&self, _event: ReplicationEvent) -> DeliveryDecision {
            DeliveryDecision::Dropped
        }
    }

    /// Always resolves to one fixed principal — the authenticated test path.
    struct FixedAuth(Principal);

    #[async_trait]
    impl SyncAuth for FixedAuth {
        async fn authenticate(&self, _token: &str) -> Option<Principal> {
            Some(self.0.clone())
        }
    }

    fn push_tables() -> PushTables {
        PushTables {
            tenant_column: Some("org_id".into()),
            tables: [("tasks".to_string(), PushTemplate::Silent)]
                .into_iter()
                .collect(),
        }
    }

    fn event(lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    /// The tenant column extractor: `org_id` → t1 (the fake payload is
    /// opaque bytes, so the value is handed out directly).
    fn extract(_e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
        (col == "org_id").then(|| ColumnValue::text("t1"))
    }

    /// Build the full chain: store + registry + rail + router + fan-out.
    /// `session` optionally registers a live session for account u1 first.
    async fn harness(
        session: Option<Arc<dyn EventSink>>,
    ) -> (
        Arc<RecordingRail>,
        Arc<InMemoryTokenRegistry>,
        Arc<FanOutService>,
    ) {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        if let Some(sink) = session {
            store
                .add(
                    SyncSession::new_authenticated(
                        Predicate::all("tasks"),
                        Principal::new("u1", "t1"),
                    ),
                    sink,
                )
                .await;
        }
        let registry = Arc::new(InMemoryTokenRegistry::new());
        registry
            .upsert("apns", "dev-e2e", "u1", "t1")
            .await
            .unwrap();
        let rail = Arc::new(RecordingRail {
            sends: Mutex::new(Vec::new()),
            live_sends: Mutex::new(Vec::new()),
        });
        let registry_dyn: Arc<dyn PushTokenRegistry> = registry.clone();
        let router = PushRouter::new(
            Arc::clone(&rail) as Arc<dyn PushSink>,
            registry_dyn,
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_tables(),
                live_activities: std::collections::HashMap::new(),
            },
            Duration::from_millis(60),
            Arc::new(Metrics::new()),
        );
        let svc = Arc::new(
            FanOutService::new(Arc::clone(&store))
                .with_push_tables(push_tables())
                .with_push_notifier(Arc::new(router) as Arc<dyn PushNotifier>),
        );
        (rail, registry, svc)
    }

    async fn burst(svc: &FanOutService) {
        for lsn in 1..=100u64 {
            let _ = svc.fan_out(&event(lsn), extract).await;
        }
    }

    async fn soon(mut f: impl FnMut() -> bool) {
        for _ in 0..250 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    }

    /// Let a completed window settle — no further sends may arrive.
    async fn quiet() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// (a) 100-event burst to an OFFLINE account ⇒ exactly ONE push, carrying
    /// the latest LSN (the doorbell is a wake-up; the durable checkpoint is
    /// the correctness mechanism).
    #[tokio::test]
    async fn burst_to_offline_account_yields_exactly_one_push() {
        let (rail, _registry, svc) = harness(None).await;
        burst(&svc).await;
        soon(|| !rail.sends.lock().unwrap().is_empty()).await;
        quiet().await;
        let sends = rail.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1, "100-event burst must collapse to one push");
        assert_eq!(sends[0].1, "dev-e2e");
        assert_eq!(
            sends[0].2,
            PushPayload::Silent {
                table: "tasks".into(),
                lsn: Lsn::new(100)
            }
        );
    }

    /// (b) ONLINE account ⇒ ZERO pushes — the socket is the transport; a
    /// push would double-signal a client that is already receiving.
    #[tokio::test]
    async fn online_account_gets_no_push() {
        // A recording sink that never drops: the session is healthy.
        struct OkSink;
        #[async_trait]
        impl EventSink for OkSink {
            async fn deliver(&self, _event: ReplicationEvent) -> DeliveryDecision {
                DeliveryDecision::Delivered
            }
        }
        let (rail, _registry, svc) = harness(Some(Arc::new(OkSink))).await;
        burst(&svc).await;
        quiet().await;
        assert!(
            rail.sends.lock().unwrap().is_empty(),
            "an online account must not be doorbelled"
        );
    }

    /// (c) `Dropped`-but-online ⇒ ZERO pushes — `Dropped` is slow-client
    /// backpressure, NOT presence (ADR-0037 §4); pushing a draining socket
    /// double-signals a client that is catching up.
    #[tokio::test]
    async fn dropped_but_online_account_gets_no_push() {
        let (rail, _registry, svc) = harness(Some(Arc::new(DroppingSink))).await;
        burst(&svc).await;
        quiet().await;
        assert!(
            rail.sends.lock().unwrap().is_empty(),
            "'Dropped' is backpressure, not offline-presence"
        );
    }

    /// (d) Sign-out: the token deregistered through the REST route receives
    /// nothing afterwards — and it DID receive a push before deregistration,
    /// proving the route (not the fixture) removed it.
    #[tokio::test]
    async fn signout_deregisters_token_via_rest_route() {
        let (rail, registry, svc) = harness(None).await;

        // Phase 1: pre-sign-out, the burst doorbells the device.
        burst(&svc).await;
        soon(|| !rail.sends.lock().unwrap().is_empty()).await;
        quiet().await;
        assert_eq!(rail.sends.lock().unwrap().len(), 1);

        // Sign-out: DELETE /push-tokens/{token} through the real handler,
        // with the same JWT auth path the route uses.
        let state = PushApiState {
            auth: Arc::new(FixedAuth(Principal::new("u1", "t1"))),
            registry: registry.clone(),
            tenant_column: Some("org_id".into()),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer jwt-signout".parse().unwrap(),
        );
        let status = push_api::delete_push_token(
            axum::extract::State(state),
            axum::extract::Path("dev-e2e".to_string()),
            headers,
        )
        .await
        .expect("deregistration succeeds");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(
            registry
                .list_by_account("t1", "u1")
                .await
                .unwrap()
                .is_empty(),
            "the REST route must have removed the token row"
        );

        // Phase 2: a fresh burst (past the previous window) reaches nothing.
        burst(&svc).await;
        quiet().await;
        assert_eq!(
            rail.sends.lock().unwrap().len(),
            1,
            "no push to the deregistered token"
        );
    }
}
