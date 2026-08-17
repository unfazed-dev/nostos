//! The ADR-0038 delegation leg (plan task 2.4 — "the test that
//! matters"): nostos-server's RemoteNotifier delegating to a REAL spawned
//! nostos-pushd. In-process on both sides but full-fidelity on the wire:
//! a real axum daemon over a tempdir SQLite store, and the real
//! nostos-infra RemoteNotifier (the `PushNotifier` port impl) wired with
//! in-memory registry + session store. nostos-push tests using
//! nostos-infra / nostos-application's port types is the sanctioned
//! direction (the daemon LIB stays nostos-domain + nostos-infra only, pin
//! 0.5 — the dev-dependency does not change the shipped dependency
//! direction).
//!
//! The scenario mirrors ADR-0037's test-that-matters across the network
//! hop: two devices share one (offline) account, another account is
//! ONLINE; a burst of hints ⇒ exactly ONE coalesced rail dispatch per
//! offline token, ZERO to the online account, and the receipt log records
//! EVERY push_id with its metadata echo (the push-LSN correlation
//! channel) — which RemoteNotifier's receipt poll feeds back into
//! Metrics.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nostos_application::ports::{
    DeliveryDecision, EventSink, Metrics, PushHint, PushNotifier, SessionStore,
};
use nostos_domain::{Lsn, Predicate, Principal, ReplicationEvent, SyncSession};
use nostos_infra::push::remote::RemoteNotifier;
use nostos_infra::push::router::{InMemoryTokenRegistry, PushTokenRegistry, RouterConfig};
use nostos_infra::push::{PushPayload, RailOutcome};
use nostos_infra::InMemorySessionStore;
use nostos_push::auth::ApiKeys;
use nostos_push::coalescer::{self, CoalescerLimits};
use nostos_push::limit::SendRateLimiter;
use nostos_push::rail::{RailDispatch, Rails};
use nostos_push::store::{SqliteStore, Store};
use nostos_push::{build_router, AppState};
use serde_json::Value;

const KEY: &str = "delegation-test-key";
const TOK_A: &str = "offline-token-aaaaaaaaaaaa";
const TOK_B: &str = "offline-token-bbbbbbbbbbbb";
const TOK_ON: &str = "online-token-cccccccccccc";
/// Hints fired at the offline account in the burst (its two tokens each
/// receive one POST per hint ⇒ 2× BURST accepted push_ids).
const BURST: u64 = 12;
const LATEST_LSN: u64 = 100 + BURST - 1;

/// In-memory rail double (the daemon.rs MockRail idiom, reduced to the
/// e2e's need): counts every dispatch into a shared counter, always
/// delivers.
struct CountingRail {
    calls: Arc<Mutex<usize>>,
}

impl CountingRail {
    fn new() -> (Arc<Mutex<usize>>, Arc<Self>) {
        let calls = Arc::new(Mutex::new(0usize));
        (
            Arc::clone(&calls),
            Arc::new(Self {
                calls, // the SAME counter the dispatches bump
            }),
        )
    }
}

#[async_trait]
impl RailDispatch for CountingRail {
    async fn send(
        &self,
        _token: &str,
        _collapse_key: Option<&str>,
        _payload: &PushPayload,
    ) -> RailOutcome {
        *self.calls.lock().expect("calls") += 1;
        RailOutcome::Delivered
    }
}

/// A no-op sink — presence only needs the session REGISTERED (store
/// membership is presence, ADR-0037 §4).
struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn deliver(&self, _e: ReplicationEvent) -> DeliveryDecision {
        DeliveryDecision::Delivered
    }
}

fn hint(tenant: &str, account: &str, table: &str, lsn: u64) -> PushHint {
    PushHint {
        table: table.to_string(),
        tenant_id: tenant.to_string(),
        account_id: account.to_string(),
        lsn: Lsn::new(lsn),
        payload: None,
    }
}

/// Poll (bounded) until the async predicate holds.
async fn soon<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A real daemon on an ephemeral port: tempdir SQLite store, one mocked
/// (counting, always-delivered) apns rail, and a 1500ms debounce so a
/// burst of sequential POSTs lands in ONE coalescing window (the
/// production default is 2000ms). Returns (addr, rail-call counter,
/// db-path for best-effort cleanup).
async fn spawn_daemon() -> (SocketAddr, Arc<Mutex<usize>>, String) {
    let db = std::env::temp_dir().join(format!("nostos-pushd-e2e-{}.db", uuid::Uuid::new_v4()));
    let db_path = db.to_string_lossy().into_owned();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&db_path).expect("sqlite store"));
    let (calls, rail) = CountingRail::new();
    let rails = Rails::new(Some(rail as Arc<dyn RailDispatch>), None, None);
    let coalescer = coalescer::spawn_coalescer(
        Arc::clone(&store),
        rails.clone(),
        Duration::from_millis(1500),
        CoalescerLimits::default(),
    );
    let state = AppState {
        store,
        rails,
        // The daemon-side key MUST carry the :rail role (plan task 4.1,
        // finding 1): RemoteNotifier sends in rail mode (unregistered
        // token + platform), which a Standard key may not.
        api_keys: ApiKeys::parse(&format!("tenant-a:{KEY}:rail")).expect("keys"),
        sender: coalescer.tx.clone(),
        send_limiter: Arc::new(SendRateLimiter::new(10, 50)),
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
    (addr, calls, db_path)
}

/// The ADR-0038 delegation test-that-matters: two devices share one
/// (offline) account, one account is online; a burst of hints through the
/// REAL RemoteNotifier against the REAL daemon ⇒ exactly one coalesced
/// dispatch per offline token, zero to the online one, every push_id
/// receipted with its metadata echo, and the receipt poll flipping
/// Metrics + the push-LSN correlation map.
#[tokio::test]
async fn delegation_leg_coalesced_dispatch_and_receipt_correlation() {
    let (addr, calls, db_path) = spawn_daemon().await;
    let base = format!("http://{addr}");

    // Sync side: in-memory registry — two tokens share offline account
    // u1; u2 holds one token and IS online via a registered session.
    let registry = Arc::new(InMemoryTokenRegistry::new());
    registry
        .upsert("apns", TOK_A, "u1", "sync-tenant")
        .await
        .unwrap();
    registry
        .upsert("apns", TOK_B, "u1", "sync-tenant")
        .await
        .unwrap();
    registry
        .upsert("apns", TOK_ON, "u2", "sync-tenant")
        .await
        .unwrap();
    let sessions = Arc::new(InMemorySessionStore::new());
    sessions
        .add(
            SyncSession::new_authenticated(
                Predicate::all("tasks"),
                Principal::new("u2", "org-acme"),
            ),
            Arc::new(NoopSink),
        )
        .await;
    assert!(sessions.account_online("u2").await, "u2 is online");
    assert!(!sessions.account_online("u1").await, "u1 is offline");

    let metrics = Arc::new(Metrics::new());
    let notifier = RemoteNotifier::new(
        &base,
        KEY,
        registry,
        sessions,
        RouterConfig::default(),
        Arc::clone(&metrics),
    );

    // The burst: BURST hints at the offline account, a few at the ONLINE
    // one (which must be suppressed sync-side — no POST at all).
    for i in 0..BURST {
        notifier
            .notify(hint("sync-tenant", "u1", "tasks", 100 + i))
            .await;
    }
    for i in 0..4u64 {
        notifier
            .notify(hint("sync-tenant", "u2", "tasks", 200 + i))
            .await;
    }

    let client = reqwest::Client::new();
    let fetch_receipts = || {
        let client = client.clone();
        let url = format!("{base}/v1/receipts?since=0&limit=1000");
        async move {
            let resp = client
                .get(url)
                .bearer_auth(KEY)
                .send()
                .await
                .expect("receipts poll");
            assert!(resp.status().is_success());
            let body: Value = resp.json().await.expect("json");
            body["receipts"].as_array().expect("receipts array").clone()
        }
    };

    // 12 hints × 2 offline tokens ⇒ 24 push_ids, each with one receipt.
    soon(|| async { fetch_receipts().await.len() >= 24 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // straggler guard
    let receipts = fetch_receipts().await;
    assert_eq!(
        receipts.len(),
        24,
        "every accepted push_id yields exactly one receipt: {receipts:?}"
    );

    // Exactly one dispatch per OFFLINE token, zero to the online one.
    assert_eq!(
        *calls.lock().expect("calls"),
        2,
        "one coalesced dispatch per offline token (tok-a + tok-b), zero to the online account"
    );

    // Distinct push_ids; every receipt echoes ITS OWN metadata.
    let mut ids: Vec<&str> = receipts
        .iter()
        .map(|r| r["push_id"].as_str().expect("push_id"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 24, "24 distinct push_ids");
    for r in &receipts {
        assert_eq!(
            r["metadata"]["account"],
            serde_json::json!("u1"),
            "only the offline account's sends exist: {r:?}"
        );
        let lsn = r["metadata"]["lsn"].as_str().expect("lsn echo");
        let lsn: u64 = lsn.parse().expect("decimal lsn");
        assert!(
            (100..100 + BURST).contains(&lsn),
            "metadata echoes the fired hint's LSN: {lsn}"
        );
    }

    // Winner/loser shape per token: one winner (latest LSN wins the
    // window), the rest coalesced with detail naming that winner.
    for token in [TOK_A, TOK_B] {
        let mine: Vec<&Value> = receipts
            .iter()
            .filter(|r| r["token"] == serde_json::json!(token))
            .collect();
        assert_eq!(mine.len(), 12, "one receipt per POST for {token}");
        let winners: Vec<&&Value> = mine.iter().filter(|r| r.get("detail").is_none()).collect();
        assert_eq!(winners.len(), 1, "exactly one winner per token");
        assert_eq!(
            winners[0]["metadata"]["lsn"],
            serde_json::json!(LATEST_LSN.to_string()),
            "latest payload wins the window"
        );
        let winner_id = winners[0]["push_id"].as_str().expect("winner id");
        for r in &mine {
            if r["push_id"].as_str() != Some(winner_id) {
                assert_eq!(
                    r["detail"],
                    serde_json::json!(format!("coalesced:{winner_id}"))
                );
            }
        }
    }

    // The receipt poll feeds Metrics: 2 winner receipts ⇒ push_sent 2;
    // losers count nowhere (not rail sends); the correlation map holds
    // the latest LSN for u1.
    soon(|| async {
        let s = metrics.snapshot();
        s.push_sent == 2 && s.push_failed == 0 && s.push_dropped == 0
    })
    .await;
    let s = metrics.snapshot();
    assert_eq!(s.push_sent, 2);
    assert_eq!(s.push_failed, 0);
    assert_eq!(s.push_dropped, 0, "the sync path never dropped a hint");
    assert_eq!(
        metrics.push_last_lsn.lock().unwrap().get("u1").copied(),
        Some(LATEST_LSN),
        "the receipt metadata echo is the push-LSN correlation channel"
    );

    // Rail mode never wrote a daemon registry row: a no-platform send for
    // a delegated token still 404s (standalone semantics unchanged).
    let resp = client
        .post(format!("{base}/v1/send"))
        .bearer_auth(KEY)
        .json(&serde_json::json!({
            "token": TOK_A,
            "payload": {"silent": {"table": "tasks", "lsn": "999"}},
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Best-effort tempdir cleanup (the OS sweeps regardless).
    let _ = std::fs::remove_file(&db_path);
}
