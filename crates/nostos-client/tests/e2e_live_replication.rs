//! Live-replication E2E for the `nostos-client` SDK against the shared spine
//! server (`nostos-infra/examples/e2e_server`). Proves the SDK's real public API
//! drives a full server→client→server round-trip with NO Postgres and NO
//! docker:
//!
//! 1. **PUSH**: a `POST /push` injects a `tasks` row server-side → it replicates
//!    to the client over the real WS → the SDK applies it → `query()` reads it.
//! 2. **ECHO**: the SDK `write()`s a row to its durable outbox → the server's
//!    echo `WriteBack` accepts it and re-emits it through the fan-out → the
//!    writer receives its own write → `query()` reads it.
//!
//! This is the **reference shape** every downstream SDK live-E2E test copies:
//! the same two-direction round-trip against the same spine binary.

#![allow(clippy::uninlined_format_args)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{PendingWrite, WriteOp};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// A row the spine injects on `POST /push` (matches e2e_server_selftest's shape).
const PUSH_BODY: &str =
    r#"{"pk":"rust-push","payload":{"title":"from-server","status":"open","priority":"5"}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn sdk_live_round_trip_against_spine() {
    let (port, mut child) = spawn_spine().await;

    // Build the SDK client against the spine's WS endpoint. PID-unique DB path
    // so a stale file from a prior run can't yield a false positive.
    let db_path =
        std::env::temp_dir().join(format!("nostos-rust-e2e-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let storage =
        SqliteStorage::open(db_path.to_str().expect("utf8 db path")).expect("open sqlite");
    let url = format!("ws://127.0.0.1:{port}/sync");
    let config = SyncClientConfig {
        table: "tasks".into(),
        // Generous idle bound: keeps the session alive across subscribe → push →
        // echo while still bounding the test. The test finishes in ~2s and
        // aborts the run task; idle_timeout never fires in practice.
        idle_timeout: Some(Duration::from_secs(30)),
        ..SyncClientConfig::default()
    };
    let client = Arc::new(SyncClient::new(url, storage, config));

    // Drive the session in the background (subscribe + drain). run_once returns
    // when idle_timeout fires; we abort it after the assertions.
    let run_task = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            let _ = client.run_once().await;
        }
    });
    // Let the subscribe land + the session register with the fan-out service
    // (the spine only delivers to sessions registered at fan-out time).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- direction 1: server PUSH → on-device query ----
    http_push(port, PUSH_BODY).await;
    let pushed = poll_row(&client, "rust-push", Duration::from_secs(8))
        .await
        .expect("pushed row never became queryable");
    assert_eq!(pushed["pk"], "rust-push");
    eprintln!("[rust-e2e] PUSH_OK: {pushed:?}");

    // ---- direction 2: client WRITE → server echo → on-device query ----
    client
        .write(PendingWrite {
            table: "tasks".into(),
            op: WriteOp::Upsert,
            pk: "rust-echo".into(),
            payload_json: Some(r#"{"title":"from-client","status":"open","priority":"5"}"#.into()),
        })
        .await
        .expect("write");
    let echoed = poll_row(&client, "rust-echo", Duration::from_secs(8))
        .await
        .expect("echoed write never became queryable");
    assert_eq!(echoed["pk"], "rust-echo");
    eprintln!("[rust-e2e] ECHO_OK: {echoed:?}");

    run_task.abort();
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&db_path);
    println!("[rust-e2e] DONE");
}

/// Poll the on-device store for `pk` until a row appears or `deadline` elapses.
async fn poll_row(
    client: &SyncClient<SqliteStorage>,
    pk: &str,
    deadline: Duration,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let sql = format!("SELECT pk FROM cairn_data WHERE table_name = 'tasks' AND pk = '{pk}'");
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let sql = sql.clone();
        // `with_storage` returns a nested Result (outer ClientError, inner
        // StorageError) — flatten both before iterating the Vec<Map>.
        let rows = client
            .with_storage(move |s| s.query(&sql))
            .await
            .expect("with_storage")
            .expect("query");
        if let Some(row) = rows.into_iter().next() {
            return Some(row);
        }
        if tokio::time::Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Minimal HTTP/1.1 POST /push over a raw TCP stream — no HTTP dep (the spine's
/// control endpoint is localhost-only).
async fn http_push(port: u16, body: &str) {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect spine");
    let req = format!(
        "POST /push HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
    let head = String::from_utf8_lossy(&buf[..buf.len().min(40)]);
    assert!(head.contains("200"), "POST /push non-200: {head}");
}

/// Spawn the spine binary, discover its port via the `NOSTOS_E2E_PORT` stdout
/// line. Mirrors `e2e_server_selftest`'s spawn + ancestor-walking path lookup.
async fn spawn_spine() -> (u16, tokio::process::Child) {
    let exe = spine_binary_path();
    if !exe.exists() {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "nostos-infra", "--example", "e2e_server"])
            .status()
            .expect("cargo build spine");
        assert!(status.success(), "build spine failed");
    }
    let exe = spine_binary_path();
    assert!(exe.exists(), "spine binary not found at {}", exe.display());

    let mut child = Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn spine {}: {e}", exe.display()));

    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut port: Option<u16> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let line = match tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None) | Err(_)) => break,
            Err(_) => continue,
        };
        if let Some(rest) = line.strip_prefix("NOSTOS_E2E_PORT=") {
            port = rest.trim().parse::<u16>().ok();
        }
        if line.trim() == "NOSTOS_E2E_READY" {
            break;
        }
    }
    // Keep the stdout pipe open (drain in bg) so the child isn't SIGPIPE'd.
    tokio::spawn(async move {
        while let Ok(Ok(Some(_))) =
            tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await
        {}
    });
    let port = port.expect("never saw NOSTOS_E2E_PORT");
    eprintln!("[rust-e2e] spine on port {port}");
    (port, child)
}

/// Resolve the built spine binary. Workspace members share the workspace-root
/// `target/`, so walk up from `CARGO_MANIFEST_DIR` to find it.
fn spine_binary_path() -> std::path::PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let rel = std::path::Path::new("target")
        .join(profile)
        .join("examples")
        .join("e2e_server");
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut dir: Option<&std::path::Path> = Some(manifest);
    while let Some(d) = dir {
        let candidate = d.join(&rel);
        if candidate.exists() {
            return candidate;
        }
        dir = d.parent();
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(&rel);
        if candidate.exists() {
            return candidate;
        }
    }
    manifest.join(&rel)
}
