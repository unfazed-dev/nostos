//! Integration tests for the `NOSTOS_ADMIN_TOKEN` gate on `PUT /rules`
//! (Task 21, ADR-0031 addendum).
//!
//! Mirrors `tests/put_rules.rs`: `nostos-server` is bin-only (no `[lib]`
//! target), so the only way to exercise the composition root's actual route
//! table is to spawn the compiled binary and drive it over real HTTP.

use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn server_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_nostos-server"))
}

/// An obviously-fake placeholder, never a real secret — just long enough to
/// clear the 32-char startup floor.
fn admin_token() -> String {
    format!("nostos-admin-auth-test-token-{}", "x".repeat(16))
}

/// A second, different token of the same shape — used to prove a *wrong*
/// bearer token (not merely a missing one) is rejected.
fn other_token() -> String {
    format!("nostos-admin-auth-test-token-{}", "y".repeat(16))
}

const SUPABASE_SECRET: &str = "test-secret-32-bytes-minimum!!!";

/// Mint an HS256 JWT carrying `{"sub": sub}` — mirrors
/// `crates/nostos-infra/tests/auth_sync.rs::mint_jwt`. A genuinely valid,
/// verifying sync-user JWT, not a forged one: the point of
/// `supabase_jwt_is_not_admin` is that even a *correct* application-user
/// credential is the wrong kind of credential for this route.
fn mint_supabase_jwt(sub: &str) -> String {
    let header = b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}";
    let payload = format!("{{\"sub\":\"{sub}\"}}");
    let h = base64url(header);
    let p = base64url(payload.as_bytes());
    let signing_input = format!("{h}.{p}");
    let mut mac = HmacSha256::new_from_slice(SUPABASE_SECRET.as_bytes()).unwrap();
    mac.update(signing_input.as_bytes());
    let sig = base64url(&mac.finalize().into_bytes());
    format!("{signing_input}.{sig}")
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(TABLE[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(TABLE[((buf << (6 - bits)) & 0x3F) as usize] as char);
    }
    out
}

struct Server {
    child: std::process::Child,
    base: String,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct SpawnOpts<'a> {
    tag: &'a str,
    admin_token: Option<&'a str>,
    sync_auth: &'a str,
    supabase_secret: Option<&'a str>,
    /// `NOSTOS_LOG` directive — e.g. `"error"` or `"error,nostos::audit=info"`.
    log: &'a str,
    /// When set, the child's stdout (where `tracing_subscriber::fmt()`
    /// writes by default) is redirected to this file instead of discarded.
    /// A file, not `Stdio::piped()`: a live child writing to a pipe nobody
    /// drains can block once the OS buffer fills — a file never blocks.
    log_file: Option<&'a std::path::Path>,
}

fn default_opts(tag: &str) -> SpawnOpts<'_> {
    SpawnOpts {
        tag,
        admin_token: None,
        sync_auth: "none",
        supabase_secret: None,
        log: "error",
        log_file: None,
    }
}

async fn spawn(opts: SpawnOpts<'_>) -> Server {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "nostos-server-admin-auth-it-{}-{}-{port}",
        opts.tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let rules_path = dir.join("nostos_rules.toml");

    let mut cmd = std::process::Command::new(server_binary());
    cmd.env("NOSTOS_BIND", format!("127.0.0.1:{port}"))
        .env("NOSTOS_REPLICATOR", "fake")
        .env("NOSTOS_RULES_FILE", &rules_path)
        .env("NOSTOS_SYNC_AUTH", opts.sync_auth)
        .env("NOSTOS_LOG", opts.log)
        // The parent test process's TERM/COLORTERM leak into the child via
        // Command's default env inheritance, so tracing_subscriber::fmt()
        // (init_tracing, main.rs — no .with_ansi(false)) emits ANSI-colored
        // log lines whenever a contributor's shell is color-capable. That
        // breaks audit_line_emitted_once_per_mutation's line.contains("x=")
        // checks (fields get wrapped in separate SGR escapes). NO_COLOR=1
        // is the documented tracing-subscriber override, and keeping the
        // fix here — not in init_tracing — is deliberate: it's a test
        // determinism concern, not a production logging behavior change.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());

    match opts.log_file {
        Some(path) => {
            cmd.stdout(std::fs::File::create(path).expect("create log file"));
        }
        None => {
            cmd.stdout(Stdio::null());
        }
    }
    if let Some(token) = opts.admin_token {
        cmd.env("NOSTOS_ADMIN_TOKEN", token);
    }
    if let Some(secret) = opts.supabase_secret {
        cmd.env("NOSTOS_SUPABASE_JWT_SECRET", secret);
    }

    let child = cmd.spawn().expect("spawn nostos-server");
    let base = format!("http://127.0.0.1:{port}");
    let server = Server { child, base, dir };

    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("{}/rules", server.base))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nostos-server did not become ready in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server
}

fn valid_body() -> serde_json::Value {
    serde_json::json!({
        "sync_mode": "toggles",
        "tables": [{"table": "tasks", "sync": true, "scope": "owner_id = claims.sub"}],
    })
}

#[tokio::test]
async fn put_rules_without_token_is_404() {
    // NOSTOS_ADMIN_TOKEN deliberately left unset: the route must behave as
    // if it were never mounted at all — 404, not 401 — regardless of what
    // credential the caller presents.
    let server = spawn(default_opts("no-token")).await;
    let client = reqwest::Client::new();

    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {}", admin_token()))
        .json(&valid_body())
        .send()
        .await
        .expect("PUT /rules");

    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn put_rules_with_wrong_token_is_401() {
    let token = admin_token();
    let mut opts = default_opts("wrong-token");
    opts.admin_token = Some(&token);
    let server = spawn(opts).await;
    let client = reqwest::Client::new();

    let wrong = other_token();
    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {wrong}"))
        .json(&valid_body())
        .send()
        .await
        .expect("PUT /rules");

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn supabase_jwt_is_not_admin() {
    let token = admin_token();
    let mut opts = default_opts("supabase-jwt-not-admin");
    opts.admin_token = Some(&token);
    opts.sync_auth = "supabase-jwt";
    opts.supabase_secret = Some(SUPABASE_SECRET);
    let server = spawn(opts).await;
    let client = reqwest::Client::new();

    let jwt = mint_supabase_jwt("attacker");
    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {jwt}"))
        .json(&valid_body())
        .send()
        .await
        .expect("PUT /rules");

    // A valid, verifying Supabase JWT — NOSTOS_SYNC_AUTH=supabase-jwt would
    // happily authenticate it on /sync — is still not the admin token: 401,
    // not 200. This is the test that stops the two auth systems from being
    // conflated.
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn short_admin_token_fails_startup() {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "nostos-server-admin-auth-it-short-token-{}-{port}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let rules_path = dir.join("nostos_rules.toml");

    let child = tokio::process::Command::new(server_binary())
        .env("NOSTOS_BIND", format!("127.0.0.1:{port}"))
        .env("NOSTOS_REPLICATOR", "fake")
        .env("NOSTOS_RULES_FILE", &rules_path)
        .env("NOSTOS_SYNC_AUTH", "none")
        .env("NOSTOS_LOG", "error")
        .env("NOSTOS_ADMIN_TOKEN", "short")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nostos-server");

    // The process should fail fast at the startup check, well before it
    // would ever attempt to bind — no readiness poll needed, just wait for
    // exit. `wait_with_output` drains stderr concurrently, so a short error
    // message can't deadlock even though the pipe is live.
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("nostos-server did not exit in time")
        .expect("wait_with_output");

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !output.status.success(),
        "a short NOSTOS_ADMIN_TOKEN must refuse to start"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("short"),
        "the token value must never appear in the startup error, got: {stderr}"
    );
}

#[tokio::test]
async fn put_rules_rejects_form_content_type() {
    let token = admin_token();
    let mut opts = default_opts("form-content-type");
    opts.admin_token = Some(&token);
    let server = spawn(opts).await;
    let client = reqwest::Client::new();

    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("sync_mode=toggles")
        .send()
        .await
        .expect("PUT /rules");

    assert_eq!(response.status(), 415);
}

#[tokio::test]
async fn audit_line_emitted_once_per_mutation() {
    let token = admin_token();
    let log_path = std::env::temp_dir().join(format!(
        "nostos-admin-auth-audit-log-{}-{}.log",
        std::process::id(),
        free_port()
    ));
    let mut opts = default_opts("audit-line");
    opts.admin_token = Some(&token);
    opts.log = "error,nostos::audit=info";
    opts.log_file = Some(&log_path);
    let server = spawn(opts).await;
    let client = reqwest::Client::new();

    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {token}"))
        .json(&valid_body())
        .send()
        .await
        .expect("PUT /rules");
    assert_eq!(response.status(), 200);

    // The audit line is written synchronously before the handler returns
    // the response, so it's already on disk by the time we get here — drop
    // the server anyway to force the file closed before we read it.
    drop(server);

    let mut contents = String::new();
    std::fs::File::open(&log_path)
        .expect("open log file")
        .read_to_string(&mut contents)
        .expect("read log file");
    let _ = std::fs::remove_file(&log_path);

    let mutation_lines: Vec<&str> = contents
        .lines()
        .filter(|l| l.contains("rules_mutation"))
        .collect();
    assert_eq!(
        mutation_lines.len(),
        1,
        "expected exactly one audit line, got log contents: {contents}"
    );
    assert!(
        !contents.contains(&token),
        "the admin token must never appear in the audit log, got: {contents}"
    );

    // Task 21 §3 shape: actor id, source, mode transition, checksum
    // transition, and the changed-table count must all be present, not just
    // the "rules_mutation" marker.
    let line = mutation_lines[0];
    assert!(line.contains("actor="), "missing actor=, got: {line}");
    assert!(line.contains("source="), "missing source=, got: {line}");
    assert!(
        line.contains("mode_before="),
        "missing mode_before=, got: {line}"
    );
    assert!(
        line.contains("mode_after="),
        "missing mode_after=, got: {line}"
    );
    assert!(
        line.contains("checksum_before=0x"),
        "missing checksum_before=0x, got: {line}"
    );
    assert!(
        line.contains("checksum_after=0x"),
        "missing checksum_after=0x, got: {line}"
    );
    assert!(
        line.contains("tables_changed="),
        "missing tables_changed=, got: {line}"
    );
}
