//! `nostos doctor` — read-only health checks: connectivity, `wal_level`,
//! publication, slot headroom, replication lag, JWKS reachability. Never
//! creates or alters anything (that's `init`'s job).

use std::path::Path;

use anyhow::Result;

use crate::config::{NostosConfig, DEFAULT_FILE_NAME};
use crate::dotenv;
use crate::pg::PgControl;

pub async fn run(cwd: &Path) -> Result<()> {
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
