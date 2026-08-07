//! Integration test for `PUT /rules` (Task 20).
//!
//! `nostos-server` is bin-only (no `[lib]` target — see `Cargo.toml`), so the
//! only way to exercise the composition root's actual route table is to spawn
//! the compiled binary as a child process and drive it over real HTTP, the
//! same way an operator or the Task 21 web panel would.

use std::process::Stdio;
use std::time::Duration;

/// ponytail: pick a free port here, then hand it to the child via
/// `NOSTOS_BIND` — a TOCTOU race is possible (something else grabs the port
/// between the pick and the child's bind), acceptable for a single-machine
/// test run. A collision surfaces as a flaky "connection refused", not a
/// false pass.
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

/// Task 21 gated `PUT /rules` behind `NOSTOS_ADMIN_TOKEN` — an obviously-fake
/// placeholder, never a real secret, just long enough to clear the 32-char
/// startup floor.
fn admin_token() -> &'static str {
    "nostos-put-rules-test-admin-token-0000"
}

struct Server {
    child: std::process::Child,
    base: String,
    rules_path: std::path::PathBuf,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawns `nostos-server` in fake-replicator/no-auth mode against a fresh
/// throwaway `nostos_rules.toml`, and waits for it to answer `GET /rules`.
async fn spawn(tag: &str) -> Server {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "nostos-server-put-rules-it-{tag}-{}-{port}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let rules_path = dir.join("nostos_rules.toml");

    let child = std::process::Command::new(server_binary())
        .env("NOSTOS_BIND", format!("127.0.0.1:{port}"))
        .env("NOSTOS_REPLICATOR", "fake")
        .env("NOSTOS_RULES_FILE", &rules_path)
        .env("NOSTOS_SYNC_AUTH", "none")
        .env("NOSTOS_LOG", "error")
        .env("NOSTOS_ADMIN_TOKEN", admin_token())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn nostos-server");

    let base = format!("http://127.0.0.1:{port}");
    let server = Server {
        child,
        base,
        rules_path,
        dir,
    };

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

#[tokio::test]
async fn put_rules_over_http_persists_and_swaps() {
    let server = spawn("write-and-swap").await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "sync_mode": "toggles",
        "tables": [{"table": "tasks", "sync": true, "scope": "owner_id = claims.sub"}],
    });
    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {}", admin_token()))
        .json(&body)
        .send()
        .await
        .expect("PUT /rules");
    assert_eq!(response.status(), 200);
    let put_body: serde_json::Value = response.json().await.expect("parse PUT response");
    assert_eq!(put_body["sync_mode"], "toggles");

    // File on disk reflects the write.
    let on_disk = std::fs::read_to_string(&server.rules_path).expect("read rules file");
    assert!(on_disk.contains("sync_mode = \"toggles\""));
    assert!(on_disk.contains("[tables.tasks]"));

    // GET /rules proves the in-process ruleset swapped too (same checksum,
    // no restart needed).
    let get_response = client
        .get(format!("{}/rules", server.base))
        .send()
        .await
        .expect("GET /rules");
    let get_body: serde_json::Value = get_response.json().await.expect("parse GET response");
    assert_eq!(get_body["checksum"], put_body["checksum"]);
    assert_eq!(get_body["sync_mode"], "toggles");
}

/// Same as `spawn`, plus `NOSTOS_CORS_ORIGINS` — exercises the explicit-
/// origins CORS branch instead of the empty-default `permissive()` path.
async fn spawn_with_cors(tag: &str, cors_origin: &str) -> Server {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "nostos-server-put-rules-it-{tag}-{}-{port}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let rules_path = dir.join("nostos_rules.toml");

    let child = std::process::Command::new(server_binary())
        .env("NOSTOS_BIND", format!("127.0.0.1:{port}"))
        .env("NOSTOS_REPLICATOR", "fake")
        .env("NOSTOS_RULES_FILE", &rules_path)
        .env("NOSTOS_SYNC_AUTH", "none")
        .env("NOSTOS_LOG", "error")
        .env("NOSTOS_ADMIN_TOKEN", admin_token())
        .env("NOSTOS_CORS_ORIGINS", cors_origin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn nostos-server");

    let base = format!("http://127.0.0.1:{port}");
    let server = Server {
        child,
        base,
        rules_path,
        dir,
    };

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

/// C1 follow-up regression: the explicit-origins CORS branch
/// (`crates/nostos-server/src/main.rs`, `build_cors_layer`) omitted
/// `Method::PUT` from `allow_methods`, so the moment an operator followed
/// `docs/OPERATING.md`'s production guidance and set `NOSTOS_CORS_ORIGINS`,
/// a real browser's preflight for the admin panel's own `PUT /rules` save
/// would see `PUT` missing from `Access-Control-Allow-Methods` and refuse to
/// send the real request — even though the route itself is reachable and
/// correctly gated (see `admin_auth.rs`). This drives the exact preflight a
/// browser issues before `web/src/routes/admin/rules/+page.svelte`'s Save
/// button can fire, then the real PUT with the `Origin` header a browser
/// would attach.
#[tokio::test]
async fn put_rules_preflight_allows_put_under_configured_cors_origin() {
    let origin = "http://admin-panel.example.test";
    let server = spawn_with_cors("cors-preflight", origin).await;
    let client = reqwest::Client::new();

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{}/rules", server.base))
        .header("Origin", origin)
        .header("Access-Control-Request-Method", "PUT")
        .header(
            "Access-Control-Request-Headers",
            "authorization, content-type",
        )
        .send()
        .await
        .expect("OPTIONS preflight");

    assert!(
        preflight.status().is_success(),
        "preflight rejected: {}",
        preflight.status()
    );
    let allow_methods = preflight
        .headers()
        .get("access-control-allow-methods")
        .expect("Access-Control-Allow-Methods header present")
        .to_str()
        .expect("header is ASCII")
        .to_string();
    assert!(
        allow_methods.contains("PUT"),
        "PUT missing from Access-Control-Allow-Methods: {allow_methods}"
    );

    // The real PUT, with the Origin header a browser would attach, must both
    // succeed and come back with CORS headers a browser would accept.
    let body = serde_json::json!({"sync_mode": "toggles", "tables": []});
    let put_response = client
        .put(format!("{}/rules", server.base))
        .header("Origin", origin)
        .header("Authorization", format!("Bearer {}", admin_token()))
        .json(&body)
        .send()
        .await
        .expect("PUT /rules");

    assert_eq!(put_response.status(), 200);
    assert_eq!(
        put_response
            .headers()
            .get("access-control-allow-origin")
            .expect("Access-Control-Allow-Origin present")
            .to_str()
            .expect("header is ASCII"),
        origin
    );
}

#[tokio::test]
async fn put_rules_rejects_hand_mode_over_http() {
    let server = spawn("reject-hand").await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({"sync_mode": "hand", "tables": []});
    let response = client
        .put(format!("{}/rules", server.base))
        .header("Authorization", format!("Bearer {}", admin_token()))
        .json(&body)
        .send()
        .await
        .expect("PUT /rules");

    assert_eq!(response.status(), 422);
    let err: serde_json::Value = response.json().await.expect("parse error response");
    assert!(err["error"].as_str().unwrap().contains("hand"));

    // Nothing touched: the file was never created (server starts with no
    // `nostos_rules.toml`, defaulting to `all` mode).
    assert!(!server.rules_path.exists());
}
