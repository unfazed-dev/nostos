//! Shared helpers for nostos-client integration tests.
//!
//! Reproduces the minimal in-process-server spawn pattern from
//! `nostos-infra/tests/common` (cross-crate test dirs can't share modules
//! without promoting helpers to public API, which would violate ponytail —
//! so the small duplication is intentional and documented).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;

use nostos_application::ports::SyncAuth;
use nostos_application::SessionManager;
use nostos_domain::Tier;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Spawn an in-process axum sync server on an ephemeral port with a FRESH
/// store, returning the bound address + the shared store (so the test can drive
/// a FanOutService against the same store the transport reads).
pub async fn spawn_server_with_store(
    buffer: usize,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<dyn nostos_application::ports::SessionStore>,
) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let (addr, handle) = spawn_server_with_existing_store(
        Arc::clone(&store),
        Arc::new(nostos_infra::AllowAnonymous::new()),
        buffer,
    )
    .await;
    (addr, handle, store)
}

/// Spawn the server against an EXISTING store + auth verifier — used by the
/// chaos test, which needs to drive multiple event batches through one store
/// while reconnecting the client.
pub async fn spawn_server_with_existing_store(
    store: Arc<dyn nostos_application::ports::SessionStore>,
    auth: Arc<dyn SyncAuth>,
    buffer: usize,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let manager = Arc::new(SessionManager::new(store, Tier::Enterprise));
    let state = SyncRouterState::new(Arc::clone(&manager), auth).with_buffer(buffer);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle)
}

/// A fresh temp dir for a durability test (stdlib only — no `tempfile` crate).
pub fn tempfile_dir() -> String {
    let base = std::env::temp_dir();
    let mut nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_string();
    nanos.push_str("-nostos-client-test");
    let dir = base.join(nanos);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.to_string_lossy().into_owned()
}
