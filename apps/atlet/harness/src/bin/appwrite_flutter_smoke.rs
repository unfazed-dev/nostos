//! Launch the real Atlet Flutter UI test with disposable cloud credentials.

use std::{fs::OpenOptions, path::PathBuf, process::Command, time::Instant};

use anyhow::{bail, Context, Result};
use atlet_harness::auth::Credentials;
use clap::Parser;
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
    /// `basic` tests one role; `order` runs the admin and both buyers in one app.
    #[arg(long, default_value = "basic", value_parser = ["basic", "order"])]
    scenario: String,
    /// Ignored credentials file, never placed on the command line.
    #[arg(long, default_value = "apps/atlet/.env.cloud")]
    credentials: PathBuf,
    /// Directory for one JSON result per run. Contains no account credentials.
    #[arg(long, default_value = "apps/atlet/.results")]
    evidence_dir: PathBuf,
}

fn main() -> Result<()> {
    let args = nostos_infra::env::parse::<Args>();
    let root = find_checkout()?;
    let credentials_path = if args.credentials.is_absolute() {
        args.credentials.clone()
    } else {
        root.join(&args.credentials)
    };
    let secrets = Credentials::read(&credentials_path)?;
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
    let started = Instant::now();
    let mut command = Command::new(if use_fvm { "fvm" } else { "flutter" });
    if use_fvm {
        command.arg("flutter");
    }
    let output = command
        .arg("test")
        .arg(if args.scenario == "order" {
            "integration_test/appwrite_order_visual_test.dart"
        } else {
            "integration_test/appwrite_visual_smoke_test.dart"
        })
        .arg("-d")
        .arg(&args.device)
        .arg(format!("--dart-define-from-file={}", defines.display()))
        .current_dir(flutter)
        .output()
        .context("launch Flutter visual smoke")?;
    drop(guard);
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
    let evidence = json!({
        "run_id":run_id,
        "started_at":started_at.to_rfc3339(),
        "finished_at":chrono::Utc::now().to_rfc3339(),
        "duration_ms":started.elapsed().as_millis(),
        "commit":commit,
        "provider":"appwrite",
        "project_id":secrets.get("APPWRITE_PROJECT_ID")?,
        "function_id":"atlet_sync",
        "sdk":"nostos_flutter",
        "flutter_version":flutter_version,
        "device":args.device,
        "scenario":args.scenario,
        "role":args.role,
        "success":output.status.success() && convergence.is_some(),
        "exit_code":output.status.code(),
        "failure_reason":if !output.status.success() {Some("flutter_test_failed")} else if convergence.is_none() {Some("missing_convergence_evidence")} else {None},
        "convergence":convergence,
    });
    std::fs::write(&evidence_path, serde_json::to_vec_pretty(&evidence)?)?;
    println!("Evidence: {}", evidence_path.display());
    if !output.status.success() || convergence.is_none() {
        bail!("Flutter visual smoke failed on {}", args.device);
    }
    println!(
        "Appwrite Flutter visual smoke passed: {} / {} on {}",
        args.scenario, prefix, args.device
    );
    Ok(())
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
