//! HTTP-level integration tests for the daemon (plan task 1.8 — the gate).
//!
//! Provider mocking: the rails' test-pointing constructors are private to
//! nostos-infra, so per the sanctioned alternative this suite drives the
//! coalescer/dispatch through the crate's own RailDispatch seam with an
//! in-memory mock (scriptable outcomes + a call counter). The daemon is a
//! REAL spawned axum listener on 127.0.0.1:0 — the ProviderMock fixture
//! idiom — exercised over reqwest, so headers, middleware, status codes and
//! JSON shapes are all end-to-end.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nostos_infra::push::{PushPayload, RailOutcome};
use nostos_push::auth::ApiKeys;
use nostos_push::coalescer::{self, CoalescerLimits};
use nostos_push::limit::SendRateLimiter;
use nostos_push::rail::{RailDispatch, Rails};
use nostos_push::store::{Platform, SqliteStore, Store};
use nostos_push::{build_router, AppState};
use serde_json::{json, Value};

const KEY_A: &str = "test-secret-a";
const KEY_B: &str = "test-secret-b";
/// A Rail-role key (the ":rail" suffix in NOSTOS_PUSHD_API_KEYS) — the only
/// role that may use rail-mode dispatch (plan task 4.1, finding 1).
const KEY_RAIL: &str = "test-secret-rail";
const TOKEN_A: &str = "apns-token-0123456789abcdef0123456789abcdef";
const TOKEN_B: &str = "fcm-token-0123456789abcdef0123456789abcdef";

/// In-memory rail: counts every dispatch, replays scripted outcomes first,
/// then falls back to a default. implements the nostos-push seam — NOT a
/// modification of nostos-infra.
struct MockRail {
    calls: Arc<Mutex<usize>>,
    scripted: Mutex<VecDeque<RailOutcome>>,
    default_outcome: RailOutcome,
}

impl MockRail {
    fn new(default_outcome: RailOutcome) -> (Arc<Mutex<usize>>, Arc<Self>) {
        let calls = Arc::new(Mutex::new(0));
        (
            Arc::clone(&calls),
            Arc::new(Self {
                calls,
                scripted: Mutex::new(VecDeque::new()),
                default_outcome,
            }),
        )
    }

    /// Script outcomes replayed before the default (consumes the Arc so
    /// the call chain reads as construction).
    fn scripted(self: Arc<Self>, outcomes: Vec<RailOutcome>) -> Arc<Self> {
        *self.scripted.lock().expect("script") = outcomes.into();
        self
    }
}

#[async_trait]
impl RailDispatch for MockRail {
    async fn send(
        &self,
        _token: &str,
        _collapse_key: Option<&str>,
        _payload: &PushPayload,
    ) -> RailOutcome {
        *self.calls.lock().expect("calls") += 1;
        self.scripted
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| self.default_outcome.clone())
    }
}

/// A live daemon bound to an ephemeral port.
struct Daemon {
    addr: SocketAddr,
    client: reqwest::Client,
}

impl Daemon {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn post(
        &self,
        path: &str,
        key: Option<&str>,
        body: &Value,
    ) -> (reqwest::StatusCode, Value) {
        let mut req = self.client.post(self.url(path));
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        let resp = req.json(body).send().await.expect("request");
        let status = resp.status();
        (status, resp.json().await.expect("json body"))
    }

    async fn delete(&self, path: &str, key: &str) -> reqwest::StatusCode {
        self.client
            .delete(self.url(path))
            .bearer_auth(key)
            .send()
            .await
            .expect("request")
            .status()
    }

    async fn get(&self, path: &str, key: &str) -> (reqwest::StatusCode, Value) {
        let resp = self
            .client
            .get(self.url(path))
            .bearer_auth(key)
            .send()
            .await
            .expect("request");
        let status = resp.status();
        (status, resp.json().await.expect("json body"))
    }
}

/// Bind a daemon: in-memory store, the given rails, the given debounce,
/// production-default rate limits and coalescer ceilings.
async fn spawn_daemon(debounce_ms: u64, rails: Rails) -> Daemon {
    spawn_daemon_tuned(debounce_ms, rails, 10, 50, CoalescerLimits::default()).await
}

/// Bind a daemon with explicit rate/ceiling knobs — the audit tests run
/// with tiny values (plan task 4.1: ceilings must be injectable).
async fn spawn_daemon_tuned(
    debounce_ms: u64,
    rails: Rails,
    rate_per_sec: u32,
    send_burst: u32,
    limits: CoalescerLimits,
) -> Daemon {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().expect("store"));
    let coalescer = coalescer::spawn_coalescer(
        Arc::clone(&store),
        rails.clone(),
        Duration::from_millis(debounce_ms),
        limits,
    );
    let state = AppState {
        store,
        rails,
        api_keys: ApiKeys::parse(&format!(
            "tenant-a:{KEY_A},tenant-b:{KEY_B},tenant-r:{KEY_RAIL}:rail"
        ))
        .expect("test keys"),
        sender: coalescer.tx.clone(),
        send_limiter: Arc::new(SendRateLimiter::new(rate_per_sec, send_burst)),
        gate: coalescer.gate.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, build_router(state))
            .await
            .expect("serve");
    });
    Daemon {
        addr,
        client: reqwest::Client::new(),
    }
}

/// Rails with exactly one platform wired to the mock; the others off.
fn rails_for(platform: Platform, mock: &Arc<MockRail>) -> Rails {
    let mock: Arc<dyn RailDispatch> = mock.clone();
    let (apns, fcm, webpush) = match platform {
        Platform::Apns => (Some(mock), None, None),
        Platform::Fcm => (None, Some(mock), None),
        Platform::Webpush => (None, None, Some(mock)),
    };
    Rails::new(apns, fcm, webpush)
}

/// Register TOKEN_A (apns) under the key's tenant; assert 201.
async fn register_apns(d: &Daemon, key: &str, token: &str) {
    let (status, body) = d
        .post(
            "/v1/tokens",
            Some(key),
            &json!({"token": token, "platform": "apns", "account_tag": "acct-1"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "register: {body}");
    assert_eq!(body["registered"], json!(true));
}

/// One silent send; returns the push_id.
async fn send_silent(d: &Daemon, key: &str, token: &str, metadata: Value) -> String {
    let (status, body) = d
        .post(
            "/v1/send",
            Some(key),
            &json!({
                "token": token,
                "payload": {"silent": {"table": "docs", "lsn": "4242"}},
                "metadata": metadata,
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "send: {body}");
    assert_eq!(body["status"], json!("accepted"));
    body["push_id"].as_str().expect("push_id").to_string()
}

/// Poll the receipt log until it holds at least `n` rows for this key.
async fn wait_for_receipts(d: &Daemon, key: &str, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let (status, body) = d.get("/v1/receipts", key).await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let receipts = body["receipts"].as_array().expect("receipts").clone();
        if receipts.len() >= n {
            return receipts;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {n} receipts");
}

// ------------------------------------------------------------------ auth

#[tokio::test]
async fn auth_no_header_401() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/tokens",
            None,
            &json!({"token": TOKEN_A, "platform": "apns"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string(), "error shape: {body}");
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn auth_wrong_key_401() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/tokens",
            Some("wrong-key"),
            &json!({"token": TOKEN_A, "platform": "apns"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string(), "error shape: {body}");
}

#[tokio::test]
async fn auth_right_key_tenant_stamped() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    // The 201 + the tenant-scoped receipts read only work when the
    // middleware stamped tenant-a from the matched key.
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, _) = d.get("/v1/receipts", KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::OK);
}

/// Audit finding 5: healthz is PUBLIC and must leak nothing — status only.
#[tokio::test]
async fn healthz_is_status_only_without_rails() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let resp = d
        .client
        .get(d.url("/v1/healthz"))
        .send()
        .await
        .expect("req");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["status"], json!("ok"));
    assert!(
        body.get("rails").is_none(),
        "which rails are configured must not be public: {body}"
    );
    // The object has exactly one key.
    assert_eq!(body.as_object().expect("object").len(), 1);
}

/// Audit finding 5: the rails booleans moved behind auth on /v1/status —
/// same body shape healthz used to serve.
#[tokio::test]
async fn status_requires_auth_and_shows_rails() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    // No bearer: 401, no rails leaked.
    let resp = d.client.get(d.url("/v1/status")).send().await.expect("req");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    // With a key: the old healthz shape.
    let (status, body) = d.get("/v1/status", KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(
        body["rails"],
        json!({"apns": true, "fcm": false, "webpush": false})
    );
    // And with nothing configured: all false, still ok.
    let empty = spawn_daemon(50, Rails::new(None, None, None)).await;
    let (status, body) = empty.get("/v1/status", KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["rails"],
        json!({"apns": false, "fcm": false, "webpush": false})
    );
}

// ---------------------------------------------------------------- tokens

#[tokio::test]
async fn tokens_unknown_field_rejected_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/tokens",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "platform": "apns", "tenant_id": "evil"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn tokens_invalid_platform_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    // apns-liveactivity is an embedded-router platform, not a daemon one.
    let (status, _) = d
        .post(
            "/v1/tokens",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "platform": "apns-liveactivity"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tokens_delete_idempotent_204() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let first = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_A).await;
    assert_eq!(first, reqwest::StatusCode::NO_CONTENT);
    let second = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_A).await;
    assert_eq!(second, reqwest::StatusCode::NO_CONTENT, "idempotent");
}

/// Audit finding 6: DELETE is 204 for every not-yours case — a
/// Foreign-vs-Missing split was a token-existence oracle.
#[tokio::test]
async fn tokens_foreign_tenant_delete_204_no_oracle() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    // tenant-b deletes tenant-a's token: 204, indistinguishable from a
    // token that never existed.
    let status = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_B).await;
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
    // ...and a never-existed token is also 204 (no oracle either way).
    let status = d.delete("/v1/tokens/never-existed-token", KEY_B).await;
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
    // The row survived and still belongs to tenant-a.
    register_apns(&d, KEY_A, TOKEN_A).await;
    let status = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
}

// ------------------------------------------------------------------ send

#[tokio::test]
async fn send_accepted_202_push_id_is_uuid_v4() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let push_id = send_silent(&d, KEY_A, TOKEN_A, json!({})).await;
    let parsed = uuid::Uuid::parse_str(&push_id).expect("push_id parses as uuid");
    assert_eq!(
        parsed.get_version_num(),
        4,
        "push_id is uuid v4 (plan pin 0.4)"
    );
}

#[tokio::test]
async fn send_unknown_token_404() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": "never-registered-token", "payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn send_rail_unconfigured_503() {
    // Token on apns, but only the fcm rail is wired.
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Fcm, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let msg = body["error"].as_str().expect("error string");
    assert!(msg.contains("apns"), "names the platform: {msg}");
}

#[tokio::test]
async fn send_malformed_body_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    // Missing token.
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    // Non-decimal lsn.
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "0/16A4"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    // Both payload variants at once (untagged oneOf rejects).
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "1"}, "visible": {"title": "t", "body": "b"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn send_unknown_field_rejected_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "1"}}, "topics": ["x"]}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

// ------------------------------------------------------------- coalescer

#[tokio::test]
async fn coalescer_burst_20_single_dispatch_all_receipts() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(500, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;

    let mut push_ids = Vec::new();
    for i in 0..20 {
        push_ids.push(send_silent(&d, KEY_A, TOKEN_A, json!({"i": i})).await);
    }
    let receipts = wait_for_receipts(&d, KEY_A, 20).await;
    // Let any (wrong) second window fire before counting.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(*calls.lock().unwrap(), 1, "burst of 20 => 1 rail dispatch");

    // Every accepted push_id yields exactly one receipt.
    let mut receipt_ids: Vec<&str> = receipts
        .iter()
        .map(|r| r["push_id"].as_str().expect("push_id"))
        .collect();
    receipt_ids.sort_unstable();
    let mut expected = push_ids.clone();
    expected.sort_unstable();
    assert_eq!(
        receipt_ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );

    // Exactly one winner receipt (no coalesced detail) — the LAST send, its
    // payload won. 19 losers share the outcome with coalesced:<winner>.
    let winners: Vec<&Value> = receipts
        .iter()
        .filter(|r| r.get("detail").is_none())
        .collect();
    assert_eq!(winners.len(), 1, "one winner receipt: {receipts:?}");
    let winner_id = winners[0]["push_id"].as_str().expect("winner id");
    assert_eq!(winner_id, push_ids[19], "latest payload wins the window");
    for r in &receipts {
        let id = r["push_id"].as_str().expect("id");
        if id == winner_id {
            assert_eq!(r["outcome"], json!("delivered"));
        } else {
            assert_eq!(
                r["detail"],
                json!(format!("coalesced:{winner_id}")),
                "loser detail names the winner"
            );
            assert_eq!(r["outcome"], json!("delivered"), "shares the outcome");
        }
        // Each receipt echoes ITS OWN request's metadata (i keyed by push_id).
        let expected_i = push_ids
            .iter()
            .position(|p| p == id)
            .expect("known push_id");
        assert_eq!(r["metadata"]["i"], json!(expected_i), "own metadata echo");
    }
}

#[tokio::test]
async fn coalescer_separate_windows_two_dispatches() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(150, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;

    send_silent(&d, KEY_A, TOKEN_A, json!({"round": 1})).await;
    let first = wait_for_receipts(&d, KEY_A, 1).await;
    // Past the window: a new send opens a NEW window (deadline is fixed per
    // window — a steady stream cannot defer forever, but spacing beyond the
    // window must not coalesce either).
    tokio::time::sleep(Duration::from_millis(300)).await;
    send_silent(&d, KEY_A, TOKEN_A, json!({"round": 2})).await;
    let second = wait_for_receipts(&d, KEY_A, 2).await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert_eq!(*calls.lock().unwrap(), 2, "two windows, two dispatches");
    assert_eq!(first.len(), 1);
    assert!(
        second.iter().all(|r| r.get("detail").is_none()),
        "no coalescing across windows"
    );
}

#[tokio::test]
async fn prune_on_unregistered() {
    let (calls, mock) = MockRail::new(RailOutcome::Unregistered);
    let d = spawn_daemon(80, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;

    send_silent(&d, KEY_A, TOKEN_A, json!({})).await;
    let receipts = wait_for_receipts(&d, KEY_A, 1).await;
    assert_eq!(receipts[0]["outcome"], json!("unregistered"));

    // The registry row is gone: a follow-up send 404s, and a second flush
    // never happens (nothing left to dispatch).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "2"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "token pruned");
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn receipt_outcomes_mapped_with_detail() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let mock = mock.scripted(vec![
        RailOutcome::Fatal("boom".to_string()),
        RailOutcome::TransientRetryable,
    ]);
    let d = spawn_daemon(120, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;

    // Fatal: outcome fatal + the rail's diagnostic as detail.
    send_silent(&d, KEY_A, TOKEN_A, json!({})).await;
    let r1 = wait_for_receipts(&d, KEY_A, 1).await;
    assert_eq!(r1[0]["outcome"], json!("fatal"));
    assert_eq!(r1[0]["detail"], json!("boom"));

    // Transient: terminal on the receipt in v1 (no retries — callers retry).
    tokio::time::sleep(Duration::from_millis(250)).await;
    send_silent(&d, KEY_A, TOKEN_A, json!({})).await;
    let r2 = wait_for_receipts(&d, KEY_A, 2).await;
    assert_eq!(r2[1]["outcome"], json!("transient"));
    assert!(r2[1].get("detail").is_none());
}

// -------------------------------------------------------------- receipts

#[tokio::test]
async fn receipts_cursor_pagination() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(120, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;

    // Two receipts in two separate windows => two distinct seqs.
    send_silent(&d, KEY_A, TOKEN_A, json!({"n": 1})).await;
    let _ = wait_for_receipts(&d, KEY_A, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    send_silent(&d, KEY_A, TOKEN_A, json!({"n": 2})).await;
    let all = wait_for_receipts(&d, KEY_A, 2).await;

    let seqs: Vec<i64> = all
        .iter()
        .map(|r| r["seq"].as_i64().expect("seq"))
        .collect();
    assert!(seqs[0] < seqs[1], "ascending: {seqs:?}");

    let (status, body) = d.get("/v1/receipts?limit=1", KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["receipts"].as_array().expect("list").len(),
        1,
        "limit=1"
    );

    let (status, body) = d
        .get(&format!("/v1/receipts?since={}", seqs[0]), KEY_A)
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let tail = body["receipts"].as_array().expect("list").clone();
    assert_eq!(tail.len(), 1, "since=<first seq> skips the first row");
    assert_eq!(tail[0]["seq"], json!(seqs[1]));
    assert_eq!(tail[0]["metadata"]["n"], json!(2));
}

#[tokio::test]
async fn receipts_tenant_isolation() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(120, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    register_apns(&d, KEY_B, TOKEN_B).await;

    let id_a = send_silent(&d, KEY_A, TOKEN_A, json!({"who": "a"})).await;
    let id_b = send_silent(&d, KEY_B, TOKEN_B, json!({"who": "b"})).await;

    let receipts_a = wait_for_receipts(&d, KEY_A, 1).await;
    let receipts_b = wait_for_receipts(&d, KEY_B, 1).await;
    let ids_a: Vec<&str> = receipts_a
        .iter()
        .map(|r| r["push_id"].as_str().expect("id"))
        .collect();
    let ids_b: Vec<&str> = receipts_b
        .iter()
        .map(|r| r["push_id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids_a, vec![id_a.as_str()], "tenant a sees only its own");
    assert_eq!(ids_b, vec![id_b.as_str()], "tenant b sees only its own");
}

// -------------------------------------------------- rail mode (contract
// 0.2.0, plan pin 2.0): unregistered token + platform field => direct
// dispatch; unregistered + no field => the unchanged standalone 404.

/// Rail-mode send: unregistered token + platform => 202, one dispatch, one
/// receipt — no registry row is ever created.
#[tokio::test]
async fn rail_mode_unregistered_token_with_platform_dispatches() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(80, rails_for(Platform::Apns, &mock)).await;

    // NOT registered — the platform field is the whole address, and the
    // key carries the :rail role (finding 1).
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &json!({
                "token": "rail-mode-token-0123456789abcdef",
                "platform": "apns",
                "payload": {"silent": {"table": "tasks", "lsn": "77"}},
                "metadata": {"account": "u1", "lsn": "77"},
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "rail mode: {body}");
    let push_id = body["push_id"].as_str().expect("push_id").to_string();

    let receipts = wait_for_receipts(&d, KEY_RAIL, 1).await;
    // Let any (wrong) second window fire before counting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*calls.lock().unwrap(), 1, "exactly one rail dispatch");
    assert_eq!(receipts[0]["push_id"], json!(push_id));
    assert_eq!(receipts[0]["outcome"], json!("delivered"));
    assert_eq!(
        receipts[0]["metadata"]["account"],
        json!("u1"),
        "metadata echo (the correlation channel)"
    );

    // No registry row was created: the next send WITHOUT the platform
    // field still 404s (rail mode never writes the registry).
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &json!({
                "token": "rail-mode-token-0123456789abcdef",
                "payload": {"silent": {"table": "tasks", "lsn": "78"}},
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

/// Rail mode with a garbage platform value is a 400 (enum validated on
/// deserialize — before any registry access).
#[tokio::test]
async fn rail_mode_garbage_platform_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": "rail-mode-token-0123456789abcdef",
                "platform": "apns-liveactivity",
                "payload": {"silent": {"table": "t", "lsn": "1"}},
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].is_string(), "error shape: {body}");
}

/// Unregistered + no platform field: the standalone registry semantics are
/// unchanged — 404, no dispatch.
#[tokio::test]
async fn rail_mode_absent_platform_unregistered_still_404() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": "never-registered-token", "payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].is_string());
    assert_eq!(*calls.lock().unwrap(), 0, "no dispatch happens");
}

/// Precedence: a REGISTERED token ignores the platform field — the registry
/// row's platform wins (the daemon registry is the source of truth).
#[tokio::test]
async fn rail_mode_registered_token_ignores_platform_field() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    // TOKEN_A is registered as apns; only the fcm-side rail is NOT wired,
    // so honoring the (wrong) fcm field would 503 — the registry must win.
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": TOKEN_A,
                "platform": "fcm",
                "payload": {"silent": {"table": "t", "lsn": "1"}},
            }),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "registry platform wins over the advisory field: {body}"
    );
    wait_for_receipts(&d, KEY_A, 1).await;
    assert_eq!(*calls.lock().unwrap(), 1, "dispatched on the apns rail");
}

// ------------------------------------------------ audit closeout (task 4.1)

/// Finding 1: rail mode with a STANDARD key is 403 — the registry path
/// stays open to the very same key, proving the gate is the role, not the
/// key's validity.
#[tokio::test]
async fn rail_mode_standard_key_forbidden_403() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": "rail-mode-token-0123456789abcdef",
                "platform": "apns",
                "payload": {"silent": {"table": "tasks", "lsn": "1"}},
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].is_string(), "error shape: {body}");
    assert_eq!(*calls.lock().unwrap(), 0, "no dispatch happens");

    // The same Standard key still sends to a REGISTERED token (202).
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "2"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED);
}

/// Finding 2a: the per-tenant token bucket — burst exhausted => 429, the
/// OTHER tenant's bucket is untouched. rate 0 => no refill, so the third
/// send deterministically 429s.
#[tokio::test]
async fn send_rate_limited_429_after_burst_other_tenant_unaffected() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon_tuned(
        50,
        rails_for(Platform::Apns, &mock),
        0,
        2,
        CoalescerLimits::default(),
    )
    .await;
    let rail_body = |token: &str| {
        json!({
            "token": token,
            "platform": "apns",
            "payload": {"silent": {"table": "t", "lsn": "1"}},
        })
    };
    // tenant-a (Standard) drives its bucket through two registered sends.
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (s1, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    let (s2, _) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "2"}}}),
        )
        .await;
    let (s3, body3) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": TOKEN_A, "payload": {"silent": {"table": "t", "lsn": "3"}}}),
        )
        .await;
    assert_eq!(s1, reqwest::StatusCode::ACCEPTED);
    assert_eq!(s2, reqwest::StatusCode::ACCEPTED, "burst of 2");
    assert_eq!(
        s3,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "exhausted: {body3}"
    );
    assert!(body3["error"].is_string(), "error shape: {body3}");
    // tenant-b has its own bucket: 202.
    register_apns(&d, KEY_B, TOKEN_B).await;
    let (s4, body4) = d
        .post(
            "/v1/send",
            Some(KEY_B),
            &json!({"token": TOKEN_B, "payload": {"silent": {"table": "t", "lsn": "4"}}}),
        )
        .await;
    assert_eq!(
        s4,
        reqwest::StatusCode::ACCEPTED,
        "other tenant unaffected: {body4}"
    );
    // The rail-role tenant is equally subject to its own bucket.
    let (s5, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("rail-tok-aaaaaaaaaaaa"),
        )
        .await;
    assert_eq!(s5, reqwest::StatusCode::ACCEPTED);
    let (s6, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("rail-tok-bbbbbbbbbbbb"),
        )
        .await;
    assert_eq!(s6, reqwest::StatusCode::ACCEPTED);
    let (s7, body7) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("rail-tok-cccccccccccc"),
        )
        .await;
    assert_eq!(s7, reqwest::StatusCode::TOO_MANY_REQUESTS, "{body7}");
}

/// Finding 2b: every capped field answers 400; the exact boundary passes.
#[tokio::test]
async fn send_field_caps_each_oversized_field_400() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let visible = |title: String, body_text: String, category: Option<String>| {
        json!({
            "token": TOKEN_A,
            "payload": {"visible": {"title": title, "body": body_text, "category": category}},
        })
    };
    // title > 256
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &visible("t".repeat(257), "b".to_string(), None),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "title: {body}");
    assert!(body["error"].as_str().expect("err").contains("title"));
    // body > 1024
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &visible("t".to_string(), "b".repeat(1025), None),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body: {body}");
    // category > 128
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &visible("t".to_string(), "b".to_string(), Some("c".repeat(129))),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "category: {body}");
    // token > 2048 (send-path cap, aligned with the registry's own bound)
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({"token": "x".repeat(2049), "payload": {"silent": {"table": "t", "lsn": "1"}}}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "token: {body}");
    // collapse_key > 256
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": TOKEN_A,
                "collapse_key": "k".repeat(257),
                "payload": {"silent": {"table": "t", "lsn": "1"}},
            }),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "collapse_key: {body}"
    );
    // serialized metadata > 4096 bytes
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": TOKEN_A,
                "metadata": {"pad": "m".repeat(4200)},
                "payload": {"silent": {"table": "t", "lsn": "1"}},
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "metadata: {body}");
    assert!(body["error"].as_str().expect("err").contains("metadata"));
    // Boundary: exactly-at-cap fields are accepted.
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &visible("t".repeat(256), "b".repeat(1024), Some("c".repeat(128))),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "boundary passes: {body}"
    );

    // Token boundary, registry-consistent: a 2048-char token (Web Push
    // subscription JSON territory) the REGISTRY accepted must still SEND —
    // the send-path cap is the same 2048, not a tighter one.
    let long_token = "w".repeat(2048);
    register_apns(&d, KEY_A, &long_token).await;
    let (status, body) = d
        .post(
            "/v1/send",
            Some(KEY_A),
            &json!({
                "token": long_token,
                "payload": {"silent": {"table": "t", "lsn": "9"}},
            }),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "a token the registry accepted (2048 chars) must be sendable: {body}"
    );
}

/// Finding 2c (pending ceiling): with room for two open windows, the third
/// DISTINCT key is 429; once the windows flush, admission recovers.
#[tokio::test]
async fn coalescer_pending_key_ceiling_429_until_windows_flush() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon_tuned(
        2000,
        rails_for(Platform::Apns, &mock),
        10,
        50,
        CoalescerLimits {
            pending_keys_max: 2,
            losers_max: 64,
        },
    )
    .await;
    let rail_body = |token: &str, lsn: u64| {
        json!({
            "token": token,
            "platform": "apns",
            "payload": {"silent": {"table": "t", "lsn": lsn.to_string()}},
        })
    };
    let (s1, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("ceil-tok-aaaaaaaaaaaa", 1),
        )
        .await;
    let (s2, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("ceil-tok-bbbbbbbbbbbb", 2),
        )
        .await;
    let (s3, body3) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("ceil-tok-cccccccccccc", 3),
        )
        .await;
    assert_eq!(s1, reqwest::StatusCode::ACCEPTED);
    assert_eq!(s2, reqwest::StatusCode::ACCEPTED);
    assert_eq!(
        s3,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "third distinct key: {body3}"
    );
    assert!(body3["error"].is_string());
    // An already-open key still joins its window — never 429.
    let (s4, _) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("ceil-tok-aaaaaaaaaaaa", 4),
        )
        .await;
    assert_eq!(
        s4,
        reqwest::StatusCode::ACCEPTED,
        "open key always admitted"
    );
    // After the windows flush, the gate reopens.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let (s5, body5) = d
        .post(
            "/v1/send",
            Some(KEY_RAIL),
            &rail_body("ceil-tok-dddddddddddd", 5),
        )
        .await;
    assert_eq!(
        s5,
        reqwest::StatusCode::ACCEPTED,
        "recovered after flush: {body5}"
    );
}

/// Finding 2c (losers ceiling): six sends to one key with a 4-loser cap —
/// still exactly one receipt per push_id, one rail dispatch, and FIVE
/// coalesced receipts (one of them evicted past the cap).
#[tokio::test]
async fn coalescer_losers_ceiling_every_push_id_still_receipted() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon_tuned(
        600,
        rails_for(Platform::Apns, &mock),
        10,
        50,
        CoalescerLimits {
            pending_keys_max: 10_000,
            losers_max: 4,
        },
    )
    .await;
    let mut push_ids = Vec::new();
    for i in 0..6u64 {
        let (status, body) = d
            .post(
                "/v1/send",
                Some(KEY_RAIL),
                &json!({
                    "token": "losers-tok-aaaaaaaaaaaa",
                    "platform": "apns",
                    "metadata": {"i": i},
                    "payload": {"silent": {"table": "t", "lsn": i.to_string()}},
                }),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body}");
        push_ids.push(body["push_id"].as_str().expect("push_id").to_string());
    }
    let receipts = wait_for_receipts(&d, KEY_RAIL, 6).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(*calls.lock().unwrap(), 1, "one coalesced dispatch");

    // Every push_id yields exactly one receipt — the evicted oldest
    // included, receipted as coalesced like every other loser.
    let mut receipt_ids: Vec<&str> = receipts
        .iter()
        .map(|r| r["push_id"].as_str().expect("push_id"))
        .collect();
    receipt_ids.sort_unstable();
    let mut expected = push_ids.clone();
    expected.sort_unstable();
    assert_eq!(
        receipt_ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "one receipt per push_id, ceiling or not"
    );
    let winners: Vec<&Value> = receipts
        .iter()
        .filter(|r| r.get("detail").is_none())
        .collect();
    assert_eq!(winners.len(), 1, "exactly one winner: {receipts:?}");
    let winner_id = winners[0]["push_id"].as_str().expect("winner id");
    assert_eq!(winner_id, push_ids[5], "latest payload wins");
    for r in &receipts {
        if r["push_id"].as_str() != Some(winner_id) {
            assert_eq!(
                r["detail"],
                json!(format!("coalesced:{winner_id}")),
                "loser (evicted or not) detail names the winner"
            );
        }
    }
}

/// Finding 3: cross-tenant registration is 409 (ownership never silently
/// reassigns); the documented DELETE-then-POST migration succeeds.
#[tokio::test]
async fn tokens_cross_tenant_register_409_then_delete_then_register_201() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    let (status, body) = d
        .post(
            "/v1/tokens",
            Some(KEY_B),
            &json!({"token": TOKEN_A, "platform": "fcm"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body}");
    assert!(body["error"].is_string(), "error shape: {body}");
    // tenant-b's own DELETE cannot free the row (204, no oracle) — the
    // OWNER deletes, then the new tenant registers successfully.
    let _ = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_B).await;
    let status = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_A).await;
    assert_eq!(status, reqwest::StatusCode::NO_CONTENT);
    let (status, body) = d
        .post(
            "/v1/tokens",
            Some(KEY_B),
            &json!({"token": TOKEN_A, "platform": "fcm"}),
        )
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::CREATED,
        "delete-then-register: {body}"
    );
    assert_eq!(body["registered"], json!(true));
}

// ------------------------------------------- batch send (contract 0.4.0)

/// Happy path: two registered tokens, one batch -> 202, per-item push_ids
/// in request order, and both sends deliver + receipt after the window.
#[tokio::test]
async fn batch_send_happy_path_202_receipts_in_order() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, "tok-batch-1").await;
    register_apns(&d, KEY_A, "tok-batch-2").await;

    let (status, body) = d
        .post(
            "/v1/send/batch",
            Some(KEY_A),
            &json!({
                "items": [
                    {"token": "tok-batch-1", "payload": {"silent": {"table": "docs", "lsn": "1"}}, "metadata": {"i": 0}},
                    {"token": "tok-batch-2", "payload": {"silent": {"table": "docs", "lsn": "2"}}, "metadata": {"i": 1}},
                ]
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "batch: {body}");
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "one result per item");
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["index"], json!(i), "results ride in request order");
        assert_eq!(r["status"], json!("accepted"));
        assert!(r["push_id"].as_str().is_some_and(|s| s.len() == 36));
        assert!(r.get("error").is_none(), "accepted items carry no error");
    }

    let receipts = wait_for_receipts(&d, KEY_A, 2).await;
    assert_eq!(receipts.len(), 2);
    assert_eq!(*calls.lock().expect("calls"), 2, "both items dispatched");
}

/// Atomic phase 1: item 1 is over the title cap -> the WHOLE batch 400s
/// naming the index, and item 0 (valid) is never admitted — zero rail
/// calls, zero receipts.
#[tokio::test]
async fn batch_send_one_invalid_item_fails_whole_batch_400() {
    let (calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, "tok-batch-ok").await;

    let (status, body) = d
        .post(
            "/v1/send/batch",
            Some(KEY_A),
            &json!({
                "items": [
                    {"token": "tok-batch-ok", "payload": {"silent": {"table": "docs", "lsn": "1"}}},
                    {"token": "tok-batch-ok", "payload": {"visible": {"title": "x".repeat(300), "body": "b"}}},
                ]
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "batch: {body}");
    let msg = body["error"].as_str().expect("error message");
    assert!(msg.contains("item 1"), "names the failing index: {msg}");

    tokio::time::sleep(Duration::from_millis(300)).await; // > debounce window
    assert!(*calls.lock().expect("calls") == 0, "nothing dispatched");
    let (_, receipts) = d.get("/v1/receipts", KEY_A).await;
    assert_eq!(
        receipts["receipts"].as_array().expect("receipts").len(),
        0,
        "nothing receipted"
    );
}

/// Rail mode in a batch under a STANDARD key -> whole-batch 403 (finding 1
/// holds at batch granularity), naming the failing item.
#[tokio::test]
async fn batch_send_rail_mode_standard_key_forbidden_403() {
    let (_calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, body) = d
        .post(
            "/v1/send/batch",
            Some(KEY_A),
            &json!({
                "items": [
                    {"token": "unregistered-token-1", "platform": "apns", "payload": {"silent": {"table": "docs", "lsn": "1"}}},
                ]
            }),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "batch: {body}");
    assert!(body["error"].as_str().expect("error").contains("item 0"));
}

/// The item cap: 0 or >100 items is a 400 before any rate/lookup work.
#[tokio::test]
async fn batch_send_item_count_cap_400() {
    let (_calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    let (status, _) = d
        .post("/v1/send/batch", Some(KEY_A), &json!({"items": []}))
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "empty batch");

    let items: Vec<Value> = (0..101)
        .map(|_| json!({"token": "t", "payload": {"silent": {"table": "docs", "lsn": "1"}}}))
        .collect();
    let (status, body) = d
        .post("/v1/send/batch", Some(KEY_A), &json!({"items": items}))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "101 items: {body}"
    );
    assert!(body["error"].as_str().expect("error").contains("1..=100"));
}

/// The batch rate check is all-or-nothing AND non-draining: a 4-item batch
/// against a burst-3 bucket 429s, and the bucket keeps its tokens — a
/// single /v1/send right after still succeeds.
#[tokio::test]
async fn batch_send_rate_short_bucket_429_non_draining() {
    let (_calls, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon_tuned(
        50,
        rails_for(Platform::Apns, &mock),
        10,
        3,
        CoalescerLimits::default(),
    )
    .await;
    register_apns(&d, KEY_A, "tok-batch-rate").await;

    let items: Vec<Value> = (0..4)
        .map(|i| json!({"token": "tok-batch-rate", "payload": {"silent": {"table": "docs", "lsn": i.to_string()}}}))
        .collect();
    let (status, body) = d
        .post("/v1/send/batch", Some(KEY_A), &json!({"items": items}))
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "batch: {body}"
    );

    // The failed batch did not drain the bucket: a single send still fits.
    let push_id = send_silent(&d, KEY_A, "tok-batch-rate", json!({"after": "429"})).await;
    assert_eq!(push_id.len(), 36);
}
