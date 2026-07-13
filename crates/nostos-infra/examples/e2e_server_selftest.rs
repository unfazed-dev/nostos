//! # e2e_server_selftest — round-trip proof for the SDK live-E2E spine.
//!
//! Spawns `e2e_server` as a child process, discovers its port via
//! `NOSTOS_E2E_PORT`, opens a real WS session, and proves BOTH directions:
//!
//! 1. **PUSH**: subscribe → `POST /push` → assert the replicated row arrives
//!    on the WS → print `[spine] PUSH_OK`.
//! 2. **ECHO**: send a `write` frame → assert the echoed WireFrame arrives on
//!    the same WS (the writer sees its own write via the replication path) →
//!    print `[spine] ECHO_OK`.
//!
//! Exits 0 iff both directions proven; non-zero otherwise. This is the same
//! shape every downstream SDK E2E will run against the spine binary — the
//! selftest IS the reference SDK client.

// Presentation/format lints trip on incidental shape in a throwaway dev
// fixture — mirroring nostos-bench's allow for the same reason.
#![allow(clippy::uninlined_format_args)]

use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::tungstenite::Message;

use nostos_infra::wire::{decode_frames, WireFrame};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- locate + spawn the e2e_server binary ----
    // Workspace members share the root `target/`, so resolve via the
    // ancestor-walking helper; if it isn't built yet, build it, then re-resolve
    // (the build lands at the workspace-root target, found on the second pass).
    let mut exe = example_binary_path("e2e_server");
    if !exe.exists() {
        eprintln!(
            "[spine] e2e_server not found at {}; building…",
            exe.display()
        );
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "nostos-infra", "--example", "e2e_server"])
            .status()
            .map_err(|e| format!("cargo build failed to start: {e}"))?;
        if !status.success() {
            return Err(format!(
                "cargo build -p nostos-infra --example e2e_server failed (status {status})"
            )
            .into());
        }
        exe = example_binary_path("e2e_server");
    }

    let mut child = tokio::process::Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))?;

    // ---- discover the port + READY line from stdout ----
    let stdout = child
        .stdout
        .take()
        .ok_or("child stdout was not piped".to_string())?;
    let mut lines = BufReader::new(stdout).lines();
    let mut port: Option<u16> = None;
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < ready_deadline {
        // `next_line()` returns io::Result<Option<String>>; `timeout(...).await`
        // wraps that in another Result<_, Elapsed>. Unpack both layers.
        let line = match tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None) | Err(_)) => break, // EOF or io error
            Err(_) => continue,             // per-line timeout — keep waiting
        };
        if let Some(rest) = line.strip_prefix("NOSTOS_E2E_PORT=") {
            port = rest.trim().parse::<u16>().ok();
        }
        if line.trim() == "NOSTOS_E2E_READY" {
            break;
        }
    }
    let port = port.ok_or("never saw NOSTOS_E2E_PORT=<port>".to_string())?;
    eprintln!("[spine] server up on port {port}");

    // Hold the stdout reader task across the session so the pipe doesn't
    // close and SIGPIPE the child. The READY line has already been consumed;
    // drain any further lines quietly.
    let _drain = tokio::spawn(async move {
        // Discard — the spine only emits the two discovery lines (+ run-time
        // tracing goes to stderr, not stdout, so this drains nothing in
        // practice; the task exists to keep the pipe open).
        while let Ok(Ok(Some(_))) =
            tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await
        {}
    });

    // ---- connect WS to /sync (AllowAnonymous → no token needed) ----
    let url = format!("ws://127.0.0.1:{port}/sync");
    let ws = retry_connect(&url, 50, Duration::from_millis(20))
        .await
        .ok_or("WS connect failed after retries".to_string())?;
    let (mut write, mut read) = ws.split();

    // Subscribe FIRST — FanOut delivers only to sessions registered at
    // fan-out time, so /push before the subscribe lands would be missed.
    let sub = serde_json::json!({"type":"subscribe","table":"tasks"}).to_string();
    write.send(Message::Text(sub)).await?;
    // Settle: the server registers the session in the store asynchronously.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ---- PUSH: POST /push, assert the row arrives on the WS ----
    let http_url = format!("http://127.0.0.1:{port}/push");
    let push_body = serde_json::json!({
        "pk": "push-1",
        "payload": {"title": "from-server", "status": "open", "priority": "5"}
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = client.post(&http_url).json(&push_body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("POST /push failed: {status} {body}").into());
    }

    let Some(pushed) = wait_for_frame(&mut read, "push-1", Duration::from_secs(5)).await? else {
        return Err("never received the pushed row (push-1)".into());
    };
    eprintln!(
        "[spine] pushed row received: lsn={} op={:?} pk={}",
        pushed.lsn, pushed.op, pushed.pk
    );
    println!("[spine] PUSH_OK");

    // ---- ECHO: send a write frame, assert the echoed insert arrives ----
    let write_frame = serde_json::json!({
        "type": "write",
        "table": "tasks",
        "op": "upsert",
        "pk": "echo-1",
        "payload": {"title": "from-client", "status": "open", "priority": "5"},
        "client_write_id": "w1"
    })
    .to_string();
    write.send(Message::Text(write_frame)).await?;

    let Some(echoed) = wait_for_frame(&mut read, "echo-1", Duration::from_secs(5)).await? else {
        return Err("never received the echoed write (echo-1)".into());
    };
    eprintln!(
        "[spine] echoed write received: lsn={} op={:?} pk={}",
        echoed.lsn, echoed.op, echoed.pk
    );
    println!("[spine] ECHO_OK");

    // ---- clean shutdown ----
    let _ = write.close().await;
    child.kill().await.ok();
    let _ = child.wait().await;
    println!("[spine] DONE");
    Ok(())
}

/// Resolve a built example binary. Workspace members share the workspace-root
/// `target/`, so the binary is NOT under `CARGO_MANIFEST_DIR/target` — it lives
/// under an ancestor's. Walk up from the manifest dir (and also try CWD, for
/// `cargo run` invoked from the workspace root) and return the first existing
/// copy; if none exists yet, fall back to the manifest-local path so the
/// caller's not-found + auto-build branch triggers and re-resolves post-build.
fn example_binary_path(name: &str) -> std::path::PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let rel = std::path::Path::new("target")
        .join(profile)
        .join("examples")
        .join(name);
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found: Option<std::path::PathBuf> = None;
    let mut dir: Option<&std::path::Path> = Some(manifest);
    while let Some(d) = dir {
        let candidate = d.join(&rel);
        if candidate.exists() {
            found = Some(candidate);
            break;
        }
        dir = d.parent();
    }
    if found.is_none() {
        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd.join(&rel);
            if candidate.exists() {
                found = Some(candidate);
            }
        }
    }
    found.unwrap_or_else(|| manifest.join(&rel))
}

/// Retry `connect_async` up to `attempts` times — the server is starting
/// concurrently and may not have called `axum::serve` yet at the instant we
/// read `NOSTOS_E2E_PORT` (the listener is bound; the router is being plumbed).
async fn retry_connect(
    url: &str,
    attempts: u32,
    backoff: Duration,
) -> Option<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    for _ in 0..attempts {
        if let Ok((stream, _)) = tokio_tungstenite::connect_async(url).await {
            return Some(stream);
        }
        tokio::time::sleep(backoff).await;
    }
    None
}

/// Read WS messages until we decode a `WireFrame` with `pk == wanted_pk`, or
/// `deadline` elapses. Non-WireFrame messages (e.g. the `write_result` ack,
/// which has no `op` field and so fails `WireFrame` deserialization) are
/// silently dropped by `decode_frames` — they don't interfere with the match.
async fn wait_for_frame<S>(
    read: &mut S,
    wanted_pk: &str,
    deadline: Duration,
) -> Result<Option<WireFrame>, Box<dyn std::error::Error>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        let remaining = end - tokio::time::Instant::now();
        let next = tokio::time::timeout(remaining, read.next()).await;
        let Some(msg) = next? else { break };
        // Decode frames directly off each message variant — no intermediate
        // Vec<u8> allocation. `decode_frames` returns Vec<WireFrame> by value.
        let frames = match msg? {
            Message::Text(s) => decode_frames(s.as_bytes()),
            Message::Binary(b) => decode_frames(b.as_ref()),
            Message::Close(_) => break,
            _ => continue,
        };
        for frame in frames {
            if frame.pk == wanted_pk {
                return Ok(Some(frame));
            }
        }
    }
    Ok(None)
}
