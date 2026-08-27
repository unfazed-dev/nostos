//! Shared harness for the nostos-tauri integration tests: spawns the no-docker
//! spine server (nostos-infra/examples/e2e_server) and drives raw-HTTP pushes.
//! Mirrors the helpers in src/lib.rs unit-test mod — integration tests cannot
//! reach into a #[cfg(test)] module, so the helpers are duplicated by design
//! (the alternative, a pub test-support feature on the production crate, would
//! ship test code in the plugin binary).

use nostos_tauri::NostosState;
use std::time::Duration;

/// Spawn the spine binary, discover its port via the NOSTOS_E2E_PORT stdout
/// line (the discovery contract every SDK E2E harness shares — see the
/// e2e_server.rs header). Blocks until NOSTOS_E2E_READY.
pub async fn spawn_spine() -> (u16, tokio::process::Child) {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let exe = spine_binary_path();
    if !exe.exists() {
        // This crate is a SEPARATE workspace from the root, so build the
        // spine against the ROOT workspace Cargo.toml.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .parent()
            .and_then(|p| p.parent())
            .expect("resolve root workspace from sdk/nostos_tauri");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "nostos-infra", "--example", "e2e_server"])
            .arg("--manifest-path")
            .arg(root.join("Cargo.toml"))
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
        };
        if line.trim() == "NOSTOS_E2E_READY" {
            break;
        }
    }
    // Keep the stdout pipe drained so the child is not SIGPIPEd.
    tokio::spawn(async move {
        while let Ok(Ok(Some(_))) =
            tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await
        {}
    });
    let port = port.expect("never saw NOSTOS_E2E_PORT");
    (port, child)
}

/// Minimal HTTP/1.1 POST /push over a raw TCP stream (no HTTP dep).
pub async fn http_push(port: u16, body: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// A connected + subscribed observer client on its own spine session — the
/// conformance suite fair "normal read path" (adapter.md fairness rule):
/// serverAcked marks derive from a SECOND client query(), which only sees
/// rows the server actually fanned out (never the writer local apply).
pub async fn observer(port: u16, tag: &str) -> NostosState {
    let state = NostosState::new();
    let db = std::env::temp_dir().join(format!(
        "nostos-tauri-conf-observer-{tag}-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);
    state
        .connect(
            Some(format!("ws://127.0.0.1:{port}/sync")),
            None,
            Some(db.to_str().expect("utf8").to_owned()),
        )
        .await
        .expect("observer connect");
    state
        .subscribe("tasks".into())
        .await
        .expect("observer subscribe");
    // Let the subscribe land + register with fan-out (the spine only delivers
    // to sessions registered at fan-out time).
    tokio::time::sleep(Duration::from_millis(500)).await;
    state
}

/// Poll state.query until at least min_rows rows matching pk_prefix exist in
/// cairn_data, or the deadline elapses. Returns the matching row count at
/// deadline (so callers assert on it).
pub async fn poll_rows_with_prefix(
    state: &NostosState,
    pk_prefix: &str,
    min_rows: usize,
    deadline: Duration,
) -> usize {
    let sql =
        format!("SELECT pk FROM cairn_data WHERE table_name = 'tasks' AND pk LIKE '{pk_prefix}%'");
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let rows_json = state.query(sql.clone()).await.expect("query");
        let rows: serde_json::Value = serde_json::from_str(&rows_json).expect("parse rows json");
        let n = rows.as_array().map_or(0, Vec::len);
        if n >= min_rows || tokio::time::Instant::now() >= end {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Resolve the built spine binary by walking up from CARGO_MANIFEST_DIR.
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
