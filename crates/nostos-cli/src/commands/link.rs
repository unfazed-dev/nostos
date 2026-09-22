//! `nostos link` — app-side: scaffold `.nostos/` (config.json + gitignored
//! `local/`) at the app repo root. See ADR-0023 D1/D3. Distinct from the
//! operator `nostos init` (publication + `nostos.toml`).

use std::path::Path;

use anyhow::{bail, Result};
use clap::Args;

use nostos_infra::rules_file::{self, RULES_FILE_NAME};

use crate::config::{Backend, LinkMode, ProjectConfig, DOT_NOSTOS_DIR, LOCAL_DIR};
use crate::direct;
use crate::prompt::prompt_nonempty;

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// `server` (a nostos-server holds the slot) or `direct` (the device syncs
    /// straight from the client's own Postgres — no Nostos server at all).
    #[arg(long, default_value = "server")]
    pub mode: String,
    /// The nostos-server `/sync` WebSocket URL (`ws://` or `wss://`). Prompted
    /// for if omitted. Direct mode derives it from the Supabase URL instead.
    #[arg(long)]
    pub sync_url: Option<String>,
    /// Direct mode: a synced table every authenticated device may read. Repeat
    /// per table. Required for any table with no scope — direct mode refuses to
    /// infer that an unscoped table is public.
    #[arg(long = "public")]
    pub public: Vec<String>,
    /// Direct mode: how long `cairn.prune()` keeps change rows.
    #[arg(long, default_value = direct::DEFAULT_RETENTION)]
    pub retention: String,
    /// Direct mode: also generate the push path, posting to this Edge Function
    /// URL when a change arrives for a scope with no awake device. Omit and no
    /// push objects are generated at all.
    #[arg(long)]
    pub push: Option<String>,
    /// Backend kind: `postgres` | `supabase` | `appwrite` (ADR-0023 D4).
    /// Defaults to `postgres` when omitted.
    #[arg(long)]
    pub backend: Option<String>,
    /// Supabase project URL (with `--backend supabase`).
    #[arg(long)]
    pub supabase_url: Option<String>,
    /// Supabase publishable (anon) key (with `--backend supabase`).
    #[arg(long)]
    pub supabase_anon_key: Option<String>,
    /// Human project name (informational).
    #[arg(long, default_value = "nostos-app")]
    pub project: String,
}

/// Run `nostos link`: resolve the sync URL + backend, write `.nostos/config.json`,
/// create the gitignored `.nostos/local/`, and ensure `.gitignore` covers it.
///
/// # Errors
/// [`anyhow::Error`] if the sync URL is not `ws://`/`wss://`, the backend kind
/// is unknown, a required (prompted) value is empty, or any disk write fails.
// `async` by contract: `main.rs` dispatches `run(args, &cwd).await` alongside
// the other subcommands, so the signature must stay async even though the link
// body is pure file IO today. ponytail: drop this allow if link grows a
// reachability probe against `sync_url` (the natural async surface).
#[allow(clippy::unused_async)]
pub async fn run(args: LinkArgs, cwd: &Path) -> Result<()> {
    let mode = match args.mode.as_str() {
        "server" => LinkMode::Server,
        "direct" => LinkMode::Direct,
        other => bail!("unknown --mode `{other}` (expected `server` or `direct`)"),
    };
    if mode == LinkMode::Direct {
        return run_direct(args, cwd);
    }
    let sync_url = match args.sync_url {
        Some(u) => u,
        None => prompt_nonempty("nostos-server /sync URL (ws:// or wss://): ")?,
    };
    if !(sync_url.starts_with("ws://") || sync_url.starts_with("wss://")) {
        bail!(
            "sync URL must start with `ws://` or `wss://` (got `{sync_url}`); \
             `nostos link` needs the nostos-server WebSocket endpoint"
        );
    }

    let backend = resolve_backend(
        args.backend.as_deref(),
        args.supabase_url.as_deref(),
        args.supabase_anon_key.as_deref(),
    )?;

    let config = ProjectConfig {
        mode: LinkMode::Server,
        project: args.project,
        sync_url,
        backend: Some(backend),
    };
    config.save(cwd)?;

    let local_dir = cwd.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
    std::fs::create_dir_all(&local_dir)?;

    ensure_gitignore_local(cwd)?;

    println!("\u{2713} wrote `.nostos/config.json`");
    println!("\u{2713} created gitignored `.nostos/local/`");
    println!(
        "note: only publishable keys belong in config.json \u{2014} \
         service keys / JWT secrets / DB passwords go in `.env` or `.nostos/local/`"
    );
    println!("next: `nostos pull && nostos gen`");
    Ok(())
}

/// Direct mode: no sync server exists, so the "sync URL" is the Realtime
/// socket the doorbell joins. Everything else in `.nostos/config.json` is
/// unchanged, and the generated SQL lands in `.nostos/direct.sql`.
fn run_direct(args: LinkArgs, cwd: &Path) -> Result<()> {
    if args.sync_url.is_some() {
        bail!(
            "--sync-url is server mode only: direct mode has no nostos-server to \
             point at. The device reaches the database itself, and the socket it \
             opens is the project's own Realtime endpoint."
        );
    }
    let backend = resolve_backend(
        Some(args.backend.as_deref().unwrap_or("supabase")),
        args.supabase_url.as_deref(),
        args.supabase_anon_key.as_deref(),
    )?;
    let Backend::Supabase { url, .. } = &backend else {
        bail!(
            "direct mode requires `--backend supabase`: the generated SQL calls \
             auth.jwt(), realtime.send() and grants to the `authenticated` role, \
             none of which exist on a plain Postgres. Run a nostos-server for those \
             (`--mode server`)."
        );
    };

    let rules_path = cwd.join(RULES_FILE_NAME);
    let rules = rules_file::load(&rules_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no {RULES_FILE_NAME} at {} — direct mode generates one trigger per \
             synced table, so it needs the table list first: run `nostos rules init`",
            rules_path.display()
        )
    })?;
    let tables = direct::plan(&rules, &args.public)?;
    let push = args.push.as_ref().map(|endpoint| direct::PushConfig {
        endpoint: endpoint.clone(),
        ..direct::PushConfig::default()
    });
    let sql = direct::render_with_push(&tables, &args.retention, push.as_ref());

    let config = ProjectConfig {
        mode: LinkMode::Direct,
        project: args.project,
        sync_url: realtime_url(url),
        backend: Some(backend.clone()),
    };
    config.save(cwd)?;

    let local_dir = cwd.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
    std::fs::create_dir_all(&local_dir)?;
    ensure_gitignore_local(cwd)?;

    let sql_path = cwd.join(DOT_NOSTOS_DIR).join(direct::OUTPUT_FILE);
    std::fs::write(&sql_path, &sql)?;

    println!("\u{2713} wrote `.nostos/config.json` (mode: direct)");
    println!(
        "\u{2713} wrote `.nostos/{}` \u{2014} {} table(s), retention {}",
        direct::OUTPUT_FILE,
        tables.len(),
        args.retention
    );
    println!(
        "next: apply it \u{2014} `psql \"$DATABASE_URL\" -f .nostos/{}`",
        direct::OUTPUT_FILE
    );
    println!("      then turn OFF \"Allow public access\" in the project's Realtime settings,");
    if push.is_some() {
        println!(
            "      push is included \u{2014} `create extension if not exists pg_net;`, set \
             `cairn.push_config.secret`, and deploy supabase/functions/cairn-push."
        );
    }
    println!("      then `nostos doctor --mode direct` to check it landed.");
    Ok(())
}

/// `https://xyz.supabase.co` -> `wss://xyz.supabase.co/realtime/v1/websocket`,
/// the endpoint `nostos_client::doorbell` joins.
fn realtime_url(project_url: &str) -> String {
    let host = project_url
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let scheme = if project_url.starts_with("http://") {
        "ws"
    } else {
        "wss"
    };
    format!("{scheme}://{host}/realtime/v1/websocket")
}

fn resolve_backend(
    kind: Option<&str>,
    supabase_url: Option<&str>,
    supabase_anon_key: Option<&str>,
) -> Result<Backend> {
    match kind.unwrap_or("postgres") {
        "postgres" => Ok(Backend::Postgres),
        "supabase" => {
            let url = match supabase_url {
                Some(u) => u.to_string(),
                None => prompt_nonempty("Supabase project URL (https://...supabase.co): ")?,
            };
            let anon_key = match supabase_anon_key {
                Some(k) => k.to_string(),
                None => prompt_nonempty("Supabase publishable (anon) key: ")?,
            };
            Ok(Backend::Supabase { url, anon_key })
        }
        "appwrite" => bail!("appwrite backend is post-v1 (ADR-0023 D4)"),
        other => {
            bail!("unknown backend `{other}`; valid options: `postgres`, `supabase`, `appwrite`")
        }
    }
}

fn ensure_gitignore_local(cwd: &Path) -> Result<()> {
    const ENTRY: &str = ".nostos/local/";
    let path = cwd.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == ENTRY) {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(ENTRY);
    content.push('\n');
    std::fs::write(&path, content)?;
    Ok(())
}
