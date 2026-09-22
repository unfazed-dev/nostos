//! `nostos doctor` — read-only health checks: connectivity, `wal_level`,
//! publication, slot headroom, replication lag, `max_slot_wal_keep_size`
//! (ADR-0043), JWKS reachability. Never creates or alters anything (that's
//! `init`'s job).
//!
//! `--mode direct` checks a different machine entirely: there is no slot and no
//! publication, so it verifies the generated schema instead — see
//! [`crate::direct::inspect`].

use std::path::Path;

use anyhow::{Context, Result};
use nostos_infra::rules_file::{self, RULES_FILE_NAME};
use clap::Args;

use crate::direct::{self, Verdict};

use crate::config::{NostosConfig, DEFAULT_FILE_NAME};
use crate::dotenv;
use crate::pg::PgControl;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// `server` (slot + publication health) or `direct` (the generated schema,
    /// grants, policies and log growth).
    #[arg(long, default_value = "server")]
    pub mode: String,
}

/// Run `nostos doctor`.
///
/// # Errors
/// [`anyhow::Error`] if any blocking check fails, or the mode is unknown.
pub async fn run(args: DoctorArgs, cwd: &Path) -> Result<()> {
    match args.mode.as_str() {
        "server" => run_server(cwd).await,
        "direct" => run_direct(cwd).await,
        other => anyhow::bail!("unknown --mode `{other}` (expected `server` or `direct`)"),
    }
}

/// Direct mode has no server to be healthy: what can be wrong is the SQL in
/// the database. Every check is a `select`, so this is safe against production.
async fn run_direct(cwd: &Path) -> Result<()> {
    let rules_path = cwd.join(RULES_FILE_NAME);
    let rules = rules_file::load(&rules_path)?.with_context(|| {
        format!(
            "no {RULES_FILE_NAME} at {} \u{2014} doctor checks one trigger per synced table",
            rules_path.display()
        )
    })?;
    // The `--public` set is not persisted, so an unscoped table would be
    // refused here. Treat every unscoped table as public for the purposes of
    // "does it have a trigger" — the scope itself is checked by the policy.
    let public: Vec<String> = rules
        .tables
        .iter()
        .filter(|t| t.sync && t.scope.as_deref().unwrap_or("").trim().is_empty())
        .map(|t| t.table.clone())
        .chain(
            rules
                .hand
                .iter()
                .filter(|r| r.scope.as_deref().unwrap_or("").trim().is_empty())
                .map(|r| r.table.clone()),
        )
        .collect();
    let tables = direct::plan(&rules, &public)?;

    let env_path = cwd.join(".env");
    let vars = dotenv::read(&env_path);
    let pg_url = ["DATABASE_URL", "NOSTOS_PG_URL", "SUPABASE_DB_URL"]
        .iter()
        .find_map(|k| vars.get(*k).cloned().or_else(|| std::env::var(k).ok()))
        .with_context(|| {
            format!(
                "no DATABASE_URL in {} or the environment \u{2014} direct mode checks the \
                 database directly, so doctor needs a connection string (the session pooler \
                 URL is fine; this never opens a replication slot)",
                env_path.display()
            )
        })?;

    let pg = PgControl::connect(&pg_url).await?;
    let checks = direct::inspect(pg.client(), &tables).await?;
    let mut all_ok = true;
    for check in &checks {
        println!("{} {}", check.glyph(), check.label);
        if check.verdict == Verdict::Fail {
            all_ok = false;
        }
    }
    print_summary(all_ok);
    if all_ok {
        Ok(())
    } else {
        anyhow::bail!("doctor found blocking issues")
    }
}

async fn run_server(cwd: &Path) -> Result<()> {
    let cfg = NostosConfig::load(&cwd.join(DEFAULT_FILE_NAME))?;
    let env_path = cwd.join(".env");
    let dotenv_vars = dotenv::read(&env_path);

    let mut all_ok = true;

    let Some(pg_url) = dotenv_vars.get(&cfg.db.url_env).cloned() else {
        report(
            &mut all_ok,
            false,
            &format!("{} not set in {}", cfg.db.url_env, env_path.display()),
        );
        print_summary(all_ok);
        anyhow::bail!("doctor found blocking issues");
    };

    let pg = match PgControl::connect(&pg_url).await {
        Ok(pg) => {
            report(&mut all_ok, true, "Postgres reachable");
            pg
        }
        Err(e) => {
            report(&mut all_ok, false, &format!("Postgres unreachable: {e:#}"));
            if let Some(hint) = ipv6_only_hint(&pg_url).await {
                println!("    {hint}");
            }
            print_summary(all_ok);
            anyhow::bail!("doctor found blocking issues");
        }
    };

    match pg.wal_level().await {
        Ok(level) if level == "logical" => {
            report(&mut all_ok, true, "wal_level = logical");
        }
        Ok(level) => report(
            &mut all_ok,
            false,
            &format!("wal_level = {level} (need logical)"),
        ),
        Err(e) => report(
            &mut all_ok,
            false,
            &format!("could not read wal_level: {e:#}"),
        ),
    }

    match pg.publication_tables(&cfg.db.publication).await {
        Ok(Some(tables)) => report(
            &mut all_ok,
            true,
            &format!(
                "publication {:?} exists (tables: {tables:?})",
                cfg.db.publication
            ),
        ),
        Ok(None) => report(
            &mut all_ok,
            false,
            &format!(
                "publication {:?} does not exist — run `nostos init`",
                cfg.db.publication
            ),
        ),
        Err(e) => report(
            &mut all_ok,
            false,
            &format!("could not read publication: {e:#}"),
        ),
    }

    match pg.slot_headroom().await {
        Ok(h) => report(
            &mut all_ok,
            h.headroom() > 1,
            &format!(
                "replication slots: {}/{} used (headroom {})",
                h.used,
                h.max,
                h.headroom()
            ),
        ),
        Err(e) => report(
            &mut all_ok,
            false,
            &format!("could not read slot headroom: {e:#}"),
        ),
    }

    match pg.slot_status(&cfg.db.slot).await {
        Ok(s) if s.exists => {
            let lag = s.lag_bytes.unwrap_or_default();
            let lsn = s.confirmed_flush_lsn.as_deref().unwrap_or("?");
            report(
                &mut all_ok,
                true,
                &format!(
                    "slot {:?}: confirmed_flush_lsn={lsn} lag={lag}B",
                    cfg.db.slot
                ),
            );
        }
        Ok(_) => report(
            &mut all_ok,
            false,
            &format!(
                "slot {:?} does not exist yet — run `nostos dev` once to create it",
                cfg.db.slot
            ),
        ),
        Err(e) => report(
            &mut all_ok,
            false,
            &format!("could not read slot status: {e:#}"),
        ),
    }

    // ADR-0043: nostos-server's NOSTOS_SLOT_MAX_LAG eviction only protects the
    // primary while the server is running. A slot left behind by a server
    // that is gone (crash, decommission, renamed slot) pins WAL forever unless
    // Postgres itself caps it. Advisory, not blocking: the dev compose
    // Postgres ships unbounded and that is fine on a laptop.
    match pg.max_slot_wal_keep_size().await {
        Ok(v) if v.trim() == "-1" => {
            advise(
                "max_slot_wal_keep_size = -1 (unbounded): an abandoned replication slot can fill \
                 the primary's disk. Recommended for production: \
                 `ALTER SYSTEM SET max_slot_wal_keep_size = '2GB'; SELECT pg_reload_conf();` \
                 or set NOSTOS_PG_SLOT_WAL_KEEP_SIZE (MB) on nostos-server (ADR-0043).",
            );
        }
        Ok(v) => report(
            &mut all_ok,
            true,
            &format!("max_slot_wal_keep_size = {v} (abandoned-slot WAL is bounded)"),
        ),
        Err(e) => advise(&format!("could not read max_slot_wal_keep_size: {e:#}")),
    }

    if let Some(supabase) = &cfg.supabase {
        match reqwest::get(&supabase.jwks_url).await {
            Ok(resp) if resp.status().is_success() => {
                report(
                    &mut all_ok,
                    true,
                    &format!("JWKS reachable ({})", supabase.jwks_url),
                );
            }
            Ok(resp) => report(
                &mut all_ok,
                false,
                &format!("JWKS returned {} ({})", resp.status(), supabase.jwks_url),
            ),
            Err(e) => report(&mut all_ok, false, &format!("JWKS unreachable: {e}")),
        }
    }

    print_summary(all_ok);
    if all_ok {
        Ok(())
    } else {
        anyhow::bail!("doctor found blocking issues")
    }
}

fn report(all_ok: &mut bool, ok: bool, label: &str) {
    println!("{} {label}", if ok { "\u{2713}" } else { "\u{2717}" });
    if !ok {
        *all_ok = false;
    }
}

/// A recommendation that does not fail `doctor` — printed with ⚠ and never
/// flips `all_ok`.
fn advise(label: &str) {
    println!("\u{26a0} {label}");
}

fn print_summary(all_ok: bool) {
    println!();
    println!(
        "{}",
        if all_ok {
            "all checks passed"
        } else {
            "one or more checks failed — see \u{2717} above"
        }
    );
}

/// When the DB host resolves to ONLY IPv6 addresses (Supabase free-tier
/// direct connections are IPv6-only), a failed connect is almost always
/// missing IPv6 egress on the local network — which surfaces as an opaque
/// "No route to host". Name the real problem and the two ways out.
/// Best-effort: any resolution error returns `None` (the original connect
/// error already printed).
async fn ipv6_only_hint(pg_url: &str) -> Option<String> {
    let host = pg_url.parse::<reqwest::Url>().ok()?.host_str()?.to_string();
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 5432))
        .await
        .ok()?
        .collect();
    if !addrs.is_empty() && addrs.iter().all(std::net::SocketAddr::is_ipv6) {
        Some(format!(
            "hint: {host} is IPv6-only (no A records). If your network lacks \
             working IPv6 egress this fails as \"no route to host\". Fixes: \
             use a network with IPv6, or enable the Supabase IPv4 add-on \
             (paid) for the direct connection. Poolers don't carry logical \
             replication."
        ))
    } else {
        None
    }
}
