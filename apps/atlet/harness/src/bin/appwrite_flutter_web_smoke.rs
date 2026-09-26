//! Build and exercise the full Flutter Atlet UI in Chrome against Appwrite Cloud.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use atlet_harness::auth::{sign_in, Credentials};
use clap::Parser;
use nostos_client::{appwrite::AppwriteDirectClient, SqliteStorage};
use nostos_core::{PendingWrite, WriteOp};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "customer_a", value_parser = ["admin", "customer_a", "customer_b"])]
    role: String,
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    #[arg(long, default_value = "apps/atlet/.results")]
    evidence_dir: PathBuf,
    /// Reuse an existing Flutter web build during local iteration.
    #[arg(long)]
    no_build: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let started = Instant::now();
    let args = nostos_infra::env::parse::<Args>();
    let root = find_checkout()?;
    let flutter = root.join("apps/atlet/flutter");
    let credentials = absolute(&root, &args.credentials);
    let secrets = Credentials::read(&credentials)?;
    let evidence_dir = absolute(&root, &args.evidence_dir);
    fs::create_dir_all(&evidence_dir)?;
    let run_id = Uuid::new_v4().simple().to_string();
    let evidence_path = evidence_dir.join(format!("appwrite-flutter-web-{run_id}.json"));
    if args.role == "admin" {
        finish_open_web_orders(&secrets)
            .await
            .context("finish prior incomplete browser demo orders")?;
        cleanup_customer_a_cart(&secrets)
            .await
            .context("clear dedicated customer A demo cart before checkout")?;
    }
    let prefix = match args.role.as_str() {
        "admin" => "ATLET_ADMIN",
        "customer_a" => "ATLET_USER_A",
        "customer_b" => "ATLET_USER_B",
        _ => unreachable!("clap restricts role"),
    };
    if !args.no_build {
        // Flutter 3.47's SwiftPM plugin copy runs even during web pub get; a
        // fresh checkout needs these generated parent directories to exist.
        fs::create_dir_all(flutter.join("build/ios/SourcePackages"))?;
        fs::create_dir_all(flutter.join("build/macos/SourcePackages"))?;
        let mut build = Command::new(if has_fvm() { "fvm" } else { "flutter" });
        if has_fvm() {
            build.arg("flutter");
        }
        let output = build
            .args(["build", "web", "--release"])
            .arg("--dart-define=ATLET_PROVIDER=appwrite")
            .arg(format!(
                "--dart-define=APPWRITE_ENDPOINT={}",
                secrets.get("APPWRITE_ENDPOINT")?
            ))
            .arg(format!(
                "--dart-define=APPWRITE_PROJECT_ID={}",
                secrets.get("APPWRITE_PROJECT_ID")?
            ))
            .current_dir(&flutter)
            .output()
            .context("build Atlet Flutter web")?;
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            bail!("Flutter web build failed");
        }
    }
    let script = flutter.join("web/e2e/appwrite_cloud.cjs");
    let mut browser = Command::new("node");
    browser
        .arg(&script)
        .env("NODE_PATH", root.join("sdk/nostos_web/node_modules"))
        .env("ATLET_WEB_ROOT", flutter.join("build/web"))
        .env("ATLET_WEB_ENDPOINT", secrets.get("APPWRITE_ENDPOINT")?)
        .env("ATLET_WEB_PROJECT_ID", secrets.get("APPWRITE_PROJECT_ID")?)
        .env("ATLET_WEB_EMAIL", secrets.get(&format!("{prefix}_EMAIL"))?)
        .env(
            "ATLET_WEB_PASSWORD",
            secrets.get(&format!("{prefix}_PASSWORD"))?,
        )
        .env("ATLET_WEB_ROLE", &args.role)
        .env(
            "ATLET_WEB_CUSTOMER_A_EMAIL",
            secrets.get("ATLET_USER_A_EMAIL")?,
        )
        .env(
            "ATLET_WEB_CUSTOMER_A_PASSWORD",
            secrets.get("ATLET_USER_A_PASSWORD")?,
        )
        .env(
            "ATLET_WEB_CUSTOMER_B_EMAIL",
            secrets.get("ATLET_USER_B_EMAIL")?,
        )
        .env(
            "ATLET_WEB_CUSTOMER_B_PASSWORD",
            secrets.get("ATLET_USER_B_PASSWORD")?,
        )
        .env("ATLET_WEB_EVIDENCE", &evidence_path)
        .current_dir(&flutter);
    if args.role == "admin" {
        // Separate login sessions mint distinct valid JWTs for two same-user
        // browser tabs. They are process-only and never enter the evidence.
        let http = reqwest::Client::new();
        for (key, account_prefix, user_id) in [
            ("ATLET_WEB_FOLLOWER_JWT", "ATLET_ADMIN", "atlet_admin_demo"),
            ("ATLET_WEB_ROTATED_JWT", "ATLET_ADMIN", "atlet_admin_demo"),
            (
                "ATLET_WEB_WRONG_ACCOUNT_JWT",
                "ATLET_USER_B",
                "atlet_user_b_demo",
            ),
        ] {
            let jwt = sign_in(
                &http,
                secrets.get("APPWRITE_ENDPOINT")?,
                secrets.get("APPWRITE_PROJECT_ID")?,
                secrets.get(&format!("{account_prefix}_EMAIL"))?,
                secrets.get(&format!("{account_prefix}_PASSWORD"))?,
                user_id,
            )
            .await?;
            browser.env(key, jwt);
        }
    }
    let output = browser.output().context("run Chrome visual smoke")?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let browser: Value = serde_json::from_slice(
        &fs::read(&evidence_path).context("browser did not write evidence")?,
    )?;
    // A second, native Nostos device verifies the browser write reached the
    // shared cloud journal, then deletes the fixture before this run returns.
    let session_title = browser["session_title"].as_str();
    let discarded_title = browser["discarded_title"].as_str();
    let product_name = browser["product_name"].as_str();
    let order_id = browser["order_id"].as_str();
    let cloud = if session_title.is_some()
        || discarded_title.is_some()
        || product_name.is_some()
        || order_id.is_some()
    {
        Some(
            verify_and_cleanup(
                &secrets,
                prefix,
                session_title,
                discarded_title,
                product_name,
                order_id,
            )
            .await,
        )
    } else {
        None
    };
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()?
        .stdout;
    let source_dirty = !Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(&root)
        .output()?
        .stdout
        .is_empty();
    let evidence = json!({
        "source_ref":String::from_utf8_lossy(&commit).trim(),
        "source_dirty":source_dirty,
        "run_id":run_id,
        "fixture":{"id":run_id,"accounts":["atlet_admin_demo","atlet_user_a_demo","atlet_user_b_demo"]},
        "app_version":pubspec_version(&flutter.join("pubspec.yaml"))?,
        "sdk_version":pubspec_version(&root.join("sdk/nostos_flutter/pubspec.yaml"))?,
        "sdk":"nostos_flutter",
        "device":"chrome",
        "provider":"appwrite",
        "role":args.role,
        "browser":browser,
        "cloud_verified":cloud.as_ref().is_some_and(Result::is_ok),
        "cloud_error":cloud.as_ref().and_then(|result| result.as_ref().err()).map(ToString::to_string),
        "browser_exit_code":output.status.code(),
        "duration_ms":started.elapsed().as_millis(),
        "success":output.status.success() && browser["success"] == true && cloud.as_ref().is_some_and(Result::is_ok),
    });
    fs::write(&evidence_path, serde_json::to_vec_pretty(&evidence)?)?;
    println!("Evidence: {}", evidence_path.display());
    if !output.status.success()
        || browser["success"] != true
        || !evidence["cloud_verified"].as_bool().unwrap_or(false)
    {
        bail!("Appwrite Flutter web visual smoke failed");
    }
    Ok(())
}

fn pubspec_version(path: &Path) -> Result<String> {
    fs::read_to_string(path)?
        .lines()
        .find_map(|line| line.strip_prefix("version: "))
        .map(str::to_string)
        .with_context(|| format!("version missing from {}", path.display()))
}

async fn verify_and_cleanup(
    secrets: &Credentials,
    prefix: &str,
    title: Option<&str>,
    discarded_title: Option<&str>,
    product_name: Option<&str>,
    order_id: Option<&str>,
) -> Result<()> {
    let user = match prefix {
        "ATLET_ADMIN" => "atlet_admin_demo",
        "ATLET_USER_A" => "atlet_user_a_demo",
        "ATLET_USER_B" => "atlet_user_b_demo",
        _ => bail!("unknown Atlet cloud role"),
    };
    let endpoint = secrets.get("APPWRITE_ENDPOINT")?;
    let project = secrets.get("APPWRITE_PROJECT_ID")?;
    let jwt = sign_in(
        &reqwest::Client::new(),
        endpoint,
        project,
        secrets.get(&format!("{prefix}_EMAIL"))?,
        secrets.get(&format!("{prefix}_PASSWORD"))?,
        user,
    )
    .await?;
    let path = std::env::temp_dir().join(format!("atlet-web-verify-{}.sqlite", Uuid::new_v4()));
    let storage = SqliteStorage::open(&path.to_string_lossy())?;
    let client = AppwriteDirectClient::new(endpoint, project, "atlet_sync", storage)?;
    client.set_user(user, &jwt).await?;
    client.sync().await?;
    let rows = client
        .engine()
        .lock()
        .expect("Atlet verify mutex poisoned")
        .storage()
        .rows_for("sessions")?;
    let parsed = rows
        .into_iter()
        .filter_map(|(pk, payload)| {
            serde_json::from_slice::<Value>(&payload)
                .ok()
                .map(|value| (pk, value))
        })
        .collect::<Vec<_>>();
    let session_verified = title.is_none_or(|title| {
        parsed
            .iter()
            .any(|(_, value)| value["title"] == title && !value["server_committed_at"].is_null())
    });
    let discarded_absent =
        discarded_title.is_none_or(|title| parsed.iter().all(|(_, value)| value["title"] != title));
    let mut cleanup = parsed
        .iter()
        .filter(|(_, value)| {
            value["title"].as_str().is_some_and(|name| {
                name.starts_with("Atlet web offline ") || name.starts_with("tlet web offline ")
            })
        })
        .map(|(pk, _)| PendingWrite {
            table: "sessions".into(),
            op: WriteOp::Delete,
            pk: pk.clone(),
            payload_json: None,
        })
        .collect::<Vec<_>>();
    let mut product_verified = product_name.is_none();
    if prefix == "ATLET_ADMIN" || product_name.is_some() {
        let products = client
            .engine()
            .lock()
            .expect("Atlet verify mutex poisoned")
            .storage()
            .rows_for("products")?;
        let products = products
            .into_iter()
            .filter_map(|(pk, payload)| {
                serde_json::from_slice::<Value>(&payload)
                    .ok()
                    .map(|value| (pk, value))
            })
            .collect::<Vec<_>>();
        product_verified =
            product_name.is_none_or(|name| products.iter().any(|(_, value)| value["name"] == name));
        cleanup.extend(
            products
                .iter()
                .filter(|(_, value)| {
                    value["name"].as_str().is_some_and(|name| {
                        name.starts_with("Atlet web product ")
                            || name.starts_with("tlet web product ")
                    })
                })
                .map(|(pk, _)| PendingWrite {
                    table: "products".into(),
                    op: WriteOp::Delete,
                    pk: pk.clone(),
                    payload_json: None,
                }),
        );
    }
    let order_verified = if let Some(order_id) = order_id {
        let orders = client
            .engine()
            .lock()
            .expect("Atlet verify mutex poisoned")
            .storage()
            .rows_for("orders")?;
        orders.into_iter().any(|(pk, payload)| {
            pk == order_id
                && serde_json::from_slice::<Value>(&payload).is_ok_and(|value| {
                    value["status"] == "delivered" && value["user_id"] == "atlet_user_a_demo"
                })
        })
    } else {
        true
    };
    if !cleanup.is_empty() {
        client.write_batch(&cleanup)?;
        client.sync().await?;
    }
    client.sign_out().await?;
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    if prefix == "ATLET_ADMIN" {
        finish_open_web_orders(secrets).await?;
        cleanup_customer_a_cart(secrets).await?;
    }
    if !session_verified {
        bail!("browser session {title:?} absent from second Nostos device");
    }
    if !discarded_absent {
        bail!("signed-out browser write {discarded_title:?} reached the cloud");
    }
    if !product_verified {
        bail!("browser product {product_name:?} absent from second Nostos device");
    }
    if !order_verified {
        bail!("browser order {order_id:?} not delivered on second Nostos device");
    }
    Ok(())
}

async fn finish_open_web_orders(secrets: &Credentials) -> Result<()> {
    let endpoint = secrets.get("APPWRITE_ENDPOINT")?;
    let project = secrets.get("APPWRITE_PROJECT_ID")?;
    let jwt = sign_in(
        &reqwest::Client::new(),
        endpoint,
        project,
        secrets.get("ATLET_ADMIN_EMAIL")?,
        secrets.get("ATLET_ADMIN_PASSWORD")?,
        "atlet_admin_demo",
    )
    .await?;
    let path = std::env::temp_dir().join(format!("atlet-web-orders-{}.sqlite", Uuid::new_v4()));
    let storage = SqliteStorage::open(&path.to_string_lossy())?;
    let client = AppwriteDirectClient::new(endpoint, project, "atlet_sync", storage)?;
    client.set_user("atlet_admin_demo", &jwt).await?;
    client.sync().await?;
    for (from, to) in [("paid", "shipped"), ("shipped", "delivered")] {
        let writes = client
            .engine()
            .lock()
            .expect("Atlet orders mutex poisoned")
            .storage()
            .rows_for("orders")?
            .into_iter()
            .filter_map(|(pk, payload)| {
                let value = serde_json::from_slice::<Value>(&payload).ok()?;
                (value["status"] == from
                    && value["user_id"] == "atlet_user_a_demo"
                    && value["items_json"]
                        .as_str()
                        .is_some_and(|items| items.contains("Atlet web product ")))
                .then_some(PendingWrite {
                    table: "orders".into(),
                    op: WriteOp::Upsert,
                    pk,
                    payload_json: Some(json!({"status":to}).to_string()),
                })
            })
            .collect::<Vec<_>>();
        if !writes.is_empty() {
            client.write_batch(&writes)?;
            client.sync().await?;
        }
    }
    client.sign_out().await?;
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(())
}

async fn cleanup_customer_a_cart(secrets: &Credentials) -> Result<()> {
    // These are dedicated cloud demo accounts. A failed visual checkout can
    // leave a cart line after its catalog fixture is removed; clear it before
    // the next run so the next order tests only its own product.
    let endpoint = secrets.get("APPWRITE_ENDPOINT")?;
    let project = secrets.get("APPWRITE_PROJECT_ID")?;
    let jwt = sign_in(
        &reqwest::Client::new(),
        endpoint,
        project,
        secrets.get("ATLET_USER_A_EMAIL")?,
        secrets.get("ATLET_USER_A_PASSWORD")?,
        "atlet_user_a_demo",
    )
    .await?;
    let path = std::env::temp_dir().join(format!("atlet-web-cart-{}.sqlite", Uuid::new_v4()));
    let storage = SqliteStorage::open(&path.to_string_lossy())?;
    let client = AppwriteDirectClient::new(endpoint, project, "atlet_sync", storage)?;
    client.set_user("atlet_user_a_demo", &jwt).await?;
    client.sync().await?;
    let cart = client
        .engine()
        .lock()
        .expect("Atlet cart mutex poisoned")
        .storage()
        .rows_for("cart_items")?
        .into_iter()
        .map(|(pk, _)| PendingWrite {
            table: "cart_items".into(),
            op: WriteOp::Delete,
            pk,
            payload_json: None,
        })
        .collect::<Vec<_>>();
    if !cart.is_empty() {
        client.write_batch(&cart)?;
        client.sync().await?;
    }
    client.sign_out().await?;
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
    Ok(())
}

fn has_fvm() -> bool {
    Command::new("fvm")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn absolute(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn find_checkout() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("apps/atlet/flutter/pubspec.yaml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("run from inside a Nostos checkout");
        }
    }
}
