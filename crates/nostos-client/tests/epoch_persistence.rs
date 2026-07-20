//! ADR-0025 F2 hardening: a real `SyncClient` persists the server's slot epoch
//! from the `resume_info` frame — the reconnect-resume gate signal.
//!
//! The raw-frame e2e (`nostos-infra/tests/e2e_pg_oplog_replay.rs`) drives the
//! epoch manually and proves the SERVER path (epoch-gate + op-log replay +
//! tenant-tagged deletes). It does NOT exercise the CLIENT wiring this test
//! isolates: server emits `resume_info` → client intercepts it (before the row
//! path) → `Storage::save_epoch` → durable read-back via `client.epoch()`.
//!
//! PG-free: the transport emits `resume_info` on EVERY subscribe with whatever
//! `slot_epoch` the `Metrics` handle holds, so a known non-zero epoch is
//! injected directly (no `PgReplicator` needed). That isolates the client link
//! from the Postgres path and keeps the test fast + hermetic.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use nostos_application::ports::{Metrics, SyncAuth};
use nostos_application::SessionManager;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::Storage; // trait method `epoch()` on SqliteStorage
use nostos_infra::auth::AllowAnonymous;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// The non-zero epoch the server advertises. Must differ from the fresh-DB
/// default (0) so a passing assertion proves the value flowed
/// server→resume_info→client→storage (not the trivial default).
const ADVERTISED_EPOCH: u64 = 7;

/// A real `SyncClient` connecting to a server that advertises a non-zero slot
/// epoch must persist it: after `run_once`, `client.epoch()` returns the
/// advertised value (not 0). This is the load-bearing client-side half of the
/// reconnect-resume gate (ADR-0025 F2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_client_persists_server_epoch_from_resume_info() {
    // Fresh in-memory storage: epoch defaults to 0 (the "no resume_info seen
    // yet" state → the next Subscribe sends epoch: None → snapshot).
    let storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    assert_eq!(
        storage.epoch().expect("epoch read"),
        0,
        "fresh DB epoch must be 0 (the test's baseline)"
    );

    // Server: anonymous auth + a Metrics whose slot_epoch we set to a known
    // non-zero value. register_subscribe reads this fresh + emits resume_info.
    let metrics = Arc::new(Metrics::new());
    metrics
        .slot_epoch
        .store(ADVERTISED_EPOCH, Ordering::Relaxed);
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let state = SyncRouterState::new(manager, auth)
        .with_buffer(64)
        .with_metrics(metrics);
    let app = axum::Router::new()
        .route("/sync", axum::routing::get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);

    // Real client over the real WS. Short idle_timeout: run_once returns once
    // the stream is "caught up" (resume_info arrives in ~ms; no data flows
    // because no replicator is attached, so it idles out promptly).
    let config = SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(500)),
        ..SyncClientConfig::default()
    };
    let url = format!("ws://{addr}/sync");
    let client = SyncClient::new(url, storage, config);

    // Drive one session: connect → subscribe → receive resume_info{epoch:7} →
    // persist → idle out → return.
    let outcome = client.run_once().await.expect("run_once completes");
    eprintln!(
        "run_once outcome: {} frames received",
        outcome.frames_received
    );

    // The load-bearing assertion: the advertised epoch is now durable. A 0 here
    // would mean the client never received/intercepted resume_info (the F2
    // wiring is broken), and every reconnect would force a full snapshot.
    let persisted = client.epoch().await.expect("epoch read after run_once");
    assert_eq!(
        persisted, ADVERTISED_EPOCH,
        "client must persist the server's advertised slot epoch from resume_info"
    );
}
