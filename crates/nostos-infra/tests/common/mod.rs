//! Shared helpers for the nostos-infra integration tests.
//!
//! Pulled out of `e2e_pg_replication.rs` so the WS-contract smoke tests and the
//! PowerSync smoke tests can reuse the same subscribe/collect/decode logic.
//! This is the standard Rust `tests/common/mod.rs` convention: a module that is
//! `mod common;`-included by each integration test binary but never compiled as
//! its own test target.
//
// `allow(dead_code)`: each integration test binary compiles `common` separately,
// so a helper used only by one binary shows as unused when another compiles
// this module. Every item here is referenced by at least one test binary.
#![allow(dead_code)]
#![allow(clippy::cast_possible_truncation)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use nostos_application::ports::SyncAuth;
use nostos_application::SessionManager;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::AllowAnonymous;

/// A no-PG axum sync server bound to an ephemeral loopback port, with
/// `AllowAnonymous` auth (the pre-auth test shape — no tenant enforcement).
///
/// Returns the bound address, the server task handle, the `SessionManager`
/// (which the transport registers sessions through), AND the shared store —
/// so a test can build a `FanOutService` against the **same** store the live
/// WS transport is reading from, then drive synthetic events through it.
pub async fn spawn_fake_server(
    buffer: usize,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<SessionManager>,
    Arc<dyn nostos_application::ports::SessionStore>,
) {
    spawn_fake_server_with(buffer, Arc::new(AllowAnonymous::new()), None).await
}

/// Like [`spawn_fake_server`] but with an explicit auth verifier and optional
/// tenant column — used by the auth/enforcement tests.
pub async fn spawn_fake_server_with(
    buffer: usize,
    auth: Arc<dyn SyncAuth>,
    tenant_column: Option<&str>,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<SessionManager>,
    Arc<dyn nostos_application::ports::SessionStore>,
) {
    spawn_fake_server_with_tables(buffer, auth, tenant_column, Vec::new()).await
}

/// Like [`spawn_fake_server_with`] but also configures the writable-table
/// allowlist (ADR-0013) — used by the D2 write-back contract tests. Tables in
/// `write_tables` pass the transport's allowlist gate so the test can target
/// the next layer (NoWriteBack's fake-mode error, payload validation, etc.).
pub async fn spawn_fake_server_with_tables(
    buffer: usize,
    auth: Arc<dyn SyncAuth>,
    tenant_column: Option<&str>,
    write_tables: Vec<String>,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<SessionManager>,
    Arc<dyn nostos_application::ports::SessionStore>,
) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        store.clone(),
        nostos_domain::Tier::Enterprise,
    ));

    let mut state = SyncRouterState::new(Arc::clone(&manager), auth).with_buffer(buffer);
    if let Some(col) = tenant_column {
        state = state.with_tenant_column(col);
    }
    if !write_tables.is_empty() {
        let set: std::collections::HashSet<String> = write_tables.into_iter().collect();
        state = state.with_write_tables(set);
    }
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle, manager, store)
}

/// The JSON shape of a client's subscribe frame (matches the `Subscribe`
/// variant of `ClientMessage`). Sent as a WebSocket **text** message.
pub fn subscribe_frame(table: &str, filters: &[(&str, &str)]) -> String {
    subscribe_frame_with(table, filters, None)
}

/// A subscribe frame with an optional resume LSN (the client's last applied
/// checkpoint — drives the ack-driven resume path, ADR-0009).
pub fn subscribe_frame_with(
    table: &str,
    filters: &[(&str, &str)],
    resume_lsn: Option<u64>,
) -> String {
    let filters_json = if filters.is_empty() {
        String::from("[]")
    } else {
        let items: Vec<String> = filters
            .iter()
            .map(|(c, v)| format!("{{\"column\":\"{c}\",\"value\":\"{v}\"}}"))
            .collect();
        format!("[{}]", items.join(","))
    };
    let resume = match resume_lsn {
        Some(l) => format!(",\"resume_lsn\":{l}"),
        None => String::new(),
    };
    format!("{{\"type\":\"subscribe\",\"table\":\"{table}\",\"filters\":{filters_json}{resume}}}")
}

/// An ACK frame — the client confirms it has applied through `lsn`.
pub fn ack_frame(lsn: u64) -> String {
    format!("{{\"type\":\"ack\",\"lsn\":{lsn}}}")
}

/// Connect a WebSocket client, send the subscribe frame, and collect received
/// binary frames (parsed as JSON) until `timeout` elapses.
///
/// Mirrors the real client path: text subscribe → binary frames back. Returns
/// the parsed `serde_json::Value` of each received frame.
pub async fn subscribe_and_collect(
    addr: SocketAddr,
    table: &str,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let url = format!("ws://{addr}/sync");
    subscribe_and_collect_at(url, table.to_string(), Vec::new(), None, timeout).await
}

/// Connect with an optional bearer token (query-param form for browsers),
/// subscribe with optional filters + resume LSN, and collect received frames.
///
/// Takes owned `String`s so the returned future is `'static` (spawnable).
pub async fn subscribe_and_collect_at(
    url: String,
    table: String,
    filters: Vec<(String, String)>,
    token: Option<String>,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let full_url = match &token {
        Some(t) => format!("{url}?token={t}"),
        None => url,
    };
    let filters_ref: Vec<(&str, &str)> = filters
        .iter()
        .map(|(c, v)| (c.as_str(), v.as_str()))
        .collect();
    let (mut ws, _) = tokio_tungstenite::connect_async(&full_url)
        .await
        .expect("ws connect");
    ws.send(Message::Text(subscribe_frame_with(
        &table,
        &filters_ref,
        None,
    )))
    .await
    .unwrap();
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                got.push(v);
            }
        }
    }
    got
}

/// Decode the server's hex-encoded wire payload back to bytes, for assertions.
/// (The transport's `encode_event` hex-encodes payloads; a real client SDK
/// would decode the same way.)
pub fn decode_payload_hex(hex: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).unwrap_or(0);
        let lo = (bytes[i + 1] as char).to_digit(16).unwrap_or(0);
        out.push(u8::try_from(hi * 16 + lo).expect("hex pair fits in u8"));
        i += 2;
    }
    out
}
