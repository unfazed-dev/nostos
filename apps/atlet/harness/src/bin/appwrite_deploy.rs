//! Package the versioned Atlet Rust Function and optionally deploy it to Cloud.

use std::{io::Cursor, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use atlet_harness::auth::Credentials;
use clap::Parser;
use flate2::{write::GzEncoder, Compression};
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

#[derive(Parser)]
struct Args {
    /// Upload and activate after the Appwrite build succeeds.
    #[arg(long)]
    apply: bool,
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    #[arg(long, default_value = "atlet_sync")]
    function_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let archive = package()?;
    let checksum = hex::encode(Sha256::digest(&archive));
    println!(
        "Atlet Function package: {} bytes, sha256 {checksum}",
        archive.len()
    );
    if !args.apply {
        println!("Pass --apply to upload and activate this package.");
        return Ok(());
    }
    let credentials = Credentials::read(&args.credentials)?;
    let endpoint = credentials.get("APPWRITE_ENDPOINT")?.trim_end_matches('/');
    let project = credentials.get("APPWRITE_PROJECT_ID")?;
    let key = nostos_infra::env::var("APPWRITE_API_KEY")
        .context("APPWRITE_API_KEY is required to deploy")?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;
    let url = format!("{endpoint}/functions/{}/deployments", args.function_id);
    let form = Form::new()
        .text("activate", "true")
        .text("entrypoint", "main.rs")
        .text("commands", "cargo build --release --locked")
        .part(
            "code",
            Part::bytes(archive)
                .file_name("atlet-sync.tar.gz")
                .mime_str("application/gzip")?,
        );
    let response = http
        .post(&url)
        .header("X-Appwrite-Project", project)
        .header("X-Appwrite-Key", &key)
        .multipart(form)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "Appwrite deployment HTTP {status}: {}",
            bounded_message(&body)
        );
    }
    let id = body["$id"].as_str().context("deployment ID missing")?;
    println!("Deployment {id} queued; waiting for the hosted Rust build.");
    for _ in 0..48 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let response = http
            .get(format!("{url}/{id}"))
            .header("X-Appwrite-Project", project)
            .header("X-Appwrite-Key", &key)
            .send()
            .await?;
        let status = response.status();
        let deployment: Value = response.json().await?;
        if !status.is_success() {
            bail!(
                "deployment status HTTP {status}: {}",
                bounded_message(&deployment)
            );
        }
        match deployment["status"].as_str().unwrap_or_default() {
            "ready" => {
                println!("Appwrite Function {id} ready and activated.");
                return Ok(());
            }
            "failed" | "canceled" => {
                let logs = deployment["buildLogs"].as_str().unwrap_or_default();
                bail!(
                    "Function build failed: {}",
                    logs.chars()
                        .rev()
                        .take(2000)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
                );
            }
            _ => {}
        }
    }
    bail!("Function build did not settle within four minutes; inspect deployment {id}")
}

fn package() -> Result<Vec<u8>> {
    let root = PathBuf::from("apps/atlet/appwrite/function");
    let mut tar = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for name in ["Cargo.toml", "Cargo.lock", "main.rs", "api.rs"] {
        let mut data = std::fs::read(root.join(name)).with_context(|| format!("read {name}"))?;
        if name == "Cargo.toml" {
            // The Appwrite runtime copies the Function crate below its own
            // workspace root during compilation. A nested root is rejected.
            let manifest = String::from_utf8(data)?;
            data = manifest.replace("\n[workspace]\n", "\n").into_bytes();
        }
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, Cursor::new(data))?;
    }
    let encoder = tar.into_inner()?;
    Ok(encoder.finish()?)
}

fn bounded_message(value: &Value) -> String {
    value["message"]
        .as_str()
        .unwrap_or("unknown")
        .chars()
        .take(400)
        .collect()
}
