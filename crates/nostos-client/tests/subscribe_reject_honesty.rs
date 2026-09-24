//! Honest-connected hardening (2026-08-27 incident): a rules-REJECTED
//! subscribe must surface as an error and must NEVER look "connected" to UI
//! layers, while an accepted subscribe proves itself before the session ends.
//!
//! The incident: a deploy whose rules denied the table made the server close
//! every first-subscribe with INVALID(1008) + reason. The client swallowed the
//! close (clean break), the nostos_flutter bridge flipped `Connected` on a
//! 250ms grace window, and Dart's `waitForFirstSync()` completed against a
//! session that never delivered a single row. Two guarantees now hold:
//!
//! 1. `run_once` returns `ClientError::SubscribeRejected(reason)` for the
//!    1008-with-reason close (except the reserved rules-changed reason), and
//!    `run_with_reconnect` treats it as FATAL — no silent retry storm.
//! 2. The `SyncClient::subscribed()` watch stays `false` for a rejected
//!    session and flips `true` only after a post-acceptance frame (snapshot
//!    boundary / replication event / write ack) on an accepted one.
//!
//! PG-free like `epoch_persistence.rs`: the transport's reject path fires
//! before any replicator is consulted, so a hand-mode deny-all ruleset needs
//! no Postgres to exercise it.

use std::sync::Arc;
use std::time::Duration;

use nostos_application::ports::{Metrics, SyncAuth};
use nostos_application::{ActiveRuleset, SessionManager};
use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{SyncMode, SyncRules};
use nostos_infra::auth::AllowAnonymous;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// One test server on an ephemeral port, with a CHOSEN ruleset: pass
/// `None` for all-mode (everything allowed — the accepting server) or a
/// compiled deny-all hand-mode ruleset (the rejecting server). Returns the
/// `ws://127.0.0.1:<port>/sync` URL.
async fn server_with_rules(rules: Option<ActiveRuleset>) -> String {
    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let mut state = SyncRouterState::new(manager, auth)
        .with_buffer(64)
        .with_metrics(metrics);
    if let Some(ruleset) = rules {
        state.rules = Arc::new(tokio::sync::RwLock::new(ruleset));
    }
    // The accepting server needs a snapshotter so accepted subscribes emit
    // their begin/end boundaries (the reject path never reaches it).
    state.snapshotter = Some(Arc::new(EmptySnapshotter));
    let app = axum::Router::new()
        .route("/sync", axum::routing::get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);
    format!("ws://{addr}/sync")
}

fn deny_all() -> ActiveRuleset {
    // Hand mode with an EMPTY `[[rules]]` list: `decide()` finds no scope
    // for any table → `DeniedTable` → the fatal first-subscribe reject path.
    // (version 1 is the format `compile` accepts — 0 is `UnsupportedVersion`.)
    ActiveRuleset::compile(&SyncRules {
        version: 1,
        mode: SyncMode::Hand,
        ..SyncRules::default()
    })
    .expect("deny-all ruleset compiles")
}

/// A `SnapshotSource` standing in for `PgSnapshotter`: zero rows for every
/// table. The transport still brackets an accepted subscribe with
/// snapshot_begin/end boundaries (ADR-0014), which is exactly the first
/// post-acceptance frame the PROVEN signal needs — an empty table must still
/// prove its session.
struct EmptySnapshotter;

#[async_trait::async_trait]
impl nostos_application::ports::SnapshotSource for EmptySnapshotter {
    async fn snapshot(
        &self,
        _table: &str,
        _base_lsn: nostos_domain::Lsn,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<Vec<nostos_domain::ReplicationEvent>, nostos_application::ports::SnapshotError>
    {
        Ok(Vec::new())
    }

    async fn snapshot_stream(
        &self,
        _table: &str,
        _predicate: &nostos_domain::PredicateExpr,
        _base_lsn: nostos_domain::Lsn,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<Vec<nostos_domain::ReplicationEvent>, nostos_application::ports::SnapshotError>
    {
        Ok(Vec::new())
    }
}

/// A rules-denied subscribe surfaces as `SubscribeRejected` carrying the
/// server's close reason, the session is never PROVEN, and
/// `run_with_reconnect` exits instead of retrying forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_subscribe_surfaces_error_and_stays_unproven() {
    let url = server_with_rules(Some(deny_all())).await;
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(500)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url, storage, config);

    let mut subscribed = client.subscribed();
    let err = client
        .run_once()
        .await
        .expect_err("a denied subscribe must error, not clean-return");
    match &err {
        ClientError::SubscribeRejected(reason) => {
            assert!(
                !reason.is_empty(),
                "the server's rejection reason must cross the wire"
            );
        }
        other => panic!("expected SubscribeRejected, got: {other:?}"),
    }
    // The honest signal never fired: no snapshot boundary, no events, no acks.
    assert!(
        !*subscribed.borrow_and_update(),
        "a rejected session must never be PROVEN"
    );

    // FATAL, not retried: run_with_reconnect returns the same error promptly
    // (timeout-bounded so a regression fails fast instead of hanging).
    let retry = tokio::time::timeout(Duration::from_secs(10), client.run_with_reconnect())
        .await
        .expect("run_with_reconnect must not loop forever on a rejection");
    assert!(
        matches!(retry, Err(ClientError::SubscribeRejected(_))),
        "run_with_reconnect must exit with the rejection, got: {retry:?}"
    );
}

/// An ACCEPTED subscribe proves itself mid-session: the `subscribed()`
/// watch flips true BEFORE `run_once` returns (the snapshot boundary that
/// brackets even an empty table is the first post-acceptance frame).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_subscribe_proves_itself_before_session_end() {
    let url = server_with_rules(None).await; // all-mode: everything allowed
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(800)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url, storage, config);

    let mut subscribed = client.subscribed();
    let (probe, outcome) = tokio::join!(
        async {
            // borrow-check-then-changed (wait_for holds a guard across await);
            // bounded so a regression (flag never set) fails instead of hanging.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if *subscribed.borrow_and_update() {
                        return true;
                    }
                    if subscribed.changed().await.is_err() {
                        return false;
                    }
                }
            })
            .await
            .unwrap_or(false)
        },
        client.run_once(),
    );
    outcome.expect("an accepted session runs clean until idle-out");
    assert!(
        probe,
        "the session must be PROVEN (snapshot boundary seen) before run_once returns"
    );
    assert!(
        *subscribed.borrow_and_update(),
        "the PROVEN flag must still read true at session end"
    );
}
