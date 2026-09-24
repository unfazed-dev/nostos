//! `nostos` — the CLI for a Postgres + Supabase Nostos backend: `init` sets up
//! the publication and writes `nostos.toml`/`.env`, `dev` runs the sync
//! server locally, `doctor` reports health, `deploy` generates self-host
//! config. See `docs/plans/flutter-supabase-plug-and-play-launch.md` (W3).

use anyhow::{Context, Result};
use nostos_cli::commands;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "nostos",
    version,
    about = "Nostos — a local-first sync backend for Postgres + Supabase"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Connect to Postgres, create/update the publication, write nostos.toml + .env.
    Init(commands::init::InitArgs),
    /// Run nostos-server locally using nostos.toml + .env.
    Dev,
    /// Connectivity, replication health, and JWKS reachability checks
    /// (`--mode direct`: the generated schema, grants, policies, log growth).
    Doctor(commands::doctor::DoctorArgs),
    /// Generate a self-host deploy config (fly/railway) from nostos.toml.
    Deploy(commands::deploy::DeployArgs),
    /// App-side: scaffold `.nostos/` (config.json + gitignored local/).
    Link(commands::link::LinkArgs),
    /// App-side: fetch GET /schema → `.nostos/schema.json`.
    Pull(commands::pull::PullArgs),
    /// App-side: generate per-SDK source from `.nostos/`.
    Gen(commands::gen::GenArgs),
    /// Generate/edit/validate nostos_rules.toml (per-table sync rules, ADR-0031).
    Rules(commands::rules::RulesArgs),
    /// Configure/validate push credentials in .env (ADR-0037/0038).
    Push(commands::push::PushArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let cli = nostos_infra::env::parse::<Cli>();
    let cwd = std::env::current_dir().context("reading current directory")?;

    match cli.command {
        Commands::Init(args) => commands::init::run(args, &cwd).await,
        Commands::Dev => commands::dev::run(&cwd).await,
        Commands::Doctor(args) => commands::doctor::run(args, &cwd).await,
        Commands::Deploy(args) => commands::deploy::run(args, &cwd),
        Commands::Link(args) => commands::link::run(args, &cwd).await,
        Commands::Pull(args) => commands::pull::run(args, &cwd).await,
        Commands::Gen(args) => commands::gen::run(args, &cwd).await,
        Commands::Rules(args) => commands::rules::run(args, &cwd).await,
        Commands::Push(args) => commands::push::run(args, &cwd).await,
    }
}
