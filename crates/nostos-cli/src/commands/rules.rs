//! `nostos rules` — generate, edit, and validate `nostos_rules.toml` (ADR-0031).
//!
//! `edit` is a plain-stdout line editor (no raw-mode terminal): each line is
//! parsed into an [`EditCommand`], applied to the in-memory [`SyncRules`] by
//! the pure [`apply_edit`], and only written to disk on `w`. `--mode` skips
//! the loop entirely and reuses `rules_file::set_mode` — the same primitive
//! `init`'s sibling infra module already proves preserves both sections.

use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use nostos_domain::{ScopeExpr, SyncMode, SyncRules, TableRule, RULES_VERSION};
use nostos_infra::rules_file;

use crate::config::{NostosConfig, DEFAULT_FILE_NAME};
use crate::dotenv;
use crate::pg::PgControl;
use crate::prompt;

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

#[derive(Debug, Args)]
pub struct EditRulesArgs {
    /// Switch sync_mode without entering the toggle loop.
    #[arg(long)]
    pub mode: Option<String>,
}

/// One editor command, parsed from a line of stdin. Pure and unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCommand {
    Toggle(usize),
    Scope { index: usize, scope: Option<String> },
    Mode(SyncMode),
    Save,
    Quit,
    Help,
    Unknown(String),
}

const HELP_TEXT: &str =
    "commands: <n> toggle · s <n> <scope> · mode <all|toggles|hand> · w save · \
                          q quit · ? help  (s <n> with no scope clears it)";

/// Switching back to `toggles` reactivates whatever `[tables.*]` is already
/// on disk — it does not re-query Postgres, so a table added/dropped since
/// the last `init` won't appear/disappear here. Named per team-lead ruling
/// (2026-08-06): no DB access in `edit`, discoverability hint only.
const TOGGLES_REFRESH_HINT: &str =
    " (reactivated the existing [tables.*] section — run `nostos rules init --force` to \
       re-detect tables from the publication if the schema has drifted)";

/// Parse one line of the editor's stdin protocol. Never errors — an
/// unrecognized line becomes `Unknown(line)` for the caller to report.
#[must_use]
pub fn parse_edit_command(line: &str) -> EditCommand {
    let line = line.trim();
    let mut parts = line.split_whitespace();
    let head = parts.next();
    let rest: Vec<&str> = parts.collect();

    match head {
        None => EditCommand::Unknown(String::new()),
        Some("q") if rest.is_empty() => EditCommand::Quit,
        Some("w") if rest.is_empty() => EditCommand::Save,
        Some("?") if rest.is_empty() => EditCommand::Help,
        Some("s") if !rest.is_empty() => match rest[0].parse::<usize>() {
            Ok(index) => {
                let scope_words = &rest[1..];
                let scope = (!scope_words.is_empty()).then(|| scope_words.join(" "));
                EditCommand::Scope { index, scope }
            }
            Err(_) => EditCommand::Unknown(line.to_string()),
        },
        Some("mode") if rest.len() == 1 => match SyncMode::parse(rest[0]) {
            Some(mode) => EditCommand::Mode(mode),
            None => EditCommand::Unknown(line.to_string()),
        },
        Some(tok) if rest.is_empty() => match tok.parse::<usize>() {
            Ok(index) => EditCommand::Toggle(index),
            Err(_) => EditCommand::Unknown(line.to_string()),
        },
        _ => EditCommand::Unknown(line.to_string()),
    }
}

/// Apply one command to the working ruleset. Returns a user-facing message
/// on success, or an error message on rejection. No disk I/O — the caller
/// (the interactive loop) writes the file after a successful `Save`.
pub fn apply_edit(rules: &mut SyncRules, cmd: &EditCommand) -> Result<String, String> {
    match cmd {
        EditCommand::Toggle(index) => {
            require_toggles_mode(rules)?;
            let table = table_at(&mut rules.tables, *index)?;
            table.sync = !table.sync;
            Ok(format!("[{index}] {} sync = {}", table.table, table.sync))
        }
        EditCommand::Scope { index, scope } => {
            require_toggles_mode(rules)?;
            if let Some(text) = scope {
                ScopeExpr::parse(text).map_err(|e| e.to_string())?;
            }
            let table = table_at(&mut rules.tables, *index)?;
            table.scope.clone_from(scope);
            Ok(match scope {
                Some(text) => format!("[{index}] {} scope = {text}", table.table),
                None => format!("[{index}] {} scope cleared", table.table),
            })
        }
        EditCommand::Mode(mode) => {
            rules.mode = *mode;
            let mut message = format!("sync_mode = {}", mode.as_str());
            if *mode == SyncMode::Toggles {
                message.push_str(TOGGLES_REFRESH_HINT);
            }
            Ok(message)
        }
        EditCommand::Save => {
            rules.validate().map_err(|e| e.to_string())?;
            Ok(format!(
                "validated (checksum {:#x}) — writing {}",
                rules.checksum(),
                rules_file::RULES_FILE_NAME
            ))
        }
        EditCommand::Quit => Ok("bye".to_string()),
        EditCommand::Help => Ok(HELP_TEXT.to_string()),
        EditCommand::Unknown(raw) => Err(format!("unknown command: {raw:?} — try ?")),
    }
}

/// `Toggle`/`Scope` mutate the generator-owned `[tables.*]` section, so both
/// are refused outside `toggles` mode — `all` has no toggles to mean
/// anything, `hand` has frozen the generator in favor of `[[rules]]`.
fn require_toggles_mode(rules: &SyncRules) -> Result<(), String> {
    if rules.mode == SyncMode::Toggles {
        return Ok(());
    }
    Err(format!(
        "sync_mode is `{}` — the generator is frozen. Run `nostos rules edit --mode toggles` to \
         hand truth back to the toggle editor.",
        rules.mode.as_str()
    ))
}

/// 1-based lookup matching the rendered `[1] [2] [3]` rows.
fn table_at(tables: &mut [TableRule], index: usize) -> Result<&mut TableRule, String> {
    index
        .checked_sub(1)
        .and_then(|i| tables.get_mut(i))
        .ok_or_else(|| format!("no table at index {index}"))
}

/// Pure render of the editor screen. 1-based row numbers must stay in lock
/// step with [`table_at`]'s indexing — `render_row_n_matches_toggle_n` pins
/// that down.
fn render(rules: &SyncRules) -> String {
    use std::fmt::Write as _;

    let mut out = format!(
        "nostos_rules.toml — sync_mode = {}          checksum {:#x}\n",
        rules.mode.as_str(),
        rules.checksum()
    );
    for (i, table) in rules.tables.iter().enumerate() {
        let n = i + 1;
        let mark = if table.sync { "x" } else { " " };
        let name = &table.table;
        match &table.scope {
            Some(scope) => {
                let _ = writeln!(out, "  [{n}] [{mark}] {name:<8} scope: {scope}");
            }
            None => {
                let _ = writeln!(out, "  [{n}] [{mark}] {name}");
            }
        }
    }
    out.push_str(HELP_TEXT);
    out
}

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
        RulesCommand::Edit(edit_args) => run_edit(edit_args, cwd),
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

/// The section that is present-on-disk but not authoritative under the
/// current `mode` — `None` when nothing is being silently ignored. `All`
/// authorizes neither `[tables.*]` nor `[[rules]]`, but team-lead ruling
/// (2026-08-06, Task 17) scoped the note to the two modes that actually
/// freeze a generator; `All` stays unnoted.
fn inactive_note(rules: &SyncRules) -> Option<&'static str> {
    match rules.mode {
        SyncMode::Toggles => (!rules.hand.is_empty()).then_some("hand"),
        SyncMode::Hand => (!rules.tables.is_empty()).then_some("toggles"),
        SyncMode::All => None,
    }
}

/// The claim names referenced by every currently-active, currently-synced
/// scope, sorted and deduped. Only scopes `validate()` has already accepted
/// reach here, so `ScopeExpr::parse` is infallible in practice; a scope that
/// still fails to parse is skipped rather than panicking, matching
/// `canonical_scope`'s fallback posture in `nostos-domain`.
fn referenced_claims(rules: &SyncRules) -> Vec<String> {
    let scopes: Vec<&str> = match rules.mode {
        SyncMode::Toggles => rules
            .tables
            .iter()
            .filter(|t| t.sync)
            .filter_map(|t| active_scope_text(t.scope.as_deref()))
            .collect(),
        SyncMode::Hand => rules
            .hand
            .iter()
            .filter_map(|h| active_scope_text(h.scope.as_deref()))
            .collect(),
        SyncMode::All => Vec::new(),
    };
    let mut claims: Vec<String> = scopes
        .into_iter()
        .filter_map(|s| ScopeExpr::parse(s).ok())
        .flat_map(|expr| {
            expr.referenced_claims()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    claims.sort();
    claims.dedup();
    claims
}

fn active_scope_text(scope: Option<&str>) -> Option<&str> {
    scope.map(str::trim).filter(|s| !s.is_empty())
}

/// Render the `nostos rules check` report (Task 17). Pure — no disk I/O, no
/// `println!` — so the shape is unit-testable. `Err` carries the same text
/// `SyncRules::validate()` produced, so the caller can print it and exit
/// non-zero without duplicating the validation logic.
pub fn check_report(rules: &SyncRules) -> Result<String, String> {
    use std::fmt::Write as _;

    rules.validate().map_err(|e| e.to_string())?;

    let mut out = String::from("nostos_rules.toml — valid\n");

    let note = inactive_note(rules)
        .map(|name| format!("  ({name} section present but inactive)"))
        .unwrap_or_default();
    let _ = writeln!(out, "  {:<10} {}{note}", "sync_mode:", rules.mode.as_str());
    let _ = writeln!(out, "  {:<10} {:#x}", "checksum:", rules.checksum());

    match rules.mode {
        SyncMode::Toggles => {
            let synced: Vec<&TableRule> = rules.tables.iter().filter(|t| t.sync).collect();
            let _ = writeln!(
                out,
                "  {:<10} {} of {} tables",
                "synced:",
                synced.len(),
                rules.tables.len()
            );
            for table in synced {
                match active_scope_text(table.scope.as_deref()) {
                    Some(scope) => {
                        let _ = writeln!(out, "    {:<8}  {scope}", table.table);
                    }
                    None => {
                        let _ = writeln!(out, "    {}", table.table);
                    }
                }
            }
        }
        SyncMode::Hand => {
            let _ = writeln!(
                out,
                "  {:<10} {} of {} tables",
                "synced:",
                rules.hand.len(),
                rules.hand.len()
            );
            for rule in &rules.hand {
                match active_scope_text(rule.scope.as_deref()) {
                    Some(scope) => {
                        let _ = writeln!(out, "    {:<8}  {scope}", rule.table);
                    }
                    None => {
                        let _ = writeln!(out, "    {}", rule.table);
                    }
                }
            }
        }
        SyncMode::All => {
            // Team-lead ruling (2026-08-06, Task 17): `all` still gets a
            // `synced:` line rather than being omitted — the check
            // command's job is making sync scope visible, and silence
            // for the widest scope would invert that.
            let _ = writeln!(out, "  {:<10} all tables (unscoped)", "synced:");
        }
    }

    let claims = referenced_claims(rules);
    let claims_text = if claims.is_empty() {
        "(none)".to_string()
    } else {
        claims.join(", ")
    };
    let _ = write!(out, "  claims referenced: {claims_text}");

    Ok(out)
}

fn run_check(cwd: &Path) -> Result<()> {
    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);
    match rules_file::load(&rules_path)? {
        None => println!(
            "no {} at {} — sync_mode defaults to `all` (zero-config).",
            rules_file::RULES_FILE_NAME,
            rules_path.display()
        ),
        Some(rules) => match check_report(&rules) {
            Ok(report) => println!("{report}"),
            Err(message) => anyhow::bail!("{message}"),
        },
    }
    Ok(())
}

/// `--mode` rewrites `sync_mode` via `rules_file::set_mode` and returns —
/// no re-introspection of the database, no toggle loop. Without `--mode`,
/// loads the existing file and drives [`apply_edit`] off stdin lines until
/// `q`/`w` (or EOF, which `prompt::prompt_default` turns into `q`).
fn run_edit(args: EditRulesArgs, cwd: &Path) -> Result<()> {
    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);

    if let Some(mode_str) = args.mode {
        let mode = SyncMode::parse(&mode_str).ok_or_else(|| {
            anyhow::anyhow!("unknown --mode {mode_str:?} (expected all|toggles|hand)")
        })?;
        rules_file::set_mode(&rules_path, mode)
            .with_context(|| format!("switching sync_mode in {}", rules_path.display()))?;
        let hint = if mode == SyncMode::Toggles {
            TOGGLES_REFRESH_HINT
        } else {
            ""
        };
        println!(
            "\u{2713} sync_mode = {} ({}){hint}",
            mode.as_str(),
            rules_path.display()
        );
        return Ok(());
    }

    let loaded = rules_file::load(&rules_path)
        .with_context(|| format!("reading {}", rules_path.display()))?;
    let mut rules = loaded.with_context(|| {
        format!(
            "{} does not exist — run `nostos rules init` first",
            rules_path.display()
        )
    })?;

    loop {
        println!("{}", render(&rules));
        // ponytail: a blank line (including EOF, which read_line also leaves
        // blank) defaults to `q` so `nostos rules edit < /dev/null` exits
        // instead of spinning; upgrade to a real EOF signal if that default
        // ever surprises an interactive user.
        let line = prompt::prompt_default("> ", "q").context("reading edit command")?;
        let cmd = parse_edit_command(&line);
        match apply_edit(&mut rules, &cmd) {
            Ok(message) => {
                println!("{message}");
                if cmd == EditCommand::Save {
                    rules_file::save(&rules_path, &rules)
                        .with_context(|| format!("writing {}", rules_path.display()))?;
                }
                if cmd == EditCommand::Quit {
                    break;
                }
            }
            Err(message) => println!("! {message}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_domain::HandRule;

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

    #[test]
    fn parse_toggle_and_scope_and_mode() {
        assert_eq!(parse_edit_command("1"), EditCommand::Toggle(1));
        assert_eq!(
            parse_edit_command("s 1 owner_id = claims.sub"),
            EditCommand::Scope {
                index: 1,
                scope: Some("owner_id = claims.sub".to_string()),
            }
        );
        assert_eq!(
            parse_edit_command("s 2"),
            EditCommand::Scope {
                index: 2,
                scope: None,
            }
        );
        assert_eq!(
            parse_edit_command("mode hand"),
            EditCommand::Mode(SyncMode::Hand)
        );
        assert_eq!(
            parse_edit_command("mode all"),
            EditCommand::Mode(SyncMode::All)
        );
        assert_eq!(parse_edit_command("w"), EditCommand::Save);
        assert_eq!(parse_edit_command("q"), EditCommand::Quit);
        assert_eq!(parse_edit_command("?"), EditCommand::Help);
        assert_eq!(
            parse_edit_command("bogus"),
            EditCommand::Unknown("bogus".to_string())
        );
    }

    #[test]
    fn edit_refuses_toggle_in_hand_mode() {
        let mut rules = rules_from_tables(&["tasks".to_string()], SyncMode::Hand, false);

        let err = apply_edit(&mut rules, &EditCommand::Toggle(1))
            .expect_err("hand mode must reject a toggle edit");

        assert_eq!(
            err,
            "sync_mode is `hand` — the generator is frozen. Run `nostos rules edit --mode \
             toggles` to hand truth back to the toggle editor."
        );
        assert!(
            !rules.tables[0].sync,
            "rejected toggle must not mutate the ruleset"
        );
    }

    #[test]
    fn edit_allows_mode_switch_in_any_mode() {
        for starting_mode in [SyncMode::All, SyncMode::Toggles, SyncMode::Hand] {
            let mut rules = rules_from_tables(&["tasks".to_string()], starting_mode, false);

            apply_edit(&mut rules, &EditCommand::Mode(SyncMode::Hand))
                .expect("mode switch must be allowed regardless of current mode");

            assert_eq!(rules.mode, SyncMode::Hand);
        }
    }

    #[test]
    fn mode_switch_to_toggles_includes_refresh_hint() {
        let mut rules = rules_from_tables(&["tasks".to_string()], SyncMode::Hand, false);

        let message = apply_edit(&mut rules, &EditCommand::Mode(SyncMode::Toggles))
            .expect("switching to toggles must succeed");

        assert!(
            message.contains("nostos rules init"),
            "switching back to toggles must hint at re-detection, got: {message}"
        );

        let to_hand = apply_edit(&mut rules, &EditCommand::Mode(SyncMode::Hand))
            .expect("switching to hand must succeed");
        assert!(
            !to_hand.contains("nostos rules init"),
            "the refresh hint is toggles-specific, got: {to_hand}"
        );
    }

    #[test]
    fn scope_is_validated_on_entry() {
        let mut rules = rules_from_tables(&["tasks".to_string()], SyncMode::Toggles, false);
        let before = rules.clone();

        let cmd = parse_edit_command("s 1 owner_id OR x");
        let err = apply_edit(&mut rules, &cmd).expect_err("`owner_id OR x` is not a valid scope");

        // `owner_id` isn't followed by a comparison operator (`OR` isn't
        // one), so this is `ScopeError::MissingOperator` rather than
        // `Unsupported` — either way it's ScopeError text, which is what
        // the brief requires apply_edit to surface unchanged.
        assert!(
            err.contains("comparison operator"),
            "expected ScopeError text, got: {err}"
        );
        assert_eq!(
            rules, before,
            "a rejected scope edit must not mutate the ruleset"
        );
    }

    #[test]
    fn render_row_n_matches_toggle_n() {
        let rules = rules_from_tables(
            &[
                "notes".to_string(),
                "projects".to_string(),
                "tasks".to_string(),
            ],
            SyncMode::Toggles,
            false,
        );
        let screen = render(&rules);
        assert!(
            screen.contains("[2] [ ] projects"),
            "row [2] must be `projects` (rules.tables[1] after sort), got:\n{screen}"
        );

        let mut edited = rules;
        apply_edit(&mut edited, &EditCommand::Toggle(2)).expect("toggle row 2");
        assert!(
            edited.tables[1].sync,
            "Toggle(2) must flip the same table the render labeled [2]"
        );
    }

    #[test]
    fn save_preserves_hand_section() {
        let dir =
            std::env::temp_dir().join(format!("nostos-rules-edit-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(rules_file::RULES_FILE_NAME);

        let mut rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![TableRule {
                table: "tasks".to_string(),
                sync: false,
                scope: None,
            }],
            hand: vec![HandRule {
                table: "tasks".to_string(),
                scope: Some("org_id = claims.org_id".to_string()),
            }],
        };

        apply_edit(&mut rules, &EditCommand::Toggle(1)).expect("toggle in toggles mode");
        rules_file::save(&path, &rules).expect("save");

        let loaded = rules_file::load(&path).expect("load").expect("file exists");
        assert_eq!(
            loaded.hand, rules.hand,
            "hand section must round-trip untouched"
        );

        let text = std::fs::read_to_string(&path).expect("read saved file");
        assert!(
            text.contains("org_id = claims.org_id"),
            "hand scope text must survive a toggles-mode edit + save"
        );
    }

    #[test]
    fn check_reports_mode_and_checksum() {
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
                TableRule {
                    table: "logs".to_string(),
                    sync: false,
                    scope: None,
                },
            ],
            hand: vec![],
        };

        let report = check_report(&rules).expect("well-formed rules must report Ok");

        assert!(report.starts_with("nostos_rules.toml — valid"));
        assert!(report.contains("sync_mode: toggles"), "got:\n{report}");
        assert!(
            report.contains(&format!("checksum:  {:#x}", rules.checksum())),
            "got:\n{report}"
        );
        assert!(
            report.contains("synced:    2 of 3 tables"),
            "unsynced `logs` must not count, got:\n{report}"
        );
        assert!(report.contains("tasks"));
        assert!(report.contains("owner_id = claims.sub"));
        assert!(report.contains("projects"));
        assert!(report.contains("org_id = claims.org_id"));
        assert!(
            !report.contains("logs"),
            "an unsynced table must not appear in the per-table list, got:\n{report}"
        );
    }

    #[test]
    fn check_flags_invalid_scope_with_table_name() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: Some("a = 1 OR b = 2".to_string()),
            }],
            hand: vec![],
        };

        let err = check_report(&rules).expect_err("an unparseable scope must fail the check");

        assert_eq!(
            err,
            rules.validate().unwrap_err().to_string(),
            "check_report must surface validate()'s own error text unchanged"
        );
        assert!(
            err.contains("tasks"),
            "the error must name the offending table, got: {err}"
        );
    }

    #[test]
    fn check_notes_inactive_section() {
        let with_hand = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: None,
            }],
            hand: vec![HandRule {
                table: "tasks".to_string(),
                scope: Some("org_id = claims.org_id".to_string()),
            }],
        };
        let report = check_report(&with_hand).expect("well-formed rules must report Ok");
        assert!(
            report.contains("(hand section present but inactive)"),
            "got:\n{report}"
        );

        let without_hand = SyncRules {
            hand: vec![],
            ..with_hand
        };
        let report = check_report(&without_hand).expect("well-formed rules must report Ok");
        assert!(
            !report.contains("section present but inactive"),
            "an empty hand section must not be noted, got:\n{report}"
        );
    }

    /// Beyond the brief's four required tests — team-lead ruling (2026-08-06)
    /// asked for the `All`-mode branch to get direct coverage since it's the
    /// one shape the brief's worked example never exercises.
    #[test]
    fn check_renders_all_mode_as_unscoped() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::All,
            tables: vec![],
            hand: vec![],
        };

        let report = check_report(&rules).expect("well-formed rules must report Ok");

        assert!(report.contains("sync_mode: all"), "got:\n{report}");
        assert!(
            report.contains("synced:    all tables (unscoped)"),
            "got:\n{report}"
        );
        assert!(
            report.contains("claims referenced: (none)"),
            "got:\n{report}"
        );
        assert!(
            !report.contains("section present but inactive"),
            "all mode has nothing to note as inactive, got:\n{report}"
        );
    }

    /// Beyond the brief's four required tests — team-lead ruling (2026-08-06)
    /// asked for the `Hand`-mode branch to get direct coverage, mirroring the
    /// toggles-mode worked example with roles swapped.
    #[test]
    fn check_renders_hand_mode_symmetrically() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Hand,
            tables: vec![TableRule {
                table: "logs".to_string(),
                sync: false,
                scope: None,
            }],
            hand: vec![
                HandRule {
                    table: "tasks".to_string(),
                    scope: Some("owner_id = claims.sub".to_string()),
                },
                HandRule {
                    table: "projects".to_string(),
                    scope: Some("org_id = claims.org_id".to_string()),
                },
            ],
        };

        let report = check_report(&rules).expect("well-formed rules must report Ok");

        assert!(report.contains("sync_mode: hand"), "got:\n{report}");
        assert!(
            report.contains("(toggles section present but inactive)"),
            "hand mode with a non-empty tables section must note it, got:\n{report}"
        );
        assert!(
            report.contains("synced:    2 of 2 tables"),
            "got:\n{report}"
        );
        assert!(report.contains("tasks"));
        assert!(report.contains("owner_id = claims.sub"));
        assert!(report.contains("projects"));
        assert!(report.contains("org_id = claims.org_id"));
    }

    #[test]
    fn check_lists_referenced_claims_sorted_deduped() {
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
                    scope: Some("org_id = claims.org_id AND owner_id = claims.sub".to_string()),
                },
            ],
            hand: vec![],
        };

        let report = check_report(&rules).expect("well-formed rules must report Ok");

        assert!(
            report.contains("claims referenced: org_id, sub"),
            "claims must be sorted and deduped, got:\n{report}"
        );
    }
}
