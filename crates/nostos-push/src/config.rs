//! Daemon configuration (ADR-0038, plan tasks 1.1 + 1.7) — clap `Parser`
//! over `NOSTOS_PUSHD_*` env vars, the nostos-server `Config` pattern.
//!
//! Rail credentials deliberately have NO fields here: the rails configure
//! themselves via their own `from_env()` (`NOSTOS_FCM_CREDENTIALS_JSON`,
//! `NOSTOS_APNS_*`, `NOSTOS_WEBPUSH_VAPID_*` — one env contract across
//! embedded, daemon, and delegation, ADR-0038 §2). This struct owns only the
//! daemon-process concerns.

use clap::Parser;

/// Default [`Config::db`].
pub const DEFAULT_DB: &str = "./cairn-pushd.db";
/// Pre-rename [`DEFAULT_DB`], kept when only it exists (ADR-0046).
pub const LEGACY_DB: &str = "./cairn-pushd.db"; // rename:hold — pre-rename registry file, read as fallback until 1.0 (decision 10, ADR-0046)

/// Command-line / env configuration for nostos-pushd.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "nostos-pushd",
    version,
    about = "Nostos standalone push daemon (ADR-0038) — token-addressed APNs/FCM/Web Push sends with debounce coalescing"
)]
pub struct Config {
    /// Bind address.
    #[arg(long, env = "NOSTOS_PUSHD_BIND", default_value = "127.0.0.1:8090")]
    pub bind: String,

    /// SQLite database path for the daemon-owned token + receipt registry
    /// (plan pin 0.3). ":memory:" works for tests. Ignored when
    /// `database_url` (NOSTOS_PUSHD_DATABASE_URL) is set.
    #[arg(long, env = "NOSTOS_PUSHD_DB", default_value = DEFAULT_DB)]
    pub db: String,

    /// Postgres registry URL (v1.1, ADR-0038 §4 addendum) — selects the
    /// PgStore over the SQLite default. Requires a build with the `pg`
    /// feature: set on a build without it, startup fails fast with the
    /// rebuild hint instead of silently falling back to SQLite. Unset =
    /// the SQLite registry above, untouched.
    #[arg(long, env = "NOSTOS_PUSHD_DATABASE_URL")]
    pub database_url: Option<String>,

    /// Tenant API keys, comma-separated "tenant:secret" pairs (plan pin
    /// 0.2). Parsed and validated at boot — empty or malformed input aborts
    /// startup (fail fast; secrets must not contain commas, and only the
    /// first colon separates tenant from secret).
    #[arg(long, env = "NOSTOS_PUSHD_API_KEYS")]
    pub api_keys: String,

    /// Per-(tenant, token) debounce window in milliseconds (plan pin 0.5 —
    /// mirrors the embedded PushRouter's 2s window in nostos-infra
    /// push/router.rs). The FIRST send in a window fixes the flush deadline;
    /// later sends only replace the pending payload.
    #[arg(long, env = "NOSTOS_PUSHD_DEBOUNCE_MS", default_value_t = 2000)]
    pub debounce_ms: u64,

    /// Receipt retention in seconds (default 7 days). Aged-out receipts are
    /// swept periodically; pollers must persist anything they need.
    #[arg(
        long,
        env = "NOSTOS_PUSHD_RECEIPT_RETENTION_SECS",
        default_value_t = 604_800
    )]
    pub receipt_retention_secs: u64,

    /// Sustained /v1/send rate per tenant, requests/sec (2026-08-17
    /// security audit, plan task 4.1 — finding 2).
    /// ponytail: 10/sec is a daemon-shape default, not a measurement;
    /// the upgrade path is per-key limits once key CRUD lands (v1.1) —
    /// until then one daemon, one policy.
    #[arg(long, env = "NOSTOS_PUSHD_SEND_RATE_PER_SEC", default_value_t = 10)]
    pub send_rate_per_sec: u32,

    /// /v1/send burst size per tenant (max instantaneous requests before
    /// the 429s start; refills at the sustained rate above). Same audit
    /// ponytail as the rate knob.
    #[arg(long, env = "NOSTOS_PUSHD_SEND_BURST", default_value_t = 50)]
    pub send_burst: u32,

    /// Max distinct (tenant, token) keys with an open debounce window in
    /// the coalescer (audit finding 2). A send for a NEW key beyond this
    /// is refused with 429 at the route.
    /// ponytail: 10k is a pinned safe ceiling, not a measurement; upgrade
    /// path is deriving it from observed window occupancy.
    #[arg(long, env = "NOSTOS_PUSHD_PENDING_KEYS_MAX", default_value_t = 10_000)]
    pub pending_keys_max: usize,

    /// Max coalesced-away sends retained per key (audit finding 2); the
    /// oldest beyond the cap is receipted as coalesced at flush. Same
    /// audit ponytail as the pending-key ceiling.
    #[arg(long, env = "NOSTOS_PUSHD_LOSERS_MAX", default_value_t = 64)]
    pub losers_max: usize,

    /// Total attempts per send after a transient (429/5xx/network) rail
    /// outcome: 1 initial + the rest deferred retries (B2 of the arxa
    /// integration plan). Default 2 — one deferred retry, the embedded
    /// router's doorbell semantics: a send that fails twice is receipted
    /// transient and abandoned; the next event re-pushes, and the durable
    /// LSN checkpoint reconciles data regardless.
    #[arg(long, env = "NOSTOS_PUSHD_RETRY_MAX_ATTEMPTS", default_value_t = 2)]
    pub retry_max_attempts: u8,

    /// How long a transiently-failed send waits before its deferred
    /// retry (the re-debounce delay).
    #[arg(long, env = "NOSTOS_PUSHD_RETRY_DELAY_MS", default_value_t = 500)]
    pub retry_delay_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::{Config, LEGACY_DB};
    use clap::Parser;

    #[test]
    fn legacy_db_is_the_pre_rename_name() {
        assert_eq!(LEGACY_DB, "./cairn-pushd.db"); // rename:hold — pins the fallback to the registry deployments already have
    }

    #[test]
    fn defaults_match_the_plan_pins() {
        // api_keys is required with no default; present-but-empty is still
        // rejected at boot by ApiKeys::parse, the fail-fast authority (pin 0.2).
        let cfg = Config::parse_from(["nostos-pushd", "--api-keys", "acme:s3cr3t"]);
        assert_eq!(cfg.bind, "127.0.0.1:8090");
        assert_eq!(cfg.db, "./cairn-pushd.db");
        assert_eq!(cfg.database_url, None, "SQLite stays the default registry");
        assert_eq!(cfg.api_keys, "acme:s3cr3t");
        assert_eq!(cfg.debounce_ms, 2000);
        assert_eq!(cfg.receipt_retention_secs, 604_800);
        assert_eq!(cfg.send_rate_per_sec, 10);
        assert_eq!(cfg.send_burst, 50);
        assert_eq!(cfg.pending_keys_max, 10_000);
        assert_eq!(cfg.losers_max, 64);
        assert_eq!(cfg.retry_max_attempts, 2);
        assert_eq!(cfg.retry_delay_ms, 500);
    }

    #[test]
    fn api_keys_arg_is_required() {
        assert!(Config::try_parse_from(["nostos-pushd"]).is_err());
    }
}
