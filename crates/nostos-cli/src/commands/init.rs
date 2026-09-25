//! `nostos init` — connect, verify, create/update the publication, write
//! `nostos.toml` + `.env`. Idempotent: re-running updates the config and
//! reconciles the publication's table set without erroring on what already
//! exists.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;

use crate::config::{
    config_path, DbSection, NostosConfig, ServerSection, SupabaseSection, SyncSection,
};
use crate::pg::{PgControl, PublicationAction};
use crate::{dotenv, prompt};

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Direct Postgres connection string. Prompted for if omitted.
    #[arg(long)]
    pub db_url: Option<String>,
    /// Comma-separated tables to sync — also the publication's scope.
    /// Prompted for if omitted.
    #[arg(long)]
    pub tables: Option<String>,
    /// Comma-separated write-back allowlist (ADR-0013); must be a subset of
    /// `--tables`. Empty (default) = read-only sync.
    #[arg(long)]
    pub write_tables: Option<String>,
    /// Tenant column enforced on every predicate once auth is
    /// `supabase-jwt` (ADR-0011) — matches the server's own default.
    #[arg(long, default_value = "org_id")]
    pub tenant_column: String,
    /// Supabase project URL, e.g. `https://xyz.supabase.co`. Optional —
    /// derives the JWKS URL for `doctor` and future auth wiring.
    #[arg(long)]
    pub supabase_url: Option<String>,
    /// Publication name.
    #[arg(long, default_value = "nostos_pub")]
    pub publication: String,
    /// Replication slot name. `init` never creates the slot — nostos-server
    /// creates it lazily on first `nostos dev` (see `crate::pg` module docs)
    /// — this only records the name it will use.
    #[arg(long, default_value = "nostos_slot")]
    pub slot: String,
    /// Server bind address written to `nostos.toml` (`nostos dev` uses it).
    #[arg(long, default_value = "0.0.0.0:8800")]
    pub bind: String,
}

pub async fn run(args: InitArgs, cwd: &Path) -> Result<()> {
    let db_url = match args.db_url {
        Some(u) => u,
        None => prompt::prompt_nonempty("Direct Postgres connection string: ")
            .context("reading db-url from stdin")?,
    };
    let tables = parse_csv(&match args.tables {
        Some(t) => t,
        None => prompt::prompt_nonempty("Tables to sync (comma-separated): ")
            .context("reading tables from stdin")?,
    });
    anyhow::ensure!(!tables.is_empty(), "at least one table is required");

    let write_tables = parse_csv(&args.write_tables.unwrap_or_default());
    for wt in &write_tables {
        anyhow::ensure!(
            tables.contains(wt),
            "--write-tables entry {wt:?} must also appear in --tables"
        );
    }

    println!("Connecting to Postgres...");
    let pg = PgControl::connect(&db_url)
        .await
        .context("connecting to Postgres")?;

    let wal_level = pg.wal_level().await?;
    anyhow::ensure!(
        wal_level == "logical",
        "wal_level is {wal_level:?}, need \"logical\". On Supabase: Database → Replication → \
         set WAL level to Logical (this restarts the database). Self-hosted Postgres: \
         ALTER SYSTEM SET wal_level = logical; then restart the server."
    );
    println!("\u{2713} wal_level = logical");

    let action = pg.ensure_publication(&args.publication, &tables).await?;
    match action {
        PublicationAction::Created => {
            println!(
                "\u{2713} created publication {:?} for {tables:?}",
                args.publication
            );
        }
        PublicationAction::TablesUpdated => {
            println!(
                "\u{2713} updated publication {:?} to {tables:?}",
                args.publication
            );
        }
        PublicationAction::Unchanged => {
            println!(
                "\u{2713} publication {:?} already covers {tables:?}",
                args.publication
            );
        }
    }

    let headroom = pg.slot_headroom().await?;
    println!(
        "  replication slots: {}/{} used",
        headroom.used, headroom.max
    );
    if headroom.headroom() <= 1 {
        println!(
            "\u{26a0} only {} replication slot(s) free after this. Small Supabase computes cap \
             max_replication_slots at 5, shared with Realtime — nostos-server needs one more on \
             the first `nostos dev`. Consider a compute add-on if you're near the ceiling.",
            headroom.headroom()
        );
    }
    println!(
        "  guidance: nostos-server evicts a client that lags more than NOSTOS_SLOT_MAX_LAG \
         (default 1 GiB) so a stalled client can't grow WAL unbounded; that only holds while \
         the server is running. Set Postgres's max_slot_wal_keep_size in production so an \
         abandoned slot can't fill the primary's disk (ADR-0043). `nostos init` reports this \
         but does not change server settings; `nostos doctor` checks it."
    );

    let supabase = args.supabase_url.map(|url| {
        let url = url.trim_end_matches('/').to_string();
        let jwks_url = format!("{url}/auth/v1/.well-known/jwks.json");
        println!("  JWKS URL (for future RS256/ES256 auth, W2): {jwks_url}");
        SupabaseSection { url, jwks_url }
    });

    let cfg = NostosConfig {
        sync: SyncSection {
            tables,
            write_tables,
            tenant_column: args.tenant_column,
        },
        db: DbSection {
            url_env: "NOSTOS_PG_URL".to_string(),
            publication: args.publication,
            slot: args.slot,
        },
        supabase,
        server: ServerSection {
            bind: args.bind,
            ..ServerSection::default()
        },
    };
    let cfg_path = config_path(cwd);
    cfg.save(&cfg_path)?;
    println!("\u{2713} wrote {}", cfg_path.display());

    let env_path = cwd.join(".env");
    dotenv::set(&env_path, "NOSTOS_PG_URL", &db_url)?;
    println!(
        "\u{2713} wrote NOSTOS_PG_URL to {} (contains a credential — never commit it)",
        env_path.display()
    );
    advise_gitignore(cwd);

    println!("\nNext: `nostos dev` to start the sync server.");
    Ok(())
}

fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Prints advice only — never edits `.gitignore` on the user's behalf.
fn advise_gitignore(cwd: &Path) {
    let text = std::fs::read_to_string(cwd.join(".gitignore")).unwrap_or_default();
    if !text.lines().any(|l| l.trim() == ".env") {
        println!("  note: add `.env` to .gitignore — it is not modified automatically");
    }
}
