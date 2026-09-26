//! `cargo run -p atlet-harness --bin appwrite_probe -- --writers 4`

use std::sync::Arc;

use anyhow::{Context, Result};
use atlet_harness::{appwrite_client::AppwriteClient, probe};
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "APPWRITE_ENDPOINT",
        default_value = "https://fra.cloud.appwrite.io/v1"
    )]
    endpoint: String,
    #[arg(long, env = "APPWRITE_PROJECT_ID")]
    project_id: String,
    #[arg(long, default_value_t = 4)]
    writers: usize,
    /// Journal-delete a manual probe fixture left from protocol exploration.
    #[arg(long)]
    cleanup_legacy: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let key = nostos_infra::env::var("APPWRITE_API_KEY").context("APPWRITE_API_KEY is required")?;
    let client = Arc::new(AppwriteClient::new(&args.endpoint, &args.project_id, &key)?);
    if !args.cleanup_legacy.is_empty() {
        for id in &args.cleanup_legacy {
            let seq = probe::cleanup_legacy(&client, id).await?;
            println!("Removed legacy fixture {id} at sequence {seq}");
        }
        return Ok(());
    }
    let result = probe::run(client, args.writers).await?;
    println!(
        "Appwrite probe: {} creates, {} deletes, {} conflicts retried, head {}",
        result.creates, result.deletes, result.conflicts, result.head
    );
    Ok(())
}
