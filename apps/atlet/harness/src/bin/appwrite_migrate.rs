//! `cargo run -p atlet-harness --bin appwrite_migrate -- [--apply]`

use anyhow::{Context, Result};
use atlet_harness::{appwrite_client::AppwriteClient, migrate, migrations::MigrationCatalog};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Appwrite Cloud region endpoint ending in /v1.
    #[arg(
        long,
        env = "APPWRITE_ENDPOINT",
        default_value = "https://fra.cloud.appwrite.io/v1"
    )]
    endpoint: String,
    /// The project to check or change.
    #[arg(long, env = "APPWRITE_PROJECT_ID")]
    project_id: String,
    /// Apply missing numbered migrations; omission is a read-only check.
    #[arg(long)]
    apply: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let key = nostos_infra::env::var("APPWRITE_API_KEY")
        .context("APPWRITE_API_KEY is required (server-only, never add it to Flutter)")?;
    let client = AppwriteClient::new(&args.endpoint, &args.project_id, &key)?;
    let catalog = MigrationCatalog::load()?;
    let status = migrate::run(&client, &catalog, args.apply).await?;
    println!(
        "Appwrite migrations applied: {:?}; missing: {:?}",
        status.applied, status.missing
    );
    if !args.apply && !status.missing.is_empty() {
        anyhow::bail!("Appwrite schema migrations are missing");
    }
    Ok(())
}
