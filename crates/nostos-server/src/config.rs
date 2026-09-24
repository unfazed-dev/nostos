//! Command-line / env configuration and the pure boot-time decisions derived from it.

use std::net::SocketAddr;

use clap::Parser;

/// Command-line / env configuration for the sync server.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "nostos-server",
    version,
    about = "Nostos local-first sync server"
)]
// `clippy::struct_excessive_bools` fires at 4+. The lint's real target is a
// domain struct whose bool soup should have been an enum or a state machine —
// but this is a clap argument struct, where one `bool` per flag IS the shape,
// and the alternative (`#[command(flatten)]` sub-structs) would split the
// operator-facing `--help` output to satisfy a lint about internal modelling.
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Bind address.
    #[arg(long, env = "NOSTOS_BIND", default_value = "0.0.0.0:8800")]
    pub(crate) bind: String,

    /// Escape hatch for the anonymous-on-a-public-interface boot guard.
    ///
    /// `NOSTOS_SYNC_AUTH=none` injects no tenant filter, and `NOSTOS_BIND`
    /// defaults to `0.0.0.0` — so the two DEFAULTS together are an
    /// unauthenticated sync server on every interface. Neither default is
    /// wrong alone (anonymous is right for local dev; `0.0.0.0` is right for
    /// a container), which is exactly why the *pair* has to be the thing that
    /// refuses to boot rather than either one changing. Set this only when
    /// something in front of the server is doing the authenticating.
    #[arg(
        long,
        env = "NOSTOS_INSECURE_ANONYMOUS",
        default_value_t = false,
        value_parser = parse_env_bool
    )]
    pub(crate) insecure_anonymous: bool,

    /// WebSocket path clients connect to.
    #[arg(long, env = "NOSTOS_WS_PATH", default_value = "/sync")]
    pub(crate) ws_path: String,

    /// Per-session bounded buffer depth (backpressure).
    #[arg(long, env = "NOSTOS_SESSION_BUFFER", default_value_t = 1024)]
    pub(crate) session_buffer: usize,

    /// ADR-0040: emit `resync_required` to a client whose stream shed
    /// events (buffer full), so it clears local state and reconciles.
    #[arg(long, env = "NOSTOS_RESYNC_SIGNAL", default_value_t = false)]
    pub(crate) resync_signal: bool,

    /// Op-log writer's bounded internal channel depth (ADR-0025 slice 2). The
    /// fan-out loop `try_send`s each event into this buffer; a background task
    /// drains + flushes to `cairn_oplog`. On full, the entry is dropped (the
    /// resume path falls back to snapshot-reconcile for the gap — correct, but
    /// a capacity signal). Default 4096; raise if
    /// `cairn_oplog_dropped_total` is non-zero under sustained load. Only
    /// meaningful under `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_OPLOG_BUFFER", default_value_t = 4096)]
    pub(crate) oplog_buffer: usize,

    /// Op-log retention window in seconds (ADR-0025 slice 5). Rows older than
    /// this are aged out by the compactor. A client whose offline gap exceeds
    /// the window falls back to snapshot-reconcile (slice 1, the safety net).
    /// Default 1h.
    #[arg(long, env = "NOSTOS_OPLOG_RETENTION_SECS", default_value_t = 3600)]
    pub(crate) oplog_retention_secs: u64,

    /// Op-log compaction tick period in seconds (ADR-0025 slice 5). The
    /// compactor collapses duplicate ops per (table_name, pk) + ages out rows
    /// past the retention window. Default 5min.
    #[arg(
        long,
        env = "NOSTOS_OPLOG_COMPACT_INTERVAL_SECS",
        default_value_t = 300
    )]
    pub(crate) oplog_compact_interval_secs: u64,

    /// Replicator mode: "fake" (synthetic generator) or "pg" (real Postgres).
    /// "pg" requires the `pg` feature, which is on by default (disable with
    /// `--no-default-features`). Runtime default stays "fake" so zero-setup
    /// `cargo run` keeps working.
    #[arg(long, env = "NOSTOS_REPLICATOR", default_value = "fake")]
    pub(crate) replicator: String,

    /// Row ceiling for a single subscribe-time snapshot.
    ///
    /// A table with more rows than this is REFUSED, not truncated: a silently
    /// short first sync is indistinguishable from a complete one at the
    /// client, which is the failure shape the stream-snapshot scope bypass
    /// had (audit finding 7). Raise this if a legitimate table is bigger.
    /// Comma-separated allowlist of accepted JWT `iss` values.
    ///
    /// Empty (the default) leaves `iss` unchecked, so a token from ANY issuer
    /// whose `kid` resolves against the configured JWKS is accepted (audit
    /// finding 4). Set this to your project's issuer URL to close that.
    /// Applies to the JWKS path only — the legacy HS256 path lifts no `iss`.
    #[arg(long, env = "NOSTOS_JWT_ISSUERS", default_value = "")]
    pub(crate) jwt_issuers: String,

    /// Seconds a JWKS cache may keep serving after its last SUCCESSFUL fetch.
    ///
    /// Past this the cache fails closed rather than serving keys it can no
    /// longer vouch for — a key the IdP revoked would otherwise keep verifying
    /// for the whole outage (audit finding 6).
    #[arg(long, env = "NOSTOS_JWKS_MAX_STALE_SECS", default_value_t = 1800)]
    pub(crate) jwks_max_stale_secs: u64,

    /// Accept legacy HS256 tokens that carry no `exp` claim.
    ///
    /// Such a token never expires and survives revocation, so it is refused by
    /// default (audit finding 5). Set this only while migrating a token issuer
    /// that cannot yet stamp `exp`.
    #[arg(
        long,
        env = "NOSTOS_ALLOW_JWT_WITHOUT_EXP",
        default_value_t = false,
        value_parser = parse_env_bool
    )]
    pub(crate) allow_jwt_without_exp: bool,

    /// Require authentication on `GET /schema` and `GET /rules` (audit
    /// finding 7).
    ///
    /// Off by default because turning it on is a breaking change for clients
    /// that fetch `/schema` without a token — `nostos pull`, and any Flutter app
    /// built against an SDK older than this change.
    ///
    /// Parsed by [`parse_env_bool`] rather than read ad hoc, so a typo
    /// (`=yes please`, `=True `) refuses to boot instead of silently leaving
    /// the routes open while the log claims the variable is unset.
    #[arg(
        long,
        env = "NOSTOS_PROTECT_METADATA",
        default_value_t = false,
        value_parser = parse_env_bool
    )]
    pub(crate) protect_metadata: bool,

    /// Ceiling on live sync sessions held by a SINGLE account.
    ///
    /// The licensed `device_cap` is global, so without this one account can
    /// open every slot and lock all other tenants out of the deployment
    /// (audit finding 2). One subscribe is one session and a socket may hold
    /// 32 tables, so keep this well above 32.
    #[arg(
        long,
        env = "NOSTOS_PER_PRINCIPAL_SESSION_CAP",
        default_value_t = nostos_application::session::DEFAULT_PER_PRINCIPAL_SESSION_CAP
    )]
    pub(crate) per_principal_session_cap: u64,

    #[arg(
        long,
        env = "NOSTOS_SNAPSHOT_MAX_ROWS",
        default_value_t = nostos_application::ports::DEFAULT_SNAPSHOT_MAX_ROWS
    )]
    pub(crate) snapshot_max_rows: usize,

    /// Fake-replicator emission rate, events/second. `0` = unbounded.
    ///
    /// A10: the default is *paced*, not unbounded. An unbounded synthetic
    /// stream is pure load with no observer — it saturated any interactive
    /// session that outlived a few seconds (ADR-0027 finding). The benchmark
    /// builds its own config (`nostos-bench`), so the measured ceiling is
    /// untouched; set `0` here to firehose deliberately.
    #[arg(long, env = "NOSTOS_FAKE_EPS", default_value_t = 20)]
    pub(crate) fake_events_per_sec: u64,

    /// Fake-replicator distinct primary keys. `0` = monotonic (grows forever).
    ///
    /// Client apply is an upsert on `(table, pk)`, so a bounded key space
    /// bounds the *table* — which keeps a full-table watch snapshot O(1) in
    /// session length. Pacing alone only slows the growth.
    #[arg(long, env = "NOSTOS_FAKE_KEYS", default_value_t = 50)]
    pub(crate) fake_distinct_keys: u64,

    /// Postgres URL for the real replicator (`NOSTOS_REPLICATOR=pg`).
    /// Empty by default — selecting `pg` without setting `NOSTOS_PG_URL` fails
    /// fast with an actionable error (see the replicator match below).
    #[arg(long, env = "NOSTOS_PG_URL", default_value = "")]
    pub(crate) pg_url: String,

    /// Comma-separated list of tables clients may write to over the sync socket
    /// (ADR-0013 write-back v1). Exact-match allowlist — a table not listed
    /// here can never reach the SQL builder. Empty (default) = no tables
    /// writable; clients get a clear "table not writable" error. Example:
    /// `tasks,notes`. Only meaningful under `NOSTOS_REPLICATOR=pg` (the fake
    /// replicator has no source database, so writes return
    /// "write-back requires pg replicator" even when allowlisted).
    #[arg(long, env = "NOSTOS_WRITE_TABLES", default_value = "")]
    pub(crate) write_tables: String,

    /// Comma-separated `table:column` pairs naming the JSONB columns that hold
    /// add-wins OR-sets (ADR-0030). Writes to these tables merge element-wise
    /// server-side instead of clobbering; an OR-set write to an unconfigured
    /// table is rejected client-side. Empty (default) = no OR-set columns (LWW
    /// only). Example: `tasks:tags,notes:labels`. Only meaningful under
    /// `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_OR_SET_COLUMNS", default_value = "")]
    pub(crate) or_set_columns: String,

    /// Comma-separated `table:col` pairs naming PN-Counter CRDT columns
    /// (ADR-0030 addendum). Writes to these tables merge per-replica elementwise
    /// max server-side (state-based CRDT). Empty (default) = no counter columns.
    /// Example: `counts:value`. Only meaningful under `NOSTOS_REPLICATOR=pg`.
    #[arg(long, env = "NOSTOS_COUNTER_COLUMNS", default_value = "")]
    pub(crate) counter_columns: String,

    /// Path to the sync-rules file (ADR-0031). Missing file = `all` mode.
    /// The default falls back to the pre-rename file name (ADR-0046).
    #[arg(long, env = "NOSTOS_RULES_FILE", default_value = nostos_infra::rules_file::RULES_FILE_NAME)]
    pub(crate) rules_file: String,

    /// Coalesce the per-event ack-progress (slot-advance) scan: recompute the
    /// slowest acked LSN every N events instead of every event. `1` = the exact
    /// ADR-0009 per-event cadence. `>1` cuts the O(sessions) scan N× (safe: acks
    /// are monotonic, so a cached min never overshoots the safe-to-flush LSN; at
    /// most N events of extra WAL retention) — see the
    /// `coalesced_ack_progress_lags_but_never_overshoots` test in nostos-application.
    ///
    /// Stays `1`. A 2026-09-21 change to `16` claimed 2.23× at 100k sessions and
    /// was reverted the same day: six interleaved tiers measured `ack=1` at
    /// 39,847 ops/sec mean and `ack=16` at 43,983, with a standard deviation of
    /// ~70% of the mean in BOTH arms (ack=1 spread 7.99×, ack=16 3.41×). The
    /// arms overlap completely, so the original 32,372 → 72,051 was run-to-run
    /// noise, not the knob (benches/results/RESULTS.md).
    ///
    /// The knob does work — `ack_scan` drops ~8× (176 → 23 ms/ev) exactly as
    /// designed. That stage simply is not what bounds throughput at this tier,
    /// so coalescing buys nothing and costs WAL retention. Raise it only with a
    /// measurement on a harness that can resolve the difference; this one
    /// cannot.
    #[arg(long, env = "NOSTOS_ACK_PROGRESS_INTERVAL", default_value = "1")]
    pub(crate) ack_progress_interval: u32,

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
    pub(crate) push_tables: String,

    /// Push coalescer debounce window in milliseconds (ADR-0037 §4): bursts
    /// of hints to one account collapse to ONE push per window. Default 2s.
    #[arg(long, env = "NOSTOS_PUSH_DEBOUNCE_MS", default_value_t = 2000)]
    pub(crate) push_debounce_ms: u64,

    /// Remote nostos-pushd origin for push DELEGATION (ADR-0038 §3, plan
    /// task 2.3), e.g. `http://127.0.0.1:8090`. Both this AND
    /// `NOSTOS_PUSH_REMOTE_KEY` must be set (exactly one of the two is a
    /// config error). When set, `RemoteNotifier` replaces the embedded
    /// `PushRouter`: presence/token resolution/templates stay here, the
    /// daemon is the coalescing rail + receipt log.
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_URL", default_value = "")]
    pub(crate) push_remote_url: String,

    /// Bearer API key for the remote daemon (a tenant key from its
    /// `NOSTOS_PUSHD_API_KEYS`). Sent as `Authorization: Bearer <key>` on
    /// every delegated send and receipts poll.
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_KEY", default_value = "")]
    pub(crate) push_remote_key: String,

    /// Optional state-file path for the delegation receipts cursor
    /// (ADR-0038 §3 restart-resume). When set (and delegation is active),
    /// the receipts poll persists its `since` cursor across nostos-server
    /// restarts: loaded at startup (missing file = fresh start at 0),
    /// written back atomically (tmp+rename) at most once per second.
    /// Unset = in-memory cursor: a restart replays the daemon's receipt
    /// log (metrics-only skew — delivery state is monotonicity-guarded).
    #[arg(long, env = "NOSTOS_PUSH_REMOTE_STATE_PATH", default_value = "")]
    pub(crate) push_remote_state_path: String,

    /// Logical-replication slot name.
    #[arg(long, env = "NOSTOS_PG_SLOT", default_value = "cairn_slot")]
    pub(crate) pg_slot: String,

    /// Publication name.
    #[arg(long, env = "NOSTOS_PG_PUBLICATION", default_value = "cairn_pub")]
    pub(crate) pg_publication: String,

    /// Log filter (RUST_LOG-style).
    #[arg(long, env = "NOSTOS_LOG", default_value = "info,nostos=debug")]
    pub(crate) log: String,

    /// Licensed tier for the concurrent-device cap (the OSS / fallback path).
    /// OSS self-host defaults to `enterprise` (unlimited); a managed Cloud deploy
    /// usually presents a signed `NOSTOS_LICENSE` instead (see below), in which
    /// case this value is ignored. One of: hobby, pro, scale, enterprise.
    #[arg(long, env = "NOSTOS_TIER", default_value = "enterprise")]
    pub(crate) tier: String,

    /// Signed license token from Nostos Cloud (`<payload>.<sig>`). When present,
    /// the server verifies it with `NOSTOS_LICENSE_SECRET`; the token's tier +
    /// `device_cap` then become authoritative (managed mode). Empty (default) =
    /// OSS self-host, and `NOSTOS_TIER` is used instead. A presented-but-invalid
    /// license is fatal — the server refuses to start rather than silently
    /// downgrading to the unlimited OSS default (ADR-0006 trust boundary).
    #[arg(long, env = "NOSTOS_LICENSE", default_value = "", hide = true)]
    pub(crate) license: String,

    /// /sync authentication mode: "none" (anonymous — OSS dev default),
    /// "bearer" (one shared secret → one fixed principal; ADR-0010 addendum),
    /// or "supabase-jwt" (HS256/JWKS-verify a Supabase JWT). A managed
    /// multi-tenant deploy MUST set "supabase-jwt"; "none" refuses to inject
    /// tenant filters and mints the anonymous principal, which the push-token
    /// registry refuses — single-tenant deploys wanting push receipts use
    /// "bearer" (ADR-0010).
    #[arg(long, env = "NOSTOS_SYNC_AUTH", default_value = "none")]
    pub(crate) sync_auth: String,

    /// The shared bearer secret for `NOSTOS_SYNC_AUTH=bearer`: every /sync
    /// connection presenting it becomes one fixed principal (`local`/`local`
    /// — see StaticBearerAuth), which is what makes push-token registration
    /// (ADR-0037 §3) work on a single-tenant self-host without Supabase.
    /// Required (non-empty) when `NOSTOS_SYNC_AUTH=bearer`; ignored otherwise.
    #[arg(long, env = "NOSTOS_SYNC_BEARER_TOKEN", default_value = "")]
    pub(crate) sync_bearer_token: String,

    /// The legacy HS256 shared secret used to verify Supabase JWTs at /sync.
    /// Ignored unless `NOSTOS_SYNC_AUTH=supabase-jwt`. Matches Supabase's
    /// `JWT_SECRET` (the project's GoTrue signing key) — only present on
    /// projects created before 2025-10-01 or that haven't migrated off it.
    /// At least one of this or `NOSTOS_SUPABASE_URL`/`NOSTOS_SUPABASE_JWKS_URL`
    /// must be set when `NOSTOS_SYNC_AUTH=supabase-jwt` (ADR-0010 addendum).
    #[arg(long, env = "NOSTOS_SUPABASE_JWT_SECRET", default_value = "")]
    pub(crate) supabase_jwt_secret: String,

    /// The Supabase project URL (e.g. `https://xyzco.supabase.co`), used to
    /// derive the JWKS URL (`<url>/auth/v1/.well-known/jwks.json`) for
    /// RS256/ES256/EdDSA verification — Supabase's default signing mode for
    /// projects created since 2025-10-01. Ignored unless
    /// `NOSTOS_SYNC_AUTH=supabase-jwt`. Superseded by
    /// `NOSTOS_SUPABASE_JWKS_URL` if both are set.
    #[arg(long, env = "NOSTOS_SUPABASE_URL", default_value = "")]
    pub(crate) supabase_url: String,

    /// Explicit JWKS URL, overriding the one derived from
    /// `NOSTOS_SUPABASE_URL`. Use this for a non-standard gateway/proxy in
    /// front of Supabase's auth endpoint. Ignored unless
    /// `NOSTOS_SYNC_AUTH=supabase-jwt`.
    #[arg(long, env = "NOSTOS_SUPABASE_JWKS_URL", default_value = "")]
    pub(crate) supabase_jwks_url: String,

    /// The tenant column server-enforced on every predicate (e.g. "org_id").
    /// When set with `NOSTOS_SYNC_AUTH=supabase-jwt`, the server ANDs
    /// `<column> = <principal.tenant_id>` into every subscription so the client
    /// cannot read another tenant's rows (ADR-0011). Defaults to "org_id".
    #[arg(long, env = "NOSTOS_TENANT_COLUMN", default_value = "org_id")]
    pub(crate) tenant_column: String,

    /// Sync transport (ADR-0041): "ws" (default — serve /sync over HTTP/WS on
    /// NOSTOS_BIND) or "iroh" (serve sync sessions natively over an iroh
    /// endpoint, printing the QR-native iroh:// dial URL; the HTTP ops
    /// surface still binds NOSTOS_BIND). Requires a build
    /// with `--features iroh`.
    #[arg(long, env = "NOSTOS_TRANSPORT", default_value = "ws")]
    pub(crate) transport: String,

    /// Allowed CORS origins for browser clients, comma-separated (e.g.
    /// "https://app.example.com,http://localhost:3000"). Empty (default) =
    /// permissive (any origin) for local dev; set explicitly for production.
    #[arg(long, env = "NOSTOS_CORS_ORIGINS", default_value = "")]
    pub(crate) cors_origins: String,

    /// Browser origins allowed to open the `/sync` WebSocket, comma-separated.
    /// Empty (default) = **no check**, which is what every existing deployment
    /// gets on upgrade.
    ///
    /// Separate from `NOSTOS_CORS_ORIGINS` on purpose: CORS governs the REST
    /// surface and is enforced by the browser, whereas a WebSocket upgrade is
    /// not subject to CORS at all — the server has to check `Origin` itself or
    /// not at all. Deployments usually want the same list in both, but they are
    /// different mechanisms and collapsing them would hide that.
    #[arg(long, env = "NOSTOS_WS_ORIGINS", default_value = "")]
    pub(crate) ws_origins: String,

    /// WAL-bloat protection: the maximum LSN-gap (in WAL bytes) a live client
    /// may lag behind the head of the stream before it is evicted. A client
    /// exceeding this is disconnected; it reconnects + re-syncs from a fresh
    /// checkpoint — trading a controlled replay window for source-DB safety.
    /// Default `1073741824` (1 GiB). `0` = eviction OFF (no client is ever
    /// dropped for lag; the server logs a startup warning). Eviction only
    /// covers a *running* server with a slow client — an abandoned slot (server
    /// gone) is bounded only by `--pg-slot-wal-keep-size` (ADR-0043).
    ///
    /// Default changed from `0` (OFF) by the v0.2.0 audit (finding 1): with
    /// eviction off, one client that connects, acks nothing and stays
    /// connected pins the replication slot and grows WAL without bound — a
    /// disk-exhaustion attack on the source primary that needs no
    /// credentials beyond a valid sync session. 1 GiB is deliberately
    /// generous: eviction costs a reconnect and resync, not data loss, and a
    /// real client on a bad connection has to fall a gigabyte of WAL behind
    /// before it trips. Set `0` to restore the old unbounded behaviour.
    #[arg(long, env = "NOSTOS_SLOT_MAX_LAG", default_value_t = 1_073_741_824)]
    pub(crate) slot_max_lag: u64,

    /// Postgres `max_slot_wal_keep_size` for the replication slot (MB). Caps how
    /// much WAL a lagging slot may retain on the primary before Postgres
    /// itself invalidates the slot — the database-level backstop for WAL bloat.
    /// `0` (default) = Postgres's built-in default (unbounded). Set alongside
    /// `--slot-max-lag` in production (ADR-0016).
    #[arg(long, env = "NOSTOS_PG_SLOT_WAL_KEEP_SIZE", default_value_t = 0)]
    pub(crate) pg_slot_wal_keep_size: u64,
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
/// The tenant column the deployment enforces, `None` when none applies.
///
/// Tenant scoping (ADR-0011) needs a REAL authenticated identity to scope
/// WITH: `supabase-jwt` scopes per user claim; `bearer` (ADR-0010 addendum)
/// scopes to the deployment's one fixed principal. `none` stays `None` —
/// there is no identity to scope with, and injecting a column no principal
/// backs would silently filter every subscription to zero rows.
///
/// Why bearer wants this set: without a tenant column the fan-out's
/// fully-offline tenant-wide doorbell hint has no tenant source (the
/// documented ponytail in fanout.rs), so a killed-app device can never be
/// doorbelled. With it, mirror rows carry the column and the hint resolves
/// against the registry's single tenant.
/// Parse a boolean env var the way operators actually write one.
///
/// clap's stock `bool` parser accepts ONLY `true`/`false`, so
/// `NOSTOS_INSECURE_ANONYMOUS=1` — the universal shell/compose convention, and
/// what this server's own refusal message tells you to set — fails argument
/// parsing with `invalid value '1'`. That is a security-relevant failure mode:
/// an operator who follows the instructions verbatim gets a server that will
/// not boot, and the obvious way out of a confusing parse error is to turn off
/// whatever they just added. Caught by booting the binary; the unit test on
/// the guard itself passed happily.
fn parse_env_bool(s: &str) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "expected a boolean (1/0, true/false, yes/no, on/off), got '{other}'"
        )),
    }
}

/// Would `bind` make an anonymous `/sync` reachable from off this machine?
///
/// True = the pair is unsafe and boot must refuse (unless the operator set
/// `NOSTOS_INSECURE_ANONYMOUS`). Only a **loopback** address is safe; note that
/// `0.0.0.0` (and `[::]`) are UNSPECIFIED, not loopback — `is_loopback()`
/// returns false for them, which is the answer we want, but it is worth
/// pinning in a test rather than trusting from memory.
///
/// An address that does not parse returns `false`: reporting a malformed bind
/// is not this guard's job, and the canonical `invalid bind address` bail
/// happens later in boot. This cannot open a hole — a bind string the server
/// can't parse is a bind the server never listens on.
pub(crate) fn exposes_anonymous_sync(bind: &str) -> bool {
    bind.parse::<SocketAddr>()
        .is_ok_and(|addr| !addr.ip().is_loopback())
}

/// The ADR-0038 §3 wiring precedence (plan task 2.3) — see [`push_wiring`].
/// Map the `--slot-max-lag` knob onto an [`nostos_application::EvictionPolicy`].
/// `0` is the documented "unbounded" escape hatch; anything else is a byte
/// threshold. Pure so the default/opt-out contract is unit-testable without
/// booting the server (ADR-0043).
pub(crate) fn eviction_policy(slot_max_lag: u64) -> nostos_application::EvictionPolicy {
    if slot_max_lag > 0 {
        nostos_application::EvictionPolicy::new(slot_max_lag)
    } else {
        nostos_application::EvictionPolicy::disabled()
    }
}

/// ADR-0043: the `NOSTOS_SLOT_MAX_LAG` default is a security decision (v0.2.0
/// audit finding 1) — pin it so a refactor can't silently flip it back to
/// unbounded, and pin that `0` still means "eviction OFF".
#[cfg(test)]
mod slot_max_lag_tests {
    use super::{eviction_policy, Config};
    use clap::Parser;

    const ONE_GIB: u64 = 1_073_741_824;

    #[test]
    fn default_is_one_gib() {
        let cfg = Config::parse_from(["nostos-server"]);
        assert_eq!(cfg.slot_max_lag, ONE_GIB);
        assert_eq!(eviction_policy(cfg.slot_max_lag).max_lag, Some(ONE_GIB));
    }

    #[test]
    fn zero_means_unbounded() {
        let cfg = Config::parse_from(["nostos-server", "--slot-max-lag", "0"]);
        assert_eq!(cfg.slot_max_lag, 0);
        assert_eq!(eviction_policy(cfg.slot_max_lag).max_lag, None);
    }

    #[test]
    fn explicit_threshold_is_honoured() {
        let cfg = Config::parse_from(["nostos-server", "--slot-max-lag", "4096"]);
        assert_eq!(eviction_policy(cfg.slot_max_lag).max_lag, Some(4096));
    }
}

#[cfg(test)]
mod anonymous_bind_guard_tests {
    use super::{exposes_anonymous_sync, parse_env_bool};

    /// `=1` is what the refusal message, the compose file, and `nostos dev`
    /// all emit. clap's stock bool parser rejects it — which turned the
    /// escape hatch into an unbootable server until this parser existed.
    #[test]
    fn the_escape_hatch_accepts_how_operators_actually_write_booleans() {
        for yes in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert_eq!(parse_env_bool(yes), Ok(true), "{yes:?} should enable");
        }
        for no in ["0", "false", "no", "off", ""] {
            assert_eq!(parse_env_bool(no), Ok(false), "{no:?} should not enable");
        }
        // Garbage must be a hard error, never a silent `true` — a typo'd
        // hatch value must not open an anonymous server.
        assert!(parse_env_bool("maybe").is_err());
    }

    /// The whole point of the guard is which addresses count as "off-host".
    /// `0.0.0.0` is the DEFAULT bind and is UNSPECIFIED, not loopback — if
    /// `is_loopback()` were true for it the guard would never fire and the
    /// fail-open pair would ship silently. Pin it rather than trust it.
    #[test]
    fn only_loopback_binds_may_serve_anonymous_sync() {
        // Refused: the shipped default, and any concrete off-host interface.
        assert!(exposes_anonymous_sync("0.0.0.0:8800"));
        assert!(exposes_anonymous_sync("[::]:8800"));
        assert!(exposes_anonymous_sync("192.168.1.5:8800"));

        // Allowed: local dev keeps working with zero configuration.
        assert!(!exposes_anonymous_sync("127.0.0.1:8800"));
        assert!(!exposes_anonymous_sync("[::1]:8800"));
        assert!(!exposes_anonymous_sync("127.0.0.53:9999"));

        // Unparseable is NOT this guard's error to raise; boot bails on it
        // later with the canonical message.
        assert!(!exposes_anonymous_sync("not-an-address"));
        assert!(!exposes_anonymous_sync(""));
    }
}
