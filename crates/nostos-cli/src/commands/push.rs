//! `nostos push` — configure and validate push credentials (ADR-0038, plan
//! tasks 3.1–3.2 in `docs/plans/nostos-push-daemon-implementation.md`; the
//! `docs/push.md` page is task 3.3).
//!
//! One env-var contract serves BOTH consumers — the embedded rails inside
//! nostos-server (ADR-0037) and the standalone nostos-pushd daemon (ADR-0038):
//!
//! | rail | env (source of truth: `crates/nostos-infra/src/push/mod.rs`) |
//! |---|---|
//! | APNs | `NOSTOS_APNS_KEY_P8` (p8 PEM or path), `NOSTOS_APNS_KEY_ID`, `NOSTOS_APNS_TEAM_ID`, `NOSTOS_APNS_BUNDLE_ID`, optional `NOSTOS_APNS_SANDBOX=1` |
//! | FCM | `NOSTOS_FCM_CREDENTIALS_JSON` (service-account JSON, path or inline) |
//! | Web Push | `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` (base64url P-256 scalar), `NOSTOS_WEBPUSH_VAPID_SUBJECT` (`mailto:`) |
//!
//! `init` is flag-driven (non-interactive, like `deploy`/`rules init` —
//! scripts and CI must be able to drive it) and writes ONLY the gitignored
//! `.env` via the hand-rolled dotenv module: `nostos.toml` stays secret-free
//! by design (`config.rs`). `check` re-reads `.env` + process env (process
//! env wins) and dry-runs each configured rail — the APNs provider JWT and
//! FCM OAuth2 token are minted exactly like the rails mint them
//! (`apns.rs` `provider_token`, `fcm.rs` `access_token`). Both commands
//! verify credential shape and provider reachability, never end-to-end
//! delivery (ADR-0037 "honest limits").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};

use crate::dotenv;
use clap::{Args, Subcommand};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// The env-var contract — must match crates/nostos-infra/src/push/mod.rs:22-28
// verbatim (pinned by `env_var_constants_match_infra_push_contract`).
// ---------------------------------------------------------------------------
pub const ENV_APNS_KEY_P8: &str = "NOSTOS_APNS_KEY_P8";
pub const ENV_APNS_KEY_ID: &str = "NOSTOS_APNS_KEY_ID";
pub const ENV_APNS_TEAM_ID: &str = "NOSTOS_APNS_TEAM_ID";
pub const ENV_APNS_BUNDLE_ID: &str = "NOSTOS_APNS_BUNDLE_ID";
pub const ENV_APNS_SANDBOX: &str = "NOSTOS_APNS_SANDBOX";
pub const ENV_FCM_CREDENTIALS_JSON: &str = "NOSTOS_FCM_CREDENTIALS_JSON";
pub const ENV_WEBPUSH_VAPID_PRIVATE_KEY: &str = "NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY";
pub const ENV_WEBPUSH_VAPID_SUBJECT: &str = "NOSTOS_WEBPUSH_VAPID_SUBJECT";

/// FCM OAuth2 token endpoint + scope — mirrors `fcm.rs`.
const FCM_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
/// `--probe` target: Apple's development gateway. A plain TLS handshake —
/// no HTTP request, no device token, so no notification can be sent.
const APNS_PROBE_HOST: &str = "api.development.push.apple.com";
const APNS_PROBE_PORT: u16 = 443;
/// A provider JWT we just minted must carry an `iat` within this window.
const JWT_FRESHNESS_SECS: u64 = 60;

#[derive(Debug, Args)]
pub struct PushArgs {
    #[command(subcommand)]
    pub command: PushCommand,
}

#[derive(Debug, Subcommand)]
pub enum PushCommand {
    /// Validate push credentials and write them to .env (never nostos.toml).
    Init(InitPushArgs),
    /// Dry-run every configured rail: shape checks + optional reachability.
    Check(CheckPushArgs),
    /// Manage hashed-at-rest daemon API keys in the nostos-pushd registry
    /// (B2 of the arxa integration plan). The daemon merges these over its
    /// env keys at boot — the store wins on a tenant collision.
    Key(KeyArgs),
}

/// `nostos push key …` wrapper (the nested subcommand pattern).
#[derive(Debug, Args)]
pub struct KeyArgs {
    #[command(subcommand)]
    pub command: KeyCommand,
}

/// `nostos push key <add|list|revoke>`.
#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Mint a new secret for a tenant, store its SHA-256, print the secret
    /// ONCE. Per-tenant send limits are optional (--rate-per-sec/--burst).
    Add(KeyAddArgs),
    /// List stored keys: tenant, role, limits — never the digest alone,
    /// never a secret (there is none on disk to show).
    List(KeyListArgs),
    /// Delete a tenant's key (the tenant immediately falls back to its env
    /// key at next daemon boot, if any).
    Revoke(KeyRevokeArgs),
}

#[derive(Debug, Args)]
pub struct KeyAddArgs {
    /// The registry database (the daemon's NOSTOS_PUSHD_DB).
    #[arg(long, default_value = "./nostos-pushd.db")]
    pub db: PathBuf,
    /// Tenant this key authenticates (one key per tenant).
    #[arg(long)]
    pub tenant: String,
    /// Grant the rail role (rail-mode dispatch for RemoteNotifier
    /// delegation); default is the standard registry-path role.
    #[arg(long)]
    pub rail: bool,
    /// Per-tenant send-rate override (per second). Omit for daemon default.
    #[arg(long, requires = "burst")]
    pub rate_per_sec: Option<u32>,
    /// Per-tenant burst override. Omit for daemon default.
    #[arg(long, requires = "rate_per_sec")]
    pub burst: Option<u32>,
}

#[derive(Debug, Args)]
pub struct KeyListArgs {
    /// The registry database.
    #[arg(long, default_value = "./nostos-pushd.db")]
    pub db: PathBuf,
}

#[derive(Debug, Args)]
pub struct KeyRevokeArgs {
    /// The registry database.
    #[arg(long, default_value = "./nostos-pushd.db")]
    pub db: PathBuf,
    /// The tenant whose key is revoked.
    #[arg(long)]
    pub tenant: String,
}

/// Four bools is what the surface IS — one rail-selector flag per rail plus
/// `--force`. Collapsing them into an enum would trade four obvious flags
/// for `--rail apns --rail fcm` repetition, so the pedantic bool-count lint
/// is allowed here on purpose.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct InitPushArgs {
    /// Configure the APNs rail (needs --apns-key-p8/-key-id/-team-id/-bundle-id).
    #[arg(long)]
    pub apns: bool,
    /// Configure the FCM rail (needs --fcm-credentials-json).
    #[arg(long)]
    pub fcm: bool,
    /// Configure the Web Push rail (mints a fresh VAPID keypair; needs
    /// --vapid-subject).
    #[arg(long)]
    pub webpush: bool,
    /// Env file to write (created if absent; default .env).
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Overwrite non-blank values that already exist in the env file.
    #[arg(long)]
    pub force: bool,
    /// APNs: the .p8 key — a filesystem path or the inline PEM.
    #[arg(long, requires = "apns")]
    pub apns_key_p8: Option<String>,
    /// APNs: the 10-character key id from the Apple developer console.
    #[arg(long, requires = "apns")]
    pub apns_key_id: Option<String>,
    /// APNs: the 10-character team id.
    #[arg(long, requires = "apns")]
    pub apns_team_id: Option<String>,
    /// APNs: the app's bundle id (the apns-topic the rails send with).
    #[arg(long, requires = "apns")]
    pub apns_bundle_id: Option<String>,
    /// FCM: the service-account JSON — a filesystem path or inline JSON.
    #[arg(long, requires = "fcm")]
    pub fcm_credentials_json: Option<String>,
    /// Web Push: the VAPID contact subject (must start with mailto:).
    #[arg(long, requires = "webpush")]
    pub vapid_subject: Option<String>,
}

#[derive(Debug, Args)]
pub struct CheckPushArgs {
    /// Env file to read (default .env; process env overrides it).
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// APNs only: also attempt a TLS handshake to
    /// api.development.push.apple.com:443. Never sends a notification.
    #[arg(long)]
    pub probe: bool,
}

pub async fn run(args: PushArgs, cwd: &Path) -> Result<()> {
    match args.command {
        PushCommand::Init(init_args) => run_init(&init_args, cwd),
        PushCommand::Check(check_args) => run_check(check_args, cwd).await,
        PushCommand::Key(key) => run_key(key.command).await,
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// `nostos push key …` — CRUD over the daemon registry's hashed-at-rest key
/// store (B2). The CLI opens the SAME SQLite file nostos-pushd serves from
/// (NOSTOS_PUSHD_DB); SQLite's file locking makes a write while the daemon
/// runs safe (WAL). Key rotation story: add a new secret for the tenant
/// (the upsert replaces the digest), restart the daemon (keys load at
/// boot), distribute the new secret, done.
async fn run_key(cmd: KeyCommand) -> Result<()> {
    use nostos_infra::push::store::{SqliteStore, Store, StoredApiKey};
    use rand::RngCore;
    use sha2::{Digest, Sha256};

    match cmd {
        KeyCommand::Add(args) => {
            // Mint 32 random bytes, base64 — printable, paste-able, 256-bit.
            let mut secret_bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut secret_bytes);
            let secret = b64url(&secret_bytes);
            let digest: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
            let db = args.db.to_string_lossy().to_string();
            let store = SqliteStore::open(&db).with_context(|| format!("opening registry {db}"))?;
            let key = StoredApiKey {
                tenant_id: args.tenant.clone(),
                secret_digest: digest,
                role: if args.rail { "rail" } else { "standard" }.to_string(),
                rate_per_sec: args.rate_per_sec,
                burst: args.burst,
            };
            store.upsert_api_key(&key).await?;
            println!(
                "tenant {} key stored (role {}, limits {}).",
                args.tenant,
                if args.rail { "rail" } else { "standard" },
                match (args.rate_per_sec, args.burst) {
                    (Some(r), Some(b)) => format!("{r}/s burst {b}"),
                    _ => "daemon default".to_string(),
                },
            );
            println!();
            println!("  secret: {secret}");
            println!();
            println!("  This is the ONLY time the secret is shown — only its");
            println!("  SHA-256 is stored. Use it as the Bearer token:");
            println!("    Authorization: Bearer {secret}");
            if args.rail {
                println!("  This key may use rail-mode dispatch (POST /v1/send");
                println!("  with an unregistered token + platform field).");
            }
            println!("  Restart nostos-pushd to load the new key (keys merge at boot).");
            Ok(())
        }
        KeyCommand::List(args) => {
            let db = args.db.to_string_lossy().to_string();
            let store = SqliteStore::open(&db).with_context(|| format!("opening registry {db}"))?;
            let keys = store.list_api_keys().await?;
            if keys.is_empty() {
                println!("no stored keys (daemon uses its env keys only)");
                return Ok(());
            }
            for k in keys {
                let limits = match (k.rate_per_sec, k.burst) {
                    (Some(r), Some(b)) => format!("{r}/s burst {b}"),
                    _ => "default".to_string(),
                };
                println!("{}  role={}  limits={}", k.tenant_id, k.role, limits);
            }
            Ok(())
        }
        KeyCommand::Revoke(args) => {
            let db = args.db.to_string_lossy().to_string();
            let store = SqliteStore::open(&db).with_context(|| format!("opening registry {db}"))?;
            if store.delete_api_key(&args.tenant).await? {
                println!(
                    "revoked key for tenant {} (env key, if any, applies at next boot)",
                    args.tenant
                );
            } else {
                println!("no stored key for tenant {}", args.tenant);
            }
            Ok(())
        }
    }
}
/// Flag-driven and non-interactive by design (matching `deploy` and `rules
/// init`, the CLI's other scripting surfaces): every value arrives as a
/// flag, so CI and provisioning scripts can drive credential setup without
/// a TTY. Validation is all-or-nothing — a rail that fails validation
/// leaves the env file untouched.
fn run_init(args: &InitPushArgs, cwd: &Path) -> Result<()> {
    if !args.apns && !args.fcm && !args.webpush {
        bail!("pick at least one rail: --apns, --fcm, --webpush");
    }
    let env_path = cwd.join(&args.env_file);
    let existing = dotenv::read(&env_path);
    let mut writer = EnvWriter::new(&env_path, &existing, args.force);

    if args.apns {
        init_apns(args, &mut writer)?;
    }
    if args.fcm {
        init_fcm(args, &mut writer)?;
    }
    if args.webpush {
        init_webpush(args, &mut writer)?;
    }
    writer.finish()?;

    // `nostos init` precedent: advise, never edit .gitignore on the user's
    // behalf.
    let gitignore = std::fs::read_to_string(cwd.join(".gitignore")).unwrap_or_default();
    let env_file = args.env_file.to_string_lossy();
    if !gitignore.lines().any(|l| l.trim() == env_file.as_ref()) {
        println!(
 "note: add {} to .gitignore — it holds rail secrets (never edited automatically; secrets never go in nostos.toml)",
            args.env_file.display()
        );
    }
    println!("next: `nostos push check` dry-runs every configured rail");
    Ok(())
}

/// Validates first, writes via `writer` only after every input parsed — so a
/// bad key id cannot leave a half-written APns block behind.
fn init_apns(args: &InitPushArgs, writer: &mut EnvWriter<'_>) -> Result<()> {
    let key_id = required(args.apns_key_id.as_deref(), "--apns-key-id")?;
    let team_id = required(args.apns_team_id.as_deref(), "--apns-team-id")?;
    let bundle_id = required(args.apns_bundle_id.as_deref(), "--apns-bundle-id")?;
    validate_apns_ids(key_id, team_id, bundle_id)
        .map_err(|e| anyhow::anyhow!("--apns flags: {e}"))?;

    let p8_input = required(args.apns_key_p8.as_deref(), "--apns-key-p8")?;
    if p8_input.contains("-----BEGIN") {
        validate_p8_pem(p8_input).map_err(|e| anyhow::anyhow!("--apns-key-p8: {e}"))?;
        writer.set("apns", ENV_APNS_KEY_P8, &flatten_pem(p8_input))?;
    } else {
        // A path: read + validate now, store the path itself (the rails'
        // from_env re-reads it at boot — mirrors apns.rs).
        let pem = std::fs::read_to_string(p8_input)
            .with_context(|| format!("reading --apns-key-p8 path {p8_input:?}"))?;
        validate_p8_pem(&pem).map_err(|e| anyhow::anyhow!("--apns-key-p8 ({p8_input}): {e}"))?;
        writer.set("apns", ENV_APNS_KEY_P8, p8_input)?;
        writer.note(format!(
 "apns: stored the path {p8_input:?} — it must stay readable from the server/daemon's working directory"
        ));
    }
    writer.set("apns", ENV_APNS_KEY_ID, key_id)?;
    writer.set("apns", ENV_APNS_TEAM_ID, team_id)?;
    writer.set("apns", ENV_APNS_BUNDLE_ID, bundle_id)?;
    Ok(())
}

/// `--fcm-credentials-json` accepts a path or inline JSON, but the stored
/// value is ALWAYS the minified JSON itself: unlike `NOSTOS_APNS_KEY_P8`
/// (whose rail resolves paths), the FCM rail's `from_env` parses
/// `NOSTOS_FCM_CREDENTIALS_JSON` as JSON directly — a stored path would be a
/// config the embedded server cannot boot with, so init inlines it.
fn init_fcm(args: &InitPushArgs, writer: &mut EnvWriter<'_>) -> Result<()> {
    let creds = required(
        args.fcm_credentials_json.as_deref(),
        "--fcm-credentials-json",
    )?;
    let (json_text, from_path) = if creds.trim_start().starts_with('{') {
        (creds.to_string(), false)
    } else {
        let text = std::fs::read_to_string(creds)
            .with_context(|| format!("reading --fcm-credentials-json path {creds:?}"))?;
        (text, true)
    };
    parse_service_account(&json_text)
        .map_err(|e| anyhow::anyhow!("--fcm-credentials-json: {e}"))?;
    // Re-serialize minified: the .env writer is line-based, and a single-line
    // JSON is exactly what the FCM rail's from_env parses.
    let value: Value =
        serde_json::from_str(&json_text).context("re-serializing service-account json")?;
    let minified = serde_json::to_string(&value).context("minifying service-account json")?;
    writer.set("fcm", ENV_FCM_CREDENTIALS_JSON, &minified)?;
    if from_path {
        writer.note(format!(
            "fcm: inlined the service-account JSON from {creds:?} — NOSTOS_FCM_CREDENTIALS_JSON \\
             must be the JSON itself (the FCM rail does not read paths, unlike the APNs p8)"
        ));
    }
    Ok(())
}
fn init_webpush(args: &InitPushArgs, writer: &mut EnvWriter<'_>) -> Result<()> {
    let subject = required(args.vapid_subject.as_deref(), "--vapid-subject")?;
    anyhow::ensure!(
        subject.starts_with("mailto:"),
        "--vapid-subject must start with `mailto:` (e.g. `mailto:ops@example.com`)"
    );
    // Mint + print only when the private key will actually be written: on a
    // skip (key already set, no --force) a printed public key would describe
    // a keypair that is NOT on disk — actively misleading for the client
    // side that copies it in.
    if writer.will_set(ENV_WEBPUSH_VAPID_PRIVATE_KEY) {
        let (private_b64, public_b64) = mint_vapid_keypair();
        writer.set("webpush", ENV_WEBPUSH_VAPID_PRIVATE_KEY, &private_b64)?;
        // The public key is not a secret — the client side needs it to subscribe.
        writer.note(
            "webpush: minted a fresh VAPID P-256 keypair. Public key (the CLIENT side needs this):"
                .to_string(),
        );
        writer.note(format!(" {public_b64}"));
    }
    writer.set("webpush", ENV_WEBPUSH_VAPID_SUBJECT, subject)?;
    Ok(())
}

/// Deferred writes with skip-tracking: `set` decides wrote-vs-skipped, and
/// `finish` performs the actual `dotenv::set` calls only after every rail
/// validated (all-or-nothing per invocation). Skipped values are never
/// echoed — only the key name is recorded.
struct EnvWriter<'a> {
    path: &'a Path,
    existing: &'a BTreeMap<String, String>,
    force: bool,
    /// (rail, key, value)
    pending: Vec<(&'static str, &'static str, String)>,
    /// (rail, key) — set but non-blank already, and `--force` absent.
    skipped: Vec<(&'static str, &'static str)>,
    notes: Vec<String>,
}

impl<'a> EnvWriter<'a> {
    fn new(path: &'a Path, existing: &'a BTreeMap<String, String>, force: bool) -> Self {
        Self {
            path,
            existing,
            force,
            pending: Vec::new(),
            skipped: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Would `set` write this key? (Absent/blank existing value, or force.)
    /// `init_webpush` uses it to skip minting a keypair nobody will store.
    fn will_set(&self, key: &str) -> bool {
        self.force || self.existing.get(key).is_none_or(|v| v.trim().is_empty())
    }

    /// Record one `KEY=value`. Blank existing values are always replaced
    /// (they configure nothing); non-blank ones are skipped unless `--force`.
    fn set(&mut self, rail: &'static str, key: &'static str, value: &str) -> Result<()> {
        if value.contains('\n') || value.contains('\r') {
            // Unreachable for every current caller (flatten_pem/minified JSON
            // guarantee single lines) — the guard keeps a future caller from
            // corrupting the line-based .env format.
            bail!(
                "{key} value must be single-line for {}",
                self.path.display()
            );
        }
        if let Some(current) = self.existing.get(key) {
            if !current.trim().is_empty() && !self.force {
                self.skipped.push((rail, key));
                return Ok(());
            }
        }
        self.pending.push((rail, key, value.to_string()));
        Ok(())
    }

    fn note(&mut self, text: String) {
        self.notes.push(text);
    }

    /// Rails in first-touch order, so the report groups like the flags.
    fn rails_in_order(&self) -> Vec<&'static str> {
        let mut rails: Vec<&'static str> = Vec::new();
        for (rail, _, _) in &self.pending {
            if !rails.contains(rail) {
                rails.push(rail);
            }
        }
        for (rail, _) in &self.skipped {
            if !rails.contains(rail) {
                rails.push(rail);
            }
        }
        rails
    }

    fn finish(self) -> Result<()> {
        let rails = self.rails_in_order();
        for (_, key, value) in &self.pending {
            dotenv::set(self.path, key, value)
                .with_context(|| format!("writing {key} to {}", self.path.display()))?;
        }
        for rail in rails {
            let wrote: Vec<&str> = self
                .pending
                .iter()
                .filter(|(r, _, _)| *r == rail)
                .map(|(_, k, _)| *k)
                .collect();
            let skipped: Vec<&str> = self
                .skipped
                .iter()
                .filter(|(r, _)| *r == rail)
                .map(|(_, k)| *k)
                .collect();
            if !wrote.is_empty() {
                println!(
                    "\u{2713} {rail}: wrote {} \u{2192} {}",
                    wrote.join(", "),
                    self.path.display()
                );
            }
            if !skipped.is_empty() {
                println!(
                    "! {rail}: {} already set \u{2014} skipped (pass --force to overwrite)",
                    skipped.join(", ")
                );
            }
        }
        for note in &self.notes {
            println!("{note}");
        }
        Ok(())
    }
}

fn required<'a>(value: Option<&'a str>, flag: &str) -> Result<&'a str> {
    value
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{flag} is required for this rail"))
}

// ---------------------------------------------------------------------------
// init — pure validators/minters (unit-tested)
// ---------------------------------------------------------------------------

/// Apple key ids are exactly 10 characters; team/bundle just need to be
/// non-blank (shapes Apple does not pin, so neither do we).
fn validate_apns_ids(key_id: &str, team_id: &str, bundle_id: &str) -> Result<(), String> {
    let len = key_id.chars().count();
    if len != 10 {
        return Err(format!(
            "key id must be exactly 10 characters (Apple key id; got {len})"
        ));
    }
    if team_id.trim().is_empty() {
        return Err("team id must not be empty".to_string());
    }
    if bundle_id.trim().is_empty() {
        return Err("bundle id must not be empty".to_string());
    }
    Ok(())
}

/// p8 shape: a PEM private-key block that parses as an EC (P-256) key via the
/// exact call the APNs rail uses (`apns.rs` `with_base`). Accepts both the
/// PKCS#8 `BEGIN PRIVATE KEY` Apple ships and the SEC1
/// `BEGIN EC PRIVATE KEY` some exporters produce.
fn validate_p8_pem(pem: &str) -> Result<(), String> {
    let has_pkcs8 = pem.contains("-----BEGIN PRIVATE KEY-----");
    let has_sec1 = pem.contains("-----BEGIN EC PRIVATE KEY-----");
    if !has_pkcs8 && !has_sec1 {
        return Err(
            "no -----BEGIN PRIVATE KEY----- block (expected a .p8 PEM private key)".to_string(),
        );
    }
    EncodingKey::from_ec_pem(pem.as_bytes())
        .map_err(|e| format!("key does not parse as a P-256 EC key: {e}"))
        .map(|_| ())
}

/// Collapse a PEM to one line so the line-based .env writer (and reader) can
/// carry it. `from_ec_pem` parses the flattened form — same call the rail
/// makes on the raw env value — so the inline form round-trips into the
/// server/daemon. `inline_p8_stored_flattened_still_parses` pins that down.
fn flatten_pem(pem: &str) -> String {
    pem.lines().collect()
}

/// Resolve a stored `NOSTOS_APNS_KEY_P8` value to PEM text: inline (contains
/// `-----BEGIN`, possibly flattened) or a filesystem path. Mirrors `apns.rs/// `from_env`, including its discipline of never echoing the value — the
/// error carries only the length.
fn resolve_p8_stored(value: &str) -> Result<String, String> {
    if value.contains("-----BEGIN") {
        Ok(value.to_string())
    } else {
        std::fs::read_to_string(value).map_err(|e| {
            format!(
                "not a PEM (no -----BEGIN) and not a readable path (value is {} chars): {e}",
                value.len()
            )
        })
    }
}

#[derive(Debug)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
}

/// Service-account JSON shape (mirrors `fcm.rs` `ServiceAccount`):
/// `client_email`, `private_key` (must be a PEM RSA key — checked with the
/// same `from_rsa_pem` call the rail uses), `project_id`. Extra fields are
/// allowed and preserved by `init_fcm`'s minified re-serialization.
fn parse_service_account(json_text: &str) -> Result<ServiceAccount, String> {
    let value: Value =
        serde_json::from_str(json_text).map_err(|e| format!("service-account json: {e}"))?;
    let nonempty = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let client_email = nonempty("client_email")
        .ok_or_else(|| "service-account json: client_email is missing or empty".to_string())?
        .to_string();
    // Required for validity (the rail builds project-scoped send URLs from
    // it) but not retained here — the check path never sends.
    nonempty("project_id")
        .ok_or_else(|| "service-account json: project_id is missing or empty".to_string())?;
    let private_key = nonempty("private_key")
        .ok_or_else(|| "service-account json: private_key is missing or empty".to_string())?
        .to_string();
    if !private_key.contains("-----BEGIN") {
        return Err("service-account private_key is not a PEM (no -----BEGIN block)".to_string());
    }
    EncodingKey::from_rsa_pem(private_key.as_bytes())
        .map_err(|e| format!("service-account private_key is not an RSA PEM: {e}"))?;
    Ok(ServiceAccount {
        client_email,
        private_key,
    })
}

/// Mint a fresh VAPID server keypair (the `webpush.rs` test-keymint recipe):
/// returns (base64url-no-pad of the 32-byte scalar, base64url-no-pad of the
/// 65-byte uncompressed public point). The private half is a secret; the
/// public half is what the client subscribes with.
fn mint_vapid_keypair() -> (String, String) {
    use p256::ecdsa::SigningKey;
    use rand::thread_rng;

    let key = SigningKey::random(&mut thread_rng());
    let scalar = key.as_nonzero_scalar().to_bytes().to_vec();
    let point = key.verifying_key().to_encoded_point(false);
    (b64url(&scalar), b64url(point.as_bytes()))
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

async fn run_check(args: CheckPushArgs, cwd: &Path) -> Result<()> {
    let env_path = cwd.join(&args.env_file);
    let vars = merged_env(&env_path);
    println!(
        "push credentials — {} (process env overrides)",
        env_path.display()
    );
    let mut ok = true;

    match rail_state(
        &vars,
        &[
            ENV_APNS_KEY_P8,
            ENV_APNS_KEY_ID,
            ENV_APNS_TEAM_ID,
            ENV_APNS_BUNDLE_ID,
        ],
    ) {
        RailState::Off => println!("— apns: not configured (no NOSTOS_APNS_* set)"),
        RailState::Partial(missing) => {
            ok = false;
            println!(
 "\u{2717} apns: partially configured — missing {} — set all four together or run `nostos push init --apns ...`",
                missing.join(", ")
            );
        }
        RailState::On => check_apns(&vars, args.probe, &mut ok).await,
    }

    match vars
        .get(ENV_FCM_CREDENTIALS_JSON)
        .filter(|v| !v.trim().is_empty())
    {
        None => println!("— fcm: not configured (NOSTOS_FCM_CREDENTIALS_JSON unset)"),
        Some(credentials) => check_fcm(credentials, &mut ok).await,
    }

    match rail_state(
        &vars,
        &[ENV_WEBPUSH_VAPID_PRIVATE_KEY, ENV_WEBPUSH_VAPID_SUBJECT],
    ) {
        RailState::Off => println!("— webpush: not configured (no NOSTOS_WEBPUSH_VAPID_* set)"),
        RailState::Partial(missing) => {
            ok = false;
            println!(
 "\u{2717} webpush: missing {} — run `nostos push init --webpush --vapid-subject mailto:you@example.com` to mint a keypair",
                missing.join(", ")
            );
        }
        RailState::On => match check_webpush_shape(
            vars[ENV_WEBPUSH_VAPID_PRIVATE_KEY].as_str(),
            vars[ENV_WEBPUSH_VAPID_SUBJECT].as_str(),
        ) {
            Ok(()) => println!(
                "\u{2713} webpush: private key decodes to a 32-byte P-256 scalar; subject {}",
                vars[ENV_WEBPUSH_VAPID_SUBJECT]
            ),
            Err(e) => {
                ok = false;
                println!(
 "\u{2717} webpush: {e} — run `nostos push init --webpush --vapid-subject mailto:you@example.com --force` to mint a fresh keypair"
                );
            }
        },
    }

    println!(
 "note: these checks verify credential shape and reachability — never end-to-end delivery (ADR-0037 honest limits)"
    );
    if !ok {
        bail!("push check found failing rails");
    }
    Ok(())
}

/// `check`'s view of the world: dotenv (`.env`) overlaid by any non-blank
/// `NOSTOS_*` process env vars — a platform-injected secret must win over a
/// stale local file, the standard dotenv precedence. Pre-rename names count
/// too (ADR-0046).
#[allow(clippy::disallowed_methods)] // prefix scan; fold_legacy adds the fallback
fn merged_env(env_path: &Path) -> BTreeMap<String, String> {
    use nostos_infra::env::{fold_legacy, LEGACY_PREFIX, PREFIX};
    let mut vars = dotenv::read(env_path);
    for (name, value) in std::env::vars() {
        if (name.starts_with(PREFIX) || name.starts_with(LEGACY_PREFIX)) && !value.trim().is_empty()
        {
            vars.insert(name, value);
        }
    }
    fold_legacy(&mut vars);
    vars
}

#[derive(Debug)]
enum RailState {
    Off,
    Partial(Vec<&'static str>),
    On,
}

/// A rail is Off (nothing set), On (all `need` set and non-blank), or
/// Partial (some set — the misconfiguration the rails' `from_env` treats
/// as a hard error, surfaced here as a failing check line).
fn rail_state(vars: &BTreeMap<String, String>, need: &[&'static str]) -> RailState {
    let missing: Vec<&'static str> = need
        .iter()
        .copied()
        .filter(|k| vars.get(*k).is_none_or(|v| v.trim().is_empty()))
        .collect();
    match missing.len() {
        0 => RailState::On,
        n if n == need.len() => RailState::Off,
        _ => RailState::Partial(missing),
    }
}

async fn check_apns(vars: &BTreeMap<String, String>, probe: bool, ok: &mut bool) {
    let p8_raw = vars[ENV_APNS_KEY_P8].as_str();
    let key_id = vars[ENV_APNS_KEY_ID].as_str();
    let team_id = vars[ENV_APNS_TEAM_ID].as_str();
    let bundle_id = vars[ENV_APNS_BUNDLE_ID].as_str();

    let pem = match resolve_p8_stored(p8_raw) {
        Ok(pem) => pem,
        Err(e) => {
            *ok = false;
            println!(
 "\u{2717} apns: NOSTOS_APNS_KEY_P8 {e} — point it at the .p8 path or paste the PEM, or run `nostos push init --apns ...`"
            );
            return;
        }
    };
    if let Err(e) = validate_p8_pem(&pem) {
        *ok = false;
        println!(
 "\u{2717} apns: {e} — re-download the .p8 (Keys section) from the Apple developer console and run `nostos push init --apns ...`"
        );
        return;
    }
    let jwt = match mint_apns_provider_jwt(&pem, key_id, team_id) {
        Ok(jwt) => jwt,
        Err(e) => {
            *ok = false;
            println!("\u{2717} apns: provider JWT mint failed: {e}");
            return;
        }
    };
    if let Err(e) = verify_apns_jwt_shape(&jwt, key_id, team_id) {
        *ok = false;
        println!("\u{2717} apns: provider JWT claim shape rejected: {e}");
        return;
    }
    let mut line = format!(
 "\u{2713} apns: p8 parses as P-256; ES256 provider JWT minted (kid={key_id}, iss={team_id}, iat fresh); topic {bundle_id}"
    );
    if probe {
        match tls_probe(APNS_PROBE_HOST).await {
            Ok(()) => {
                line.push_str("; probe: TLS handshake ok (no notification sent)");
            }
            Err(e) => {
                *ok = false;
                let _ = write!(
                    line,
                    "; probe: TLS handshake FAILED ({e}) — check egress/firewall to \
                     {APNS_PROBE_HOST}:{APNS_PROBE_PORT}"
                );
            }
        }
    }
    println!("{line}");
}

async fn check_fcm(credentials_json: &str, ok: &mut bool) {
    // A hand-edited path here is the one misconfiguration whose rail error
    // ("expected value at line 1 column 1") teaches nothing — name it.
    if !credentials_json.trim_start().starts_with('{') {
        *ok = false;
        println!(
            "✗ fcm: NOSTOS_FCM_CREDENTIALS_JSON is not JSON — the FCM rail parses this \
             var directly and does not read paths (unlike the APNs p8). Run `nostos push init \
             --fcm --fcm-credentials-json <path>` to inline it"
        );
        return;
    }
    let account = match parse_service_account(credentials_json) {
        Ok(account) => account,
        Err(e) => {
            *ok = false;
            println!(
 "\u{2717} fcm: {e} — re-download the JSON (Firebase console \u{2192} Project settings \u{2192} Service accounts) or run `nostos push init --fcm ...`"
            );
            return;
        }
    };
    match mint_fcm_access_token(&account).await {
        Ok(()) => println!(
            "\u{2713} fcm: OAuth2 access token obtained (jwt-bearer grant, scope {FCM_SCOPE})"
        ),
        Err(e) => {
            *ok = false;
            println!(
 "\u{2717} fcm: token mint failed: {e} — check the service account's key/permissions and egress to oauth2.googleapis.com"
            );
        }
    }
}

/// Offline Web Push shape check (the plan's task 3.2 wording): the key must
/// base64url-decode to exactly the 32-byte P-256 scalar, the subject must be
/// `mailto:`.
fn check_webpush_shape(key_b64: &str, subject: &str) -> Result<(), String> {
    let bytes = b64url_decode(key_b64).ok_or_else(|| "private key is not base64url".to_string())?;
    if bytes.len() != 32 {
        return Err(format!(
            "private key decodes to {} bytes, expected 32 (a P-256 scalar)",
            bytes.len()
        ));
    }
    if !subject.starts_with("mailto:") {
        return Err(format!("subject must start with mailto: (got {subject:?})"));
    }
    Ok(())
}

/// Mint the ES256 provider JWT exactly like `apns.rs` `provider_token`:
/// claims `{iss: team id, iat: now}` (no exp — Apple caps validity at 1h
/// from iat), header `ES256` + `kid` = key id.
fn mint_apns_provider_jwt(pem: &str, key_id: &str, team_id: &str) -> Result<String, String> {
    let encoding_key =
        EncodingKey::from_ec_pem(pem.as_bytes()).map_err(|e| format!("p8 key: {e}"))?;
    let claims = json!({ "iss": team_id, "iat": jsonwebtoken::get_current_timestamp() });
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id.to_string());
    encode(&header, &claims, &encoding_key).map_err(|e| format!("apns jwt sign: {e}"))
}

/// Claim-shape validation of a freshly minted provider JWT: three segments,
/// `alg=ES256`, `kid` = key id, `iss` = team id, `iat` within
/// `JWT_FRESHNESS_SECS` of now. Decode-only (no verify) — we minted and hold
/// the signing key; this pins the claims shape the rail relies on.
fn verify_apns_jwt_shape(jwt: &str, key_id: &str, team_id: &str) -> Result<(), String> {
    let segments: Vec<&str> = jwt.split('.').collect();
    if segments.len() != 3 {
        return Err(format!("expected 3 jwt segments, got {}", segments.len()));
    }
    let parse = |segment: &str| -> Result<Value, String> {
        let bytes = b64url_decode(segment).ok_or_else(|| "segment is not base64url".to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| format!("segment json: {e}"))
    };
    let header = parse(segments[0])?;
    let claims = parse(segments[1])?;
    if header.get("alg").and_then(Value::as_str) != Some("ES256") {
        return Err("alg is not ES256".to_string());
    }
    if header.get("kid").and_then(Value::as_str) != Some(key_id) {
        return Err("kid does not match the key id".to_string());
    }
    if claims.get("iss").and_then(Value::as_str) != Some(team_id) {
        return Err("iss does not match the team id".to_string());
    }
    let iat = claims.get("iat").and_then(Value::as_u64).unwrap_or(0);
    if jsonwebtoken::get_current_timestamp().abs_diff(iat) > JWT_FRESHNESS_SECS {
        return Err("iat is not fresh".to_string());
    }
    Ok(())
}

/// Exchange the service-account key for an OAuth2 access token exactly like
/// `fcm.rs` `access_token` (RS256 assertion, jwt-bearer grant, FCM scope).
/// Returns `Ok` on a usable token — the token itself is never surfaced.
async fn mint_fcm_access_token(account: &ServiceAccount) -> Result<(), String> {
    let encoding_key = EncodingKey::from_rsa_pem(account.private_key.as_bytes())
        .map_err(|e| format!("private_key: {e}"))?;
    let now = jsonwebtoken::get_current_timestamp();
    let claims = json!({
        "iss": account.client_email,
        "scope": FCM_SCOPE,
        "aud": FCM_TOKEN_URL,
        "iat": now,
        "exp": now + 3600,
    });
    let assertion = encode(&Header::new(Algorithm::RS256), &claims, &encoding_key)
        .map_err(|e| format!("assertion sign: {e}"))?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = http
        .post(FCM_TOKEN_URL)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("token endpoint unreachable: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() && body.get("access_token").is_some_and(Value::is_string) {
        Ok(())
    } else {
        Err(format!("token endpoint returned {status}"))
    }
}

/// Drive a real rustls handshake (webpki roots — the `pg.rs` `connect_tls`
/// recipe) against `host:443` and stop there: no HTTP request, no device
/// token, so no notification can leave. Runs on the blocking pool because
/// rustls' `complete_io` drives a std TcpStream.
async fn tls_probe(host: &str) -> Result<(), String> {
    let host = host.to_string();
    tokio::task::spawn_blocking(move || tls_handshake_probe(&host))
        .await
        .map_err(|e| format!("probe task: {e}"))
        .and_then(|result| result)
}

fn tls_handshake_probe(host: &str) -> Result<(), String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls config: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| format!("invalid probe host {host:?}"))?;
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)
        .map_err(|e| format!("tls client: {e}"))?;
    let mut sock = std::net::TcpStream::connect((host, APNS_PROBE_PORT))
        .map_err(|e| format!("tcp connect: {e}"))?;
    let timeout = Some(std::time::Duration::from_secs(10));
    let _ = sock.set_read_timeout(timeout);
    let _ = sock.set_write_timeout(timeout);
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// codecs — hand-rolled per this workspace's standing preference (jwks.rs,
// push/mod.rs test_support) over a base64 dependency for a handful of sites.
// ---------------------------------------------------------------------------

/// Base64url, no padding (RFC 8292 `k=` / VAPID key format).
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[(n >> 18 & 0x3F) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Base64url decode (no padding accepted).
fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            // `acc` keeps already-emitted high bits — mask them off.
            out.push(u8::try_from(acc >> bits & 0xFF).expect("masked byte"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey;
    use rand::thread_rng;

    // ---- test key minting (mirrors nostos-infra push test_support) ----

    /// Padded standard base64, wrapped at 64 columns for PEM bodies.
    fn b64_std_pad(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(ALPHABET[(n >> 18 & 0x3F) as usize] as char);
            out.push(ALPHABET[(n >> 12 & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(n >> 6 & 0x3F) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3F) as usize] as char);
            }
        }
        match out.len() % 4 {
            2 => out.push_str("=="),
            3 => out.push('='),
            _ => {}
        }
        out
    }

    fn pkcs8_pem_wrap(der: &[u8]) -> String {
        let b64 = b64_std_pad(der);
        let mut body = String::new();
        for chunk in b64.as_bytes().chunks(64) {
            body.push_str(std::str::from_utf8(chunk).expect("b64 is ascii"));
            body.push('\n');
        }
        format!("-----BEGIN PRIVATE KEY-----\n{body}-----END PRIVATE KEY-----\n")
    }

    fn test_p8_pem() -> String {
        let key = SigningKey::random(&mut thread_rng());
        pkcs8_pem_wrap(key.to_pkcs8_der().expect("pkcs8").as_bytes())
    }

    fn test_rsa_pem() -> String {
        let key = rsa::RsaPrivateKey::new(&mut thread_rng(), 2048).expect("rsa key");
        key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("pem")
            .to_string()
    }

    fn temp_env_path(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nostos-push-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(".env")
    }

    // ---- p8 validation ----

    #[test]
    fn p8_validation_accepts_minted_p256_and_rejects_bad_input() {
        let pem = test_p8_pem();
        assert!(validate_p8_pem(&pem).is_ok(), "minted p8 must validate");

        assert!(validate_p8_pem("not a pem at all").is_err());
        // Right marker, garbage body.
        assert!(
            validate_p8_pem("-----BEGIN PRIVATE KEY-----\nZm9v\n-----END PRIVATE KEY-----\n")
                .is_err()
        );
        // Valid PEM, wrong algorithm family (RSA, not EC P-256).
        assert!(validate_p8_pem(&test_rsa_pem()).is_err());
    }

    #[test]
    fn apns_ids_require_exactly_10_char_key_id() {
        assert!(validate_apns_ids("ABCDEFGHIJ", "TEAM456789", "run.nostos.app").is_ok());
        assert!(validate_apns_ids("SHORT", "TEAM456789", "run.nostos.app").is_err());
        assert!(validate_apns_ids("TOOLONGKEY12", "TEAM456789", "run.nostos.app").is_err());
        assert!(validate_apns_ids("ABCDEFGHIJ", " ", "run.nostos.app").is_err());
        assert!(validate_apns_ids("ABCDEFGHIJ", "TEAM456789", "").is_err());
    }

    #[test]
    fn inline_p8_stored_flattened_still_parses() {
        let pem = test_p8_pem();
        let flat = flatten_pem(&pem);
        assert!(
            !flat.contains('\n'),
            "stored value must be single-line for the line-based .env parser"
        );
        assert!(
            validate_p8_pem(&flat).is_ok(),
            "flattened PEM must stay parseable by the rails' from_ec_pem"
        );
        // The stored form resolves as itself (inline branch).
        assert_eq!(resolve_p8_stored(&flat).expect("resolve"), flat);
    }

    #[test]
    fn resolve_p8_reads_paths_and_accepts_inline() {
        let path = temp_env_path("resolve");
        let pem = test_p8_pem();
        let file = path.with_file_name("AuthKey_ABCDEFGHIJ.p8");
        std::fs::write(&file, &pem).expect("write key file");

        assert_eq!(
            resolve_p8_stored(file.to_str().expect("utf8 path")).expect("via path"),
            pem
        );
        assert_eq!(resolve_p8_stored(&pem).expect("inline"), pem);

        let err = resolve_p8_stored("/nonexistent/nope.p8").expect_err("missing path");
        assert!(
            !err.contains("PRIVATE"),
            "errors must never echo key material: {err}"
        );
    }

    // ---- service-account validation ----

    #[test]
    fn service_account_requires_all_three_fields() {
        let full = json!({
            "project_id": "nostos-prod",
            "private_key": test_rsa_pem(),
            "client_email": "push@nostos-prod.iam.gserviceaccount.com",
        })
        .to_string();
        assert!(parse_service_account(&full).is_ok());

        for field in ["client_email", "private_key", "project_id"] {
            let mut value: Value = serde_json::from_str(&full).expect("json");
            value.as_object_mut().expect("object").remove(field);
            let err = parse_service_account(&value.to_string())
                .expect_err("missing field must be rejected");
            assert!(err.contains(field), "error must name {field}: {err}");
        }

        // EC key where Google's RSA key belongs.
        let wrong_family = json!({
            "project_id": "nostos-prod",
            "private_key": test_p8_pem(),
            "client_email": "push@nostos-prod.iam.gserviceaccount.com",
        })
        .to_string();
        assert!(parse_service_account(&wrong_family).is_err());

        // PEM-shaped private_key with a garbage body.
        let garbage = json!({
            "project_id": "nostos-prod",
            "private_key": "-----BEGIN PRIVATE KEY-----\nZm9v\n-----END PRIVATE KEY-----\n",
            "client_email": "push@nostos-prod.iam.gserviceaccount.com",
        })
        .to_string();
        assert!(parse_service_account(&garbage).is_err());

        assert!(parse_service_account("not json").is_err());
    }

    // ---- .env writer ----

    #[test]
    fn env_writer_updates_in_place_and_respects_force() {
        let path = temp_env_path("writer");
        std::fs::write(
            &path,
            "# operator comment\nOTHER=keep-me\nNOSTOS_APNS_KEY_ID=OLDID12345\n",
        )
        .expect("seed env");

        // Without --force: non-blank value is skipped, file untouched.
        let existing = dotenv::read(&path);
        let mut writer = EnvWriter::new(&path, &existing, false);
        writer
            .set("apns", ENV_APNS_KEY_ID, "NEWID12345")
            .expect("set");
        writer.finish().expect("finish");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("NOSTOS_APNS_KEY_ID=OLDID12345"),
            "kept: {text}"
        );
        assert_eq!(
            text.matches("NOSTOS_APNS_KEY_ID").count(),
            1,
            "never duplicate a key"
        );
        assert!(text.contains("OTHER=keep-me"), "unrelated vars preserved");
        assert!(text.contains("# operator comment"), "comments preserved");

        // With --force: overwritten in place, appends only what is new.
        let existing = dotenv::read(&path);
        let mut writer = EnvWriter::new(&path, &existing, true);
        writer
            .set("apns", ENV_APNS_KEY_ID, "NEWID12345")
            .expect("set");
        writer
            .set("apns", ENV_APNS_TEAM_ID, "TEAM456789")
            .expect("set");
        writer.finish().expect("finish");
        let vars = dotenv::read(&path);
        assert_eq!(vars.get(ENV_APNS_KEY_ID).expect("id"), "NEWID12345");
        assert_eq!(vars.get(ENV_APNS_TEAM_ID).expect("team"), "TEAM456789");
        assert_eq!(vars.get("OTHER").expect("other"), "keep-me");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.matches("NOSTOS_APNS_KEY_ID").count(), 1);

        // A blank existing value is replaced even without --force.
        std::fs::write(&path, "NOSTOS_APNS_BUNDLE_ID=\n").expect("blank seed");
        let existing = dotenv::read(&path);
        let mut writer = EnvWriter::new(&path, &existing, false);
        writer
            .set("apns", ENV_APNS_BUNDLE_ID, "run.nostos.app")
            .expect("set");
        writer.finish().expect("finish");
        assert_eq!(
            dotenv::read(&path).get(ENV_APNS_BUNDLE_ID).expect("bundle"),
            "run.nostos.app"
        );

        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    #[test]
    fn webpush_skip_does_not_mint_or_note() {
        let path = temp_env_path("webpush-skip");
        // Existing non-blank keypair, no --force.
        std::fs::write(
            &path,
            "NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY=vSAlGTCz2wFl3hJUX1pUeiG_baioNvNcfV5m4Q-YZsE\n",
        )
        .expect("seed");
        let existing = dotenv::read(&path);
        let mut writer = EnvWriter::new(&path, &existing, false);
        let args = InitPushArgs {
            webpush: true,
            vapid_subject: Some("mailto:ops@example.com".into()),
            ..init_args_defaults()
        };
        init_webpush(&args, &mut writer).expect("init");
        // The subject is new, so it IS queued — only the key must be skipped.
        assert!(
            !writer
                .pending
                .iter()
                .any(|(_, k, _)| *k == ENV_WEBPUSH_VAPID_PRIVATE_KEY),
            "the existing key must not be queued for overwrite"
        );
        assert!(
            writer
                .pending
                .iter()
                .any(|(_, k, v)| *k == ENV_WEBPUSH_VAPID_SUBJECT && v == "mailto:ops@example.com"),
            "a new subject is still written alongside the kept key"
        );
        assert!(
            writer.notes.is_empty(),
            "no public-key note may print for a keypair that was not written"
        );

        // With --force the mint lands and the printed public key must be the
        // point derived from the private scalar actually stored.
        let mut writer = EnvWriter::new(&path, &existing, true);
        init_webpush(&args, &mut writer).expect("init force");
        let stored = writer
            .pending
            .iter()
            .find(|(_, k, _)| *k == ENV_WEBPUSH_VAPID_PRIVATE_KEY)
            .expect("private key queued")
            .2
            .clone();
        let scalar = b64url_decode(&stored).expect("scalar");
        let signing =
            SigningKey::from_bytes(p256::FieldBytes::from_slice(&scalar)).expect("valid scalar");
        let point = b64url(signing.verifying_key().to_encoded_point(false).as_bytes());
        assert!(
            writer.notes.iter().any(|n| n.contains(&point)),
            "the printed public key must match the stored private key"
        );

        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    /// The FCM rail's from_env parses the env var as JSON directly (no path
    /// resolution, unlike the APNs p8) — so a path passed to init must be
    /// READ and stored as the minified JSON itself, or the embedded server
    /// could not boot on the value init wrote.
    #[test]
    fn fcm_path_input_is_stored_as_inline_json() {
        let dir = temp_env_path("fcm-inline")
            .parent()
            .expect("parent")
            .to_path_buf();
        let creds_path = dir.join("fcm.json");
        std::fs::write(
            &creds_path,
            json!({
                "type": "service_account",
                "project_id": "nostos-prod",
                "private_key": test_rsa_pem(),
                "client_email": "push@nostos-prod.iam.gserviceaccount.com",
                "extra_field": "preserved",
            })
            .to_string(),
        )
        .expect("write creds");

        let env_path = dir.join(".env");
        let existing = dotenv::read(&env_path);
        let mut writer = EnvWriter::new(&env_path, &existing, false);
        let args = InitPushArgs {
            fcm: true,
            fcm_credentials_json: Some(creds_path.to_str().expect("utf8 path").to_string()),
            ..init_args_defaults()
        };
        init_fcm(&args, &mut writer).expect("init fcm from path");

        let stored = writer
            .pending
            .iter()
            .find(|(_, k, _)| *k == ENV_FCM_CREDENTIALS_JSON)
            .expect("credentials queued")
            .2
            .clone();
        assert!(
            stored.starts_with('{'),
            "stored value must be the JSON itself"
        );
        assert!(
            stored.lines().count() <= 1,
            "line-based .env needs a single-line value"
        );
        // Round-trip: the rail's own parse must accept what init stored.
        assert!(parse_service_account(&stored).is_ok());
        assert!(
            stored.contains("extra_field"),
            "unknown fields are preserved"
        );
        assert!(
            writer.notes.iter().any(|n| n.contains("inlined")),
            "the operator is told the JSON was inlined from the path"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn init_args_defaults() -> InitPushArgs {
        InitPushArgs {
            apns: false,
            fcm: false,
            webpush: false,
            env_file: ".env".into(),
            force: false,
            apns_key_p8: None,
            apns_key_id: None,
            apns_team_id: None,
            apns_bundle_id: None,
            fcm_credentials_json: None,
            vapid_subject: None,
        }
    }

    #[test]
    fn env_writer_refuses_multiline_values() {
        let path = temp_env_path("multiline");
        let existing = dotenv::read(&path);
        let mut writer = EnvWriter::new(&path, &existing, false);
        let err = writer
            .set("apns", ENV_APNS_KEY_P8, "line one\nline two")
            .expect_err("must refuse");
        assert!(err.to_string().contains("single-line"));
        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    // ---- VAPID mint ----

    #[test]
    fn vapid_mint_is_32_byte_scalar_with_matching_public_point() {
        let (private_b64, public_b64) = mint_vapid_keypair();

        let scalar = b64url_decode(&private_b64).expect("base64url scalar");
        assert_eq!(scalar.len(), 32, "P-256 scalar is exactly 32 bytes");

        // Deriving the public point from the stored scalar must reproduce
        // the printed public key — what the client subscribes with.
        let signing = SigningKey::from_bytes(p256::FieldBytes::from_slice(&scalar))
            .expect("scalar is a valid P-256 key");
        let point = signing.verifying_key().to_encoded_point(false);
        assert_eq!(b64url(point.as_bytes()), public_b64);

        let public = b64url_decode(&public_b64).expect("base64url point");
        assert_eq!(public.len(), 65, "uncompressed P-256 point is 65 bytes");
        assert_eq!(public[0], 0x04, "uncompressed point marker");

        assert!(
            !private_b64.contains('='),
            "no padding in the scalar encoding"
        );
        assert!(
            !public_b64.contains('='),
            "no padding in the point encoding"
        );
    }

    #[test]
    fn webpush_shape_check_validates_scalar_and_subject() {
        let (private_b64, _) = mint_vapid_keypair();
        assert!(check_webpush_shape(&private_b64, "mailto:ops@example.com").is_ok());

        let short = b64url(&[0u8; 31]);
        assert!(check_webpush_shape(&short, "mailto:ops@example.com").is_err());
        assert!(check_webpush_shape(&private_b64, "https://nostos.run").is_err());
        assert!(check_webpush_shape("!!not-base64url!!", "mailto:ops@example.com").is_err());
    }

    // ---- APNs provider JWT ----

    #[test]
    fn apns_provider_jwt_shape_matches_apns_rail() {
        let pem = test_p8_pem();
        let jwt = mint_apns_provider_jwt(&pem, "ABCDEFGHIJ", "TEAM456789").expect("mint");
        assert!(verify_apns_jwt_shape(&jwt, "ABCDEFGHIJ", "TEAM456789").is_ok());
        assert!(verify_apns_jwt_shape(&jwt, "WRONGKID12", "TEAM456789").is_err());
        assert!(verify_apns_jwt_shape(&jwt, "ABCDEFGHIJ", "WRONGTEAM").is_err());
        assert!(verify_apns_jwt_shape("not-a-jwt", "ABCDEFGHIJ", "TEAM456789").is_err());
    }

    // ---- contract ----

    #[test]
    fn env_var_constants_match_infra_push_contract() {
        // Source of truth: crates/nostos-infra/src/push/mod.rs:22-28 (the
        // from_env() parsers read exactly these names).
        assert_eq!(ENV_FCM_CREDENTIALS_JSON, "NOSTOS_FCM_CREDENTIALS_JSON");
        assert_eq!(ENV_APNS_KEY_P8, "NOSTOS_APNS_KEY_P8");
        assert_eq!(ENV_APNS_KEY_ID, "NOSTOS_APNS_KEY_ID");
        assert_eq!(ENV_APNS_TEAM_ID, "NOSTOS_APNS_TEAM_ID");
        assert_eq!(ENV_APNS_BUNDLE_ID, "NOSTOS_APNS_BUNDLE_ID");
        assert_eq!(ENV_APNS_SANDBOX, "NOSTOS_APNS_SANDBOX");
        assert_eq!(
            ENV_WEBPUSH_VAPID_PRIVATE_KEY,
            "NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY"
        );
        assert_eq!(ENV_WEBPUSH_VAPID_SUBJECT, "NOSTOS_WEBPUSH_VAPID_SUBJECT");
    }

    #[test]
    fn rail_state_distinguishes_off_partial_on() {
        let mut vars = BTreeMap::new();
        assert!(matches!(
            rail_state(&vars, &[ENV_APNS_KEY_P8, ENV_APNS_KEY_ID]),
            RailState::Off
        ));

        vars.insert(
            ENV_APNS_KEY_P8.to_string(),
            "-----BEGIN PRIVATE KEY-----x-----END PRIVATE KEY-----".to_string(),
        );
        match rail_state(&vars, &[ENV_APNS_KEY_P8, ENV_APNS_KEY_ID]) {
            RailState::Partial(missing) => assert_eq!(missing, vec![ENV_APNS_KEY_ID]),
            other => panic!("expected Partial, got {other:?}"),
        }

        vars.insert(ENV_APNS_KEY_ID.to_string(), "ABCDEFGHIJ".to_string());
        // A blank value counts as missing (mirrors env_nonempty).
        vars.insert(ENV_APNS_TEAM_ID.to_string(), " ".to_string());
        match rail_state(&vars, &[ENV_APNS_KEY_P8, ENV_APNS_KEY_ID, ENV_APNS_TEAM_ID]) {
            RailState::Partial(missing) => assert_eq!(missing, vec![ENV_APNS_TEAM_ID]),
            other => panic!("expected Partial, got {other:?}"),
        }
        vars.insert(ENV_APNS_TEAM_ID.to_string(), "TEAM456789".to_string());
        assert!(matches!(
            rail_state(&vars, &[ENV_APNS_KEY_P8, ENV_APNS_KEY_ID, ENV_APNS_TEAM_ID]),
            RailState::On
        ));
    }
}
