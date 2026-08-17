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
use nostos_push::coalescer;
use nostos_push::rail::{RailDispatch, Rails};
use nostos_push::store::{Platform, SqliteStore, Store};
use nostos_push::{build_router, AppState};
use serde_json::{json, Value};

const KEY_A: &str = "test-secret-a";
const KEY_B: &str = "test-secret-b";
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

/// Bind a daemon: in-memory store, the given rails, the given debounce.
async fn spawn_daemon(debounce_ms: u64, rails: Rails) -> Daemon {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().expect("store"));
    let coalescer = coalescer::spawn_coalescer(
        Arc::clone(&store),
        rails.clone(),
        Duration::from_millis(debounce_ms),
    );
    let state = AppState {
        store,
        rails,
        api_keys: ApiKeys::parse(&format!("tenant-a:{KEY_A},tenant-b:{KEY_B}")).expect("test keys"),
        sender: coalescer.tx.clone(),
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

#[tokio::test]
async fn healthz_unauthenticated_200_rails_reported() {
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
    assert_eq!(
        body["rails"],
        json!({"apns": true, "fcm": false, "webpush": false})
    );
    // And with nothing configured: all false, still ok.
    let empty = spawn_daemon(50, Rails::new(None, None, None)).await;
    let resp = empty
        .client
        .get(empty.url("/v1/healthz"))
        .send()
        .await
        .expect("req");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["status"], json!("ok"));
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

#[tokio::test]
async fn tokens_foreign_tenant_delete_404() {
    let (_, mock) = MockRail::new(RailOutcome::Delivered);
    let d = spawn_daemon(50, rails_for(Platform::Apns, &mock)).await;
    register_apns(&d, KEY_A, TOKEN_A).await;
    // tenant-b deletes tenant-a's token: oracle-safe 404.
    let status = d.delete(&format!("/v1/tokens/{TOKEN_A}"), KEY_B).await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
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
