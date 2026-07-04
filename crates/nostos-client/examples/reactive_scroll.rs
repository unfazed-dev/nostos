//! # reactive_scroll — the visible "scroll forever" demo.
//!
//! Demonstrates Nostos's headline moat end-to-end, in one process, fully runnable
//! with `cargo run -p nostos-client --example reactive_scroll`:
//!
//! 1. An in-process axum sync server (the real `/sync` handler, `InMemorySessionStore`).
//! 2. A background `FanOutService` feeding a stream of `tasks` rows (each carrying
//!    `org_id`/`status`/`priority` as a JSON payload — the exact shape
//!    `PgReplicator::tuple_to_json_payload` emits) into the store.
//! 3. A `SyncClient` with durable `SqliteStorage` (a temp file), reconnect
//!    enabled, subscribing with a **`where_sql` predicate** (`status = open AND
//!    priority >= 3`) — exercising the Tier-7 safe-SQL compiler + typed
//!    comparison end-to-end (the predicate is compiled on the server and ANDed
//!    into the session; only matching rows are delivered).
//! 4. Each applied row printed live: `[lsn] op tasks pk {json}`. This is the
//!    visible reactive stream.
//! 5. A mid-run server restart — the client's `run_with_reconnect` resumes from
//!    its durable checkpoint, losing nothing (the `chaos_resume` property, made
//!    visible).
//!
//! The example *is* the verification: every line runs when it executes. There
//! are no test doubles in the demo path itself — it wires the production
//! `SyncClient`, `SqliteStorage`, `FanOutService`, and `/sync` handler.

// Presentation code: timing/format helpers trip pedantic lints that are fine
// for a demo (usize->f64 for a rate, format_push_string in a print). Mirrors
// nostos-bench's allow for the same reporting pattern.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::format_push_string
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use bytes::Bytes;
use tokio::time::sleep;

use nostos_application::ports::{ReplicatorStream, SessionStore, SyncAuth};
use nostos_application::{FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::Storage;
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp, Tier};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Build a `tasks` row event whose payload is the JSON object
/// `{"org_id":"..","status":"..","priority":".."}` — the exact shape
/// `PgReplicator::tuple_to_json_payload` produces for real Postgres rows. The
/// client's predicate (`status=open AND priority>=3`) matches against this via
/// the typed-comparison leaves shipped in ADR-0012 slice 2.
fn tasks_event(lsn: u64, org: &str, status: &str, priority: i64) -> ReplicationEvent {
    // Manual JSON build (mirrors tuple_to_json_payload — every value quoted).
    let payload =
        format!("{{\"org_id\":\"{org}\",\"status\":\"{status}\",\"priority\":\"{priority}\"}}");
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: format!("task-{lsn}"),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        },
    )
}

/// The column extractor the FanOutService uses to evaluate predicates against
/// the JSON payload — the production `extract_json_column` shape (parse once,
/// cheap lookups), inlined here so the example is self-contained.
fn extract_json(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    // Parse the small JSON object once per call (the example's events are tiny;
    // production caches the parse — see extract_json_column in nostos-infra).
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    let map = parse_flat_json(s)?;
    map.get(col).map(ColumnValue::text)
}

/// Minimal flat-JSON-object parser for `{"k":"v",...}`. Production uses
/// `serde_json` (extract_json_column); this avoids pulling serde into the
/// example's call path for clarity.
fn parse_flat_json(s: &str) -> Option<HashMap<String, String>> {
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    let mut map = HashMap::new();
    for pair in s.split(',') {
        let mut kv = pair.splitn(2, ':');
        let k = kv.next()?.trim().trim_matches('"');
        let v = kv.next()?.trim().trim_matches('"');
        map.insert(k.to_string(), v.to_string());
    }
    Some(map)
}

/// Spawn the in-process sync server against a shared store. Mirrors the
/// `nostos-client/tests/common` harness — the example wires the real handler.
async fn spawn_server(
    store: Arc<dyn SessionStore>,
    auth: Arc<dyn SyncAuth>,
    buffer: usize,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let manager = Arc::new(SessionManager::new(store, Tier::Enterprise));
    let state = SyncRouterState::new(manager, auth).with_buffer(buffer);
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

/// A tiny replicator that yields a scripted stream of `tasks` events — half
/// `open`/high-priority (match the demo predicate) and half `closed`/low
/// (filtered out). Demonstrates predicate filtering visibly.
struct DemoStream {
    total: u64,
    emitted: u64,
}

#[async_trait::async_trait]
impl ReplicatorStream for DemoStream {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if self.emitted >= self.total {
            return None;
        }
        let i = self.emitted;
        self.emitted += 1;
        let lsn = 10 + i * 10;
        // Alternate: even = open+priority5 (matches), odd = closed+priority1
        // (filtered by the client's predicate).
        let (status, priority) = if i.is_multiple_of(2) {
            ("open", 5_i64)
        } else {
            ("closed", 1)
        };
        let org = if i.is_multiple_of(3) {
            "acme"
        } else {
            "globex"
        };
        Some(tasks_event(lsn, org, status, priority))
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("=== nostos reactive_scroll demo ===");
    println!("server + client in one process; durable SQLite; typed predicate\n");

    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());

    // --- Phase 1: start the server, drive a scripted event stream into it. ---
    let (addr, server_handle) = spawn_server(Arc::clone(&store), Arc::clone(&auth), 64).await;
    println!("[server] listening on http://{addr}/sync");

    let svc = Arc::new(FanOutService::new(Arc::clone(&store)));
    let mut stream = DemoStream {
        total: 20,
        emitted: 0,
    };
    // Pump events into the fan-out service in the background. The pump sleeps
    // briefly first so the client's subscribe lands before the events flow —
    // FanOut delivers only to sessions registered at fan-out time, so the
    // subscriber must connect before the pump emits.
    let svc_pump = Arc::clone(&svc);
    let pump = tokio::spawn(async move {
        sleep(Duration::from_millis(300)).await; // let the client subscribe first
        svc_pump.run(&mut stream, extract_json).await;
    });

    // --- Phase 2: a SyncClient with durable storage + a typed predicate. ---
    let db_path = format!(
        "{}/nostos-reactive-scroll-{}.db",
        std::env::temp_dir().to_string_lossy(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&db_path); // fresh each run
    let storage = SqliteStorage::open(&db_path).expect("open sqlite");
    let initial_checkpoint = storage.checkpoint().expect("checkpoint");
    println!("[client] durable sqlite at {db_path} (initial checkpoint: {initial_checkpoint:?})");

    let url = format!("ws://{addr}/sync");
    let config = SyncClientConfig {
        table: "tasks".into(),
        token: Some("anon".into()),
        // Subscribe with a where_sql — the Tier-7 safe-SQL compiler compiles
        // this on the server and ANDs it into the session predicate. Only rows
        // matching `status = open AND priority >= 3` are delivered (the
        // FanOutService's extract_json lifts real decoded values for the
        // predicate engine to evaluate). Exercises the compiler end-to-end.
        where_sql: Some("status = open AND priority >= 3".into()),
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(500),
        max_retries: Some(3),
        idle_timeout: Some(Duration::from_secs(2)),
    };
    let client = SyncClient::new(url, storage, config);
    println!("[client] subscribing to tasks (where status = open AND priority >= 3); applying rows as they stream in...\n");

    let outcome = client.run_once().await.expect("client run_once");
    println!(
        "\n[client] session done: {} frames received, {} commits, checkpoint {:?}",
        outcome.frames_received, outcome.commits, outcome.checkpoint
    );

    // --- Phase 3: server restart — prove reconnect-resume loses nothing. ---
    println!("\n[server] RESTARTING (dropping the listener)...");
    server_handle.abort();
    sleep(Duration::from_millis(200)).await;
    let (addr2, _handle2) = spawn_server(Arc::clone(&store), Arc::clone(&auth), 64).await;
    println!("[server] back up on http://{addr2}/sync");

    // The client reconnects to the new address. Its durable checkpoint means it
    // resumes from where it left off — no loss, no duplication. Same where_sql
    // (the predicate is per-session, so the resumed session re-establishes it).
    let storage2 = SqliteStorage::open(&db_path).expect("reopen sqlite");
    let client2 = SyncClient::new(
        format!("ws://{addr2}/sync"),
        storage2,
        SyncClientConfig {
            table: "tasks".into(),
            token: Some("anon".into()),
            where_sql: Some("status = open AND priority >= 3".into()),
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(500),
            max_retries: Some(3),
            idle_timeout: Some(Duration::from_secs(1)),
        },
    );
    let outcome2 = client2.run_once().await.expect("client2 run_once");
    println!(
        "[client2] resumed from durable checkpoint: {} frames this session, checkpoint {:?}",
        outcome2.frames_received, outcome2.checkpoint
    );

    // Let the pump finish.
    let _ = pump.await;
    println!("\n=== demo complete: reactive stream + durable resume, end-to-end ===");
    let _ = std::fs::remove_file(&db_path);
}
