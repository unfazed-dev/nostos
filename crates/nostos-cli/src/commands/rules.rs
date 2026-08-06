//! `nostos rules` — generate, edit, and validate `nostos_rules.toml` (ADR-0031).
//!
//! `init` is the only fully-implemented subcommand (Task 15 of the
//! sync-streams-suite). `edit` (interactive per-table toggles) is a later
//! task — `.superpowers/sdd/nostos-sync-streams-suite/progress.md` flags it
//! "Task 15+" and its args shape isn't specified anywhere yet — so it's
//! stubbed with a clear error rather than guessed at.
//!
//! ponytail: `edit` errors instead of prompting; hand-edit the `[tables.*]`
//! section directly (`rules_file::save` preserves it) until that task lands.

use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
use nostos_infra::rules_file;

use crate::config::{NostosConfig, DEFAULT_FILE_NAME};
use crate::dotenv;
use crate::pg::PgControl;

#[derive(Debug, Args)]
pub struct RulesArgs {
    #[command(subcommand)]
    pub command: RulesCommand,
}

#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// Introspect the database and write nostos_rules.toml (one entry per table).
    Init(InitRulesArgs),
    /// Interactive per-table sync toggles (toggles mode only).
    Edit(EditRulesArgs),
    /// Validate nostos_rules.toml and print the active mode + checksum.
    Check,
}

#[derive(Debug, Args)]
pub struct InitRulesArgs {
    /// Overwrite an existing nostos_rules.toml.
    #[arg(long)]
    pub force: bool,
    /// sync_mode to write. Default: toggles.
    #[arg(long, default_value = "toggles")]
    pub mode: String,
    /// Default `sync` value for every discovered table. Default: false
    /// (opt-in beats accidental full-fleet exposure).
    #[arg(long)]
    pub sync_all: bool,
}

/// No fields yet — `nostos rules edit` isn't implemented (see module docs).
#[derive(Debug, Args)]
pub struct EditRulesArgs {}

/// Pure: build the initial ruleset from introspected table names.
/// Empty input yields a valid ruleset with zero table entries — never an error.
#[must_use]
pub fn rules_from_tables(tables: &[String], mode: SyncMode, sync_default: bool) -> SyncRules {
    let mut names = tables.to_vec();
    names.sort();
    let tables = names
        .into_iter()
        .map(|table| TableRule {
            table,
            sync: sync_default,
            scope: None,
        })
        .collect();
    SyncRules {
        version: RULES_VERSION,
        mode,
        tables,
        hand: Vec::new(),
    }
}

pub async fn run(args: RulesArgs, cwd: &Path) -> Result<()> {
    match args.command {
        RulesCommand::Init(init_args) => run_init(init_args, cwd).await,
        RulesCommand::Edit(_) => anyhow::bail!(
            "`nostos rules edit` is not implemented yet — hand-edit the [tables.*] section of \
             nostos_rules.toml directly, or use `nostos rules check` to validate it."
        ),
        RulesCommand::Check => run_check(cwd),
    }
}

async fn run_init(args: InitRulesArgs, cwd: &Path) -> Result<()> {
    let mode = SyncMode::parse(&args.mode).ok_or_else(|| {
        anyhow::anyhow!("unknown --mode {:?} (expected all|toggles|hand)", args.mode)
    })?;

    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);
    if rules_path.exists() && !args.force {
        anyhow::bail!(
            "{} already exists — refusing to overwrite. Use `nostos rules edit` to change it, \
             or pass --force to regenerate it from the database.",
            rules_path.display()
        );
    }

    let cfg = NostosConfig::load(&cwd.join(DEFAULT_FILE_NAME))?;
    let env_path = cwd.join(".env");
    let pg_url = dotenv::read(&env_path)
        .get(&cfg.db.url_env)
        .cloned()
        .with_context(|| {
            format!(
                "{} not set in {} — run `nostos init` first",
                cfg.db.url_env,
                env_path.display()
            )
        })?;

    let pg = PgControl::connect(&pg_url)
        .await
        .context("connecting to Postgres")?;
    let tables = pg
        .publication_tables(&cfg.db.publication)
        .await
        .with_context(|| format!("reading publication {:?}", cfg.db.publication))?
        .unwrap_or_default();

    let rules = rules_from_tables(&tables, mode, args.sync_all);
    rules_file::save(&rules_path, &rules)
        .with_context(|| format!("writing {}", rules_path.display()))?;

    if tables.is_empty() {
        append_template_comment(&rules_path, &cfg.db.publication)?;
        println!(
            "No tables found in publication `{}`. Wrote a template `{}` — re-run \
             `nostos rules init --force` after creating tables.",
            cfg.db.publication,
            rules_file::RULES_FILE_NAME
        );
        return Ok(());
    }

    println!(
        "\u{2713} wrote {} ({} table{})",
        rules_path.display(),
        tables.len(),
        if tables.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Append a commented example entry so an empty-DB `nostos_rules.toml` still
/// shows the `[tables.*]` shape, since there are no real entries to show it.
fn append_template_comment(rules_path: &Path, publication: &str) -> Result<()> {
    let comment = format!(
        "\n# No tables found in publication {publication:?} yet. Once you create tables and \
         re-run `nostos rules init --force`, entries will look like:\n\
         # [tables.example]\n\
         # sync = true\n\
         # scope = \"owner_id = claims.sub\"  # optional; omit to sync the whole table\n"
    );
    let mut text = std::fs::read_to_string(rules_path)
        .with_context(|| format!("reading {}", rules_path.display()))?;
    text.push_str(&comment);
    std::fs::write(rules_path, text).with_context(|| format!("writing {}", rules_path.display()))
}

fn run_check(cwd: &Path) -> Result<()> {
    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);
    match rules_file::load(&rules_path)? {
        None => println!(
            "no {} at {} — sync_mode defaults to `all` (zero-config).",
            rules_file::RULES_FILE_NAME,
            rules_path.display()
        ),
        Some(rules) => println!(
            "{}: sync_mode = {}, checksum = {:#x}",
            rules_path.display(),
            rules.mode.as_str(),
            rules.checksum()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_from_tables_defaults_to_sync_false() {
        let rules = rules_from_tables(
            &["tasks".to_string(), "notes".to_string()],
            SyncMode::Toggles,
            false,
        );
        assert_eq!(rules.tables.len(), 2);
        assert!(rules.tables.iter().all(|t| !t.sync));
    }

    #[test]
    fn rules_from_tables_empty_is_valid() {
        let rules = rules_from_tables(&[], SyncMode::Toggles, false);
        assert!(rules.tables.is_empty());
        assert!(rules.validate().is_ok());
    }

    #[test]
    fn rules_from_tables_respects_sync_all() {
        let rules = rules_from_tables(&["tasks".to_string()], SyncMode::Toggles, true);
        assert!(rules.tables.iter().all(|t| t.sync));
    }
}
