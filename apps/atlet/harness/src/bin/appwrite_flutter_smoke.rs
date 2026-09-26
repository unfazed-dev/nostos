//! Launch the real Atlet Flutter UI test with disposable cloud credentials.

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use atlet_harness::auth::{sign_in, Credentials};
use clap::Parser;
use nostos_client::{appwrite::AppwriteDirectClient, SqliteStorage};
use nostos_core::{PendingWrite, WriteOp};
use serde_json::json;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    /// Flutter target from `flutter devices`, for example macos or chrome.
    #[arg(long, default_value = "macos")]
    device: String,
    /// Real demo account to drive through the same visual app.
    #[arg(long, default_value = "customer_a", value_parser = ["admin", "customer_a", "customer_b"])]
    role: String,
    /// `basic` tests one role; `order` runs all roles; `revoked` checks a fresh inactive account.
    #[arg(long, default_value = "basic", value_parser = ["basic", "order", "revoked"])]
    scenario: String,
    /// Ignored credentials file, never placed on the command line.
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    /// Directory for one JSON result per run. Contains no account credentials.
    #[arg(long, default_value = "apps/atlet/.results")]
    evidence_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let root = find_checkout()?;
    let credentials_path = if args.credentials.is_absolute() {
        args.credentials.clone()
    } else {
        root.join(&args.credentials)
    };
    let secrets = Credentials::read(&credentials_path)?;
    if args.scenario == "revoked" && args.role != "customer_b" {
        bail!("revoked scenario requires --role customer_b");
    }
    let prefix = match args.role.as_str() {
        "admin" => "ATLET_ADMIN",
        "customer_a" => "ATLET_USER_A",
        "customer_b" => "ATLET_USER_B",
        _ => unreachable!("clap restricts role"),
    };
    let run_id = Uuid::new_v4().simple().to_string();
    let started_at = chrono::Utc::now();
    let mut values = json!({
        "ATLET_PROVIDER":"appwrite",
        "APPWRITE_ENDPOINT":secrets.get("APPWRITE_ENDPOINT")?,
        "APPWRITE_PROJECT_ID":secrets.get("APPWRITE_PROJECT_ID")?,
        "ATLET_TEST_EMAIL":secrets.get(&format!("{prefix}_EMAIL"))?,
        "ATLET_TEST_PASSWORD":secrets.get(&format!("{prefix}_PASSWORD"))?,
        "ATLET_TEST_ROLE":args.role,
        "ATLET_TEST_MANUAL_CONNECTIVITY":"true",
        "ATLET_TEST_DB_SUFFIX":run_id,
    });
    if args.scenario == "order" {
        // Supabase's pilot flag must not suppress Appwrite foreground banners
        // or initialize Firebase for an Appwrite build.
        values["ATLET_PUSH_PILOT"] = json!("true");
        for (role, prefix) in [
            ("ADMIN", "ATLET_ADMIN"),
            ("USER_A", "ATLET_USER_A"),
            ("USER_B", "ATLET_USER_B"),
        ] {
            values[format!("ATLET_TEST_{role}_EMAIL")] =
                json!(secrets.get(&format!("{prefix}_EMAIL"))?);
            values[format!("ATLET_TEST_{role}_PASSWORD")] =
                json!(secrets.get(&format!("{prefix}_PASSWORD"))?);
        }
    }
    let flutter = root.join("apps/atlet/flutter");
    let defines = std::env::temp_dir().join(format!("atlet-defines-{}.json", Uuid::new_v4()));
    let guard = RemoveOnDrop(defines.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&defines)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, &values)?;
    drop(file);
    let use_fvm = Command::new("fvm").arg("--version").output().is_ok();
    let mut flutter_command = Command::new(if use_fvm { "fvm" } else { "flutter" });
    if use_fvm {
        flutter_command.arg("flutter");
    }
    let flutter_version = flutter_command
        .arg("--version")
        .arg("--machine")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
        .and_then(|version| version["frameworkVersion"].as_str().map(str::to_owned));
    let revocation = if args.scenario == "revoked" {
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
        let path = std::env::temp_dir().join(format!("atlet-revoked-{run_id}.sqlite3"));
        let storage = SqliteStorage::open(&path.to_string_lossy())?;
        let admin = AppwriteDirectClient::new(endpoint, project, "atlet_sync", storage)?;
        admin.set_user("atlet_admin_demo", &jwt).await?;
        admin
            .sync()
            .await
            .context("bootstrap admin before revocation")?;
        // Recover a shared account left inactive by an interrupted prior run.
        set_customer_b_active(&admin, true).await?;
        if let Err(error) = set_customer_b_active(&admin, false).await {
            let _ = set_customer_b_active(&admin, true).await;
            return Err(error).context("disable customer B for revoked visual test");
        }
        Some((admin, path))
    } else {
        None
    };
    let started = Instant::now();
    let mut command = Command::new(if use_fvm { "fvm" } else { "flutter" });
    if use_fvm {
        command.arg("flutter");
    }
    let output = command
        .arg("test")
        .arg(match args.scenario.as_str() {
            "order" => "integration_test/appwrite_order_visual_test.dart",
            "revoked" => "integration_test/appwrite_revoked_visual_test.dart",
            _ => "integration_test/appwrite_visual_smoke_test.dart",
        })
        .arg("-d")
        .arg(&args.device)
        .arg(format!("--dart-define-from-file={}", defines.display()))
        .current_dir(&flutter)
        .output();
    drop(guard);
    if let Some((admin, path)) = revocation {
        // Restore the shared demo account even when the Flutter test fails.
        let restored = set_customer_b_active(&admin, true).await;
        let _ = admin.sign_out().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        restored.context("restore customer B after revoked visual test")?;
    }
    let output = output.context("launch Flutter visual smoke")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    let convergence = stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            line.split_once("ATLET_EVIDENCE:")
                .map(|(_, data)| data.trim())
        })
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .next_back();
    let visual_error_count = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| {
            line.contains("Appwrite role refresh failed:") || line.contains("order banner failed:")
        })
        .count();
    let evidence_dir = if args.evidence_dir.is_absolute() {
        args.evidence_dir
    } else {
        root.join(args.evidence_dir)
    };
    std::fs::create_dir_all(&evidence_dir)?;
    let evidence_path = evidence_dir.join(format!("appwrite-flutter-{run_id}.json"));
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    let source_dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty());
    let source_ref = commit.as_ref().map(|head| {
        if source_dirty == Some(true) {
            format!("{head}+working-tree")
        } else {
            head.clone()
        }
    });
    let error_count = visual_error_count
        + usize::from(!output.status.success())
        + usize::from(convergence.is_none());
    let evidence = json!({
        "run_id":run_id,
        "started_at":started_at.to_rfc3339(),
        "finished_at":chrono::Utc::now().to_rfc3339(),
        "duration_ms":started.elapsed().as_millis(),
        "commit":commit,
        "source_ref":source_ref,
        "source_dirty":source_dirty,
        "app_version":pubspec_version(&flutter.join("pubspec.yaml"))?,
        "sdk_version":pubspec_version(&root.join("sdk/nostos_flutter/pubspec.yaml"))?,
        "fixture":{"id":run_id,"seed":run_id,"accounts":"atlet_admin_demo,atlet_user_a_demo,atlet_user_b_demo"},
        "provider":"appwrite",
        "project_id":secrets.get("APPWRITE_PROJECT_ID")?,
        "function_id":"atlet_sync",
        "sdk":"nostos_flutter",
        "flutter_version":flutter_version,
        "device":args.device,
        "scenario":args.scenario,
        "role":args.role,
        "success":output.status.success() && convergence.is_some() && visual_error_count == 0,
        "exit_code":output.status.code(),
        "error_count":error_count,
        "visual_error_count":visual_error_count,
        "failure_reason":if !output.status.success() {Some("flutter_test_failed")} else if convergence.is_none() {Some("missing_convergence_evidence")} else if visual_error_count > 0 {Some("visual_ui_error")} else {None},
        "convergence":convergence,
    });
    std::fs::write(&evidence_path, serde_json::to_vec_pretty(&evidence)?)?;
    println!("Evidence: {}", evidence_path.display());
    if !output.status.success() || convergence.is_none() || visual_error_count > 0 {
        bail!("Flutter visual smoke failed on {}", args.device);
    }
    println!(
        "Appwrite Flutter visual smoke passed: {} / {} on {}",
        args.scenario, prefix, args.device
    );
    Ok(())
}

async fn set_customer_b_active(client: &AppwriteDirectClient, active: bool) -> Result<()> {
    client.write_batch(&[PendingWrite {
        table: "user_profiles".into(),
        op: WriteOp::Upsert,
        pk: "atlet_user_b_demo".into(),
        payload_json: Some(json!({"active":active}).to_string()),
    }])?;
    client.sync().await?;
    Ok(())
}

fn pubspec_version(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)?
        .lines()
        .find_map(|line| line.strip_prefix("version:").map(str::trim))
        .map(str::to_owned)
        .with_context(|| format!("version missing from {}", path.display()))
}

fn find_checkout() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("find Nostos checkout")?;
    loop {
        if dir.join("apps/atlet/flutter/pubspec.yaml").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("run from inside a Nostos checkout");
        }
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
