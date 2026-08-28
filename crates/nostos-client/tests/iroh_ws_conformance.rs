//! ADR-0041 transport conformance: the SAME fixture, the SAME assertions,
//! the URL swapped ws:// <-> iroh://. This is "the test that matters" from
//! the ADR — the transport is plumbing, so behavior must be identical.
//!
//! Fixture: an all-mode server whose SeedSnapshotter hands "tasks" two
//! pre-existing rows — a fresh client connecting over EITHER transport must
//! receive both snapshot rows (frames_received >= 2), reach a checkpoint,
//! and reconnect cleanly for a second session (idempotent apply).
//!
//! ## Running
//!
//! cargo test -p nostos-client --features iroh --test iroh_ws_conformance -- --nocapture
//!
//! The iroh leg binds real endpoints (n0 preset: relay + discovery) but
//! connects over the ticket's DIRECT address hints — on one host the
//! connection is immediate, no relay round-trip. Timeout-bounded so a
//! sandboxed network fails the test rather than hanging it.

#![cfg(feature = "iroh")]

use std::sync::Arc;
use std::time::Duration;

use nostos_application::ports::{Metrics, SnapshotError, SnapshotSource, SyncAuth};
use nostos_application::{ActiveRuleset, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{Lsn, ReplicationEvent, RowOp, TenantScope};
use nostos_infra::auth::AllowAnonymous;
use nostos_infra::iroh_sync::bind_sync_endpoint;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Two rows the snapshotter seeds for "tasks" — the pre-existing data a
/// freshly-connecting client must receive (ADR-0014).
struct SeedSnapshotter;

#[async_trait::async_trait]
impl SnapshotSource for SeedSnapshotter {
    async fn snapshot(
        &self,
        table: &str,
        base_lsn: Lsn,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        if table != "tasks" {
            return Ok(Vec::new());
        }
        let row = |pk: u64, title: &str| {
            let payload = format!("{{\"id\":\"{pk}\",\"title\":\"{title}\"}}");
            ReplicationEvent::new(
                Lsn::new(base_lsn.raw() + pk + 1),
                RowOp::Insert {
                    table: "tasks".into(),
                    pk: pk.to_string(),
                    payload: bytes::Bytes::copy_from_slice(payload.as_bytes()),
                },
            )
        };
        Ok(vec![row(1, "seed-one"), row(2, "seed-two")])
    }

    async fn snapshot_stream(
        &self,
        _table: &str,
        _predicate: &nostos_domain::PredicateExpr,
        _base_lsn: Lsn,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        Ok(Vec::new())
    }
}

/// The shared conformance leg: connect, subscribe, receive the seeded
/// snapshot, checkpoint, then reconnect once more — identical assertions
/// for both transports.
async fn conformance_leg(url: &str) {
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(800)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url.to_string(), storage, config);

    let first = tokio::time::timeout(Duration::from_secs(20), client.run_once())
        .await
        .expect("first session must not hang")
        .expect("first session completes");
    assert!(
        first.frames_received >= 2,
        "the seeded snapshot rows must arrive over the transport (got {})",
        first.frames_received
    );
    assert!(
        first.checkpoint > Lsn::ZERO,
        "the session must checkpoint past the snapshot"
    );

    // Reconnect over the same transport: idempotent apply — no duplicate
    // rows re-delivered as NEW frames beyond the snapshot re-send (resume
    // path with epoch + resume_lsn now set).
    let second = tokio::time::timeout(Duration::from_secs(20), client.run_once())
        .await
        .expect("second session must not hang")
        .expect("second session completes");
    assert!(
        second.checkpoint >= first.checkpoint,
        "checkpoint must never go backwards"
    );
}

fn build_app_state() -> SyncRouterState {
    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let state = SyncRouterState::new(manager, auth)
        .with_buffer(64)
        .with_metrics(metrics)
        .with_snapshotter(Arc::new(SeedSnapshotter));
    // Rules default to all-mode (zero-config), matching the default server.
    let _ = ActiveRuleset::all_mode();
    state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_over_ws() {
    let app = axum::Router::new()
        .route("/sync", axum::routing::get(sync_handler))
        .with_state(build_app_state());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);
    conformance_leg(&format!("ws://{addr}/sync")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_over_iroh() {
    // The native server shape (ADR-0041 D6): no HTTP listener in the session
    // path — the accept loop drives the session core directly.
    let endpoint = bind_sync_endpoint()
        .await
        .expect("bind the iroh sync endpoint");
    let url = endpoint.url("/sync");
    eprintln!("iroh dial url: {url}");
    let server = tokio::spawn(async move {
        endpoint.serve_sessions(build_app_state()).await;
    });
    std::mem::forget(server);

    conformance_leg(&url).await;
}

/// Rejects every token but `good-token` — pins that the iroh accept loop
/// enforces auth before any session state exists, and that `?token=` reaches
/// the verifier through the handshake the client runs over the QUIC stream.
struct FixedTokenAuth;

#[async_trait::async_trait]
impl SyncAuth for FixedTokenAuth {
    async fn authenticate(&self, token: &str) -> Option<nostos_domain::Principal> {
        (token == "good-token").then(nostos_domain::Principal::anonymous)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn iroh_auth_rejects_bad_token() {
    let state = {
        let metrics = Arc::new(Metrics::new());
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn SyncAuth> = Arc::new(FixedTokenAuth);
        SyncRouterState::new(manager, auth)
            .with_buffer(64)
            .with_metrics(metrics)
            .with_snapshotter(Arc::new(SeedSnapshotter))
    };
    let endpoint = bind_sync_endpoint()
        .await
        .expect("bind the iroh sync endpoint");
    let url = endpoint.url("/sync");
    let server = tokio::spawn(async move {
        endpoint.serve_sessions(state).await;
    });
    std::mem::forget(server);

    // Bad token: the close lands right after the handshake; whether the
    // client loop reports it as an early-close error or a zero-frame
    // session, no row may flow either way.
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    let bad = SyncClient::new(
        format!("{url}&token=bad-token"),
        storage,
        SyncClientConfig::default(),
    );
    let rejected = tokio::time::timeout(Duration::from_secs(20), bad.run_once())
        .await
        .expect("rejected session must not hang");
    if let Ok(outcome) = rejected {
        assert_eq!(
            outcome.frames_received, 0,
            "a rejected token must not receive session data"
        );
    }
    // Err: an early close surfacing as an error is also a rejection.

    // Good token through the same handshake plumbing: a full session.
    conformance_leg(&format!("{url}&token=good-token")).await;
}
