//! # sync_streams — the P5 "lazy parameterized stream" demo (ADR-0039).
//!
//! Demonstrates Nostos's sync streams end-to-end, in one process,
//! fully runnable with `cargo run -p nostos-client --example sync_streams`:
//!
//! 1. An in-process axum sync server (the real `/sync` handler) with a rules
//!    file carrying `[streams.mine]` = `owner_id = :owner AND priority >= :min`
//!    and a seeded `SnapshotSource` standing in for Postgres's pre-existing
//!    rows.
//! 2. A `SyncClient` whose BASE subscription is deliberately quiet
//!    (`status = 'archived'` — nothing matches), so every row that appears
//!    arrives via a STREAM, not the firehose.
//! 3. Mid-session, the client calls `sync_stream("mine", {"owner":"alice",
//!    "min":3}).subscribe()` — the LAZY add: a targeted snapshot backfills
//!    alice's pre-existing rows (2 of them), while bob's rows and alice's
//!    low-priority row stay invisible.
//! 4. Live fan-out through the stream: a matching insert arrives, two
//!    non-matching ones visibly do not.
//! 5. `unsubscribe()` stops the flow (another alice row does NOT arrive).
//! 6. Re-subscribing with `{"owner":"bob","min":1}` re-parameterizes lazily —
//!    bob's pre-existing rows backfill. This phase is also the regression
//!    proof for the sink ack-gate fix (`snapshot_base_lsn`): the socket has
//!    acked live traffic by now, and the snapshot still lands.
//!
//! The example IS the verification: it asserts row counts at every phase and
//! panics loudly if the stream path misbehaves. The one stand-in is
//! `SeedSnapshotter` (in lieu of `PgSnapshotter` → Postgres) — it evaluates
//! the same bound `PredicateExpr` the PG adapter turns into `$n` binds, so
//! the semantics demonstrated are the production ones.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::format_push_string,
    clippy::items_after_statements
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::routing::get;
use bytes::Bytes;
use tokio::time::sleep;

use nostos_application::ports::{
    ReplicatorStream, SessionStore, SnapshotError, SnapshotSource, SyncAuth,
};
use nostos_application::{ActiveRuleset, FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{
    ColumnValue, Lsn, PredicateExpr, ReplicationEvent, RowOp, StreamRule, SyncMode, SyncRules,
    TenantScope, Tier,
};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Build a `tasks` row event whose payload mirrors the PG tuple-image shape
/// (`PgReplicator::tuple_to_json_payload` — every value a string).
fn tasks_event(lsn: u64, owner: &str, status: &str, priority: i64) -> ReplicationEvent {
    let payload =
        format!("{{\"owner_id\":\"{owner}\",\"status\":\"{status}\",\"priority\":\"{priority}\"}}");
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: format!("task-{lsn}"),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        },
    )
}

/// The production column-extractor shape: parse the payload once, lift the
/// column as a `ColumnValue::Text` (typed coercion happens inside the
/// predicate engine, ADR-0012 slice 2).
fn extract_json(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    let parsed: serde_json::Value = serde_json::from_slice(event.payload_bytes()).ok()?;
    parsed
        .get(col)
        .and_then(|v| v.as_str())
        .map(ColumnValue::text)
}

/// A `SnapshotSource` standing in for `PgSnapshotter`: `tasks` starts EMPTY
/// for the table-level path, and `seed_rows` are the "pre-existing" rows a
/// lazy stream snapshot backfills. The bound predicate is evaluated with the
/// same `PredicateExpr::matches` semantics the live fan-out uses — the exact
/// contract the port documents for non-pg adapters.
struct SeedSnapshotter {
    seed_rows: Vec<(String, Vec<(String, ColumnValue)>)>,
}

#[async_trait]
impl SnapshotSource for SeedSnapshotter {
    async fn snapshot(
        &self,
        _table: &str,
        _base_lsn: Lsn,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        Ok(Vec::new()) // tasks starts empty — everything visible comes from streams
    }

    async fn snapshot_stream(
        &self,
        table: &str,
        predicate: &PredicateExpr,
        base_lsn: Lsn,
        _tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        let mut out = Vec::new();
        for (pk, cols) in &self.seed_rows {
            let view = |name: &str| {
                cols.iter()
                    .find(|(c, _)| *c == name)
                    .map(|(_, v)| v.clone())
            };
            if predicate.matches(view) {
                let mut payload = String::from("{");
                for (i, (k, v)) in cols.iter().enumerate() {
                    if i > 0 {
                        payload.push(',');
                    }
                    let text = match v {
                        ColumnValue::Text(s) => s.clone(),
                        other => format!("{other:?}"),
                    };
                    payload.push_str(&format!("\"{k}\":\"{text}\""));
                }
                payload.push('}');
                let lsn = base_lsn.raw() + 1 + out.len() as u64;
                out.push(ReplicationEvent::new(
                    Lsn::new(lsn),
                    RowOp::Insert {
                        table: table.to_string(),
                        pk: pk.clone(),
                        payload: Bytes::copy_from_slice(payload.as_bytes()),
                    },
                ));
            }
        }
        Ok(out)
    }
}

/// A replicator fed by an mpsc queue — the demo injects live events per
/// phase while the client stays connected (unlike reactive_scroll's fixed
/// script, the stream demo has multiple acts).
struct ScriptedStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<ReplicationEvent>,
}

#[async_trait]
impl ReplicatorStream for ScriptedStream {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        self.rx.recv().await
    }
}

/// Seed rows for the snapshotter: (pk, owner, status, priority).
fn seed_rows() -> Vec<(String, Vec<(String, ColumnValue)>)> {
    [
        ("seed-a1", "alice", "open", 5_i64),
        ("seed-a2", "alice", "open", 4),
        ("seed-a3", "alice", "open", 1), // low priority — must NOT backfill
        ("seed-b1", "bob", "open", 2),   // bob's — invisible until phase 6
        ("seed-b2", "bob", "closed", 1),
    ]
    .into_iter()
    .map(|(pk, owner, status, priority)| {
        (
            pk.to_string(),
            vec![
                ("owner_id".to_string(), ColumnValue::text(owner)),
                ("status".to_string(), ColumnValue::text(status)),
                (
                    "priority".to_string(),
                    ColumnValue::text(priority.to_string()),
                ),
            ],
        )
    })
    .collect()
}

/// Poll the in-process server's session store until exactly `want` sessions
/// remain (or panic after ~2s). The demo uses this to make the fire-and-forget
/// `unsubscribe()` observable: the client returns before the server has
/// processed the frame, and only the demo — which owns the server — can see
/// when the stream's session is really gone.
async fn wait_for_sessions(store: &Arc<dyn SessionStore>, want: usize) {
    for _ in 0..200 {
        if store.len().await == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "server still has {} sessions, wanted {want}",
        store.len().await
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(nostos_infra::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()))
        .try_init();

    println!("=== nostos sync_streams demo — P5 lazy parameterized streams (ADR-0039) ===");
    println!("server + client in one process; a quiet base subscription, so every");
    println!("row that appears arrives via a stream.\n");

    // --- The server: /sync + streams rules + the seeded snapshotter. ---
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(Arc::clone(&store), Tier::Enterprise));
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());

    let rules = SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::All,
        tables: vec![],
        hand: vec![],
        streams: vec![StreamRule {
            name: "mine".into(),
            table: "tasks".into(),
            template: "owner_id = :owner AND priority >= :min".into(),
        }],
    };
    let compiled = ActiveRuleset::compile(&rules).expect("rules compile");
    let rules_shared = Arc::new(tokio::sync::RwLock::new(compiled.clone()));
    let (rules_tx, rules_changed) = tokio::sync::watch::channel(compiled.checksum());

    let snapshotter: Arc<dyn SnapshotSource> = Arc::new(SeedSnapshotter {
        seed_rows: seed_rows(),
    });
    let state = SyncRouterState::new(Arc::clone(&manager), auth)
        .with_buffer(64)
        .with_snapshotter(snapshotter)
        .with_rules(rules_shared, rules_changed, rules_tx);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    println!("[server] listening on http://{addr}/sync");
    println!("[server] rules: [streams.mine] = \"owner_id = :owner AND priority >= :min\"");
    println!("[server] seeded pre-existing rows: alice x3 (pri 5,4,1), bob x2 (pri 2,1)\n");

    // The fan-out driver: events injected per phase via the channel.
    let (live_tx, live_rx) = tokio::sync::mpsc::unbounded_channel::<ReplicationEvent>();
    let svc = FanOutService::new(Arc::clone(&store));
    tokio::spawn(async move {
        let mut stream = ScriptedStream { rx: live_rx };
        svc.run(&mut stream, extract_json).await;
    });
    let live_lsn = AtomicU64::new(10); // live LSNs: 10, 20, 30, ...
    let pump = |owner: &str, status: &str, priority: i64| {
        let lsn = live_lsn.fetch_add(10, Ordering::Relaxed);
        live_tx
            .send(tasks_event(lsn, owner, status, priority))
            .unwrap();
    };

    // --- The client: durable SQLite; base subscription deliberately quiet. ---
    let db_path = format!(
        "{}/nostos-sync-streams-{}.db",
        std::env::temp_dir().to_string_lossy(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&db_path);
    let storage = SqliteStorage::open(&db_path).expect("open sqlite");
    let client = Arc::new(SyncClient::new(
        format!("ws://{addr}/sync"),
        storage,
        SyncClientConfig {
            table: "tasks".into(),
            token: Some("anon".into()),
            // The quiet stance: nothing live matches until an archived row
            // appears — every visible row in this demo arrives via a stream.
            where_sql: Some("status = 'archived'".into()),
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(500),
            idle_timeout: None, // long-lived: the demo disconnects explicitly
            ..SyncClientConfig::default()
        },
    ));
    let run_client = Arc::clone(&client);
    let run_task = tokio::spawn(async move { run_client.run_with_reconnect().await });

    /// Read the client's local tasks (pk, payload) sorted by pk.
    async fn local_rows(client: &SyncClient<SqliteStorage>) -> Vec<(String, String)> {
        client
            .with_storage(|s| {
                let conn = s.conn_for_test();
                let mut stmt = conn
                    .prepare(
                        "SELECT pk, payload FROM nostos_data WHERE table_name = 'tasks' ORDER BY pk",
                    )
                    .expect("prepare");
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            String::from_utf8_lossy(&r.get::<_, Vec<u8>>(1)?).into_owned(),
                        ))
                    })
                    .expect("query")
                    .map(|r| r.expect("row"))
                    .collect();
                rows
            })
            .await
            .expect("with_storage")
    }

    async fn phase(label: &str, client: &SyncClient<SqliteStorage>) -> Vec<(String, String)> {
        sleep(Duration::from_millis(500)).await; // let frames land + commit
        let rows = local_rows(client).await;
        println!("--- {label}: {} local row(s)", rows.len());
        for (pk, payload) in &rows {
            println!("    {pk}  {payload}");
        }
        rows
    }

    // --- Phase 1: quiet base — a live open row for carol matches nothing. ---
    println!("[phase 1] base subscription is `status = 'archived'` — deliberately quiet.");
    pump("carol", "open", 5);
    let rows = phase("phase 1: live carol row, base filtered it out", &client).await;
    assert!(rows.is_empty(), "the quiet base must deliver nothing");

    // --- Phase 2: the LAZY ADD — alice's pre-existing high-priority rows
    //     backfill via a targeted stream snapshot. ---
    println!("\n[phase 2] sync_stream(\"mine\", {{owner: alice, min: 3}}).subscribe() — lazy mid-session add");
    let alice = client
        .sync_stream(
            "mine",
            serde_json::json!({"owner": "alice", "min": 3})
                .as_object()
                .unwrap()
                .clone(),
        )
        .subscribe();
    let rows = phase(
        "phase 2: targeted snapshot backfilled alice's rows",
        &client,
    )
    .await;
    assert_eq!(rows.len(), 2, "exactly alice's pri>=3 rows backfill");
    assert!(rows.iter().all(|(_, p)| p.contains("alice")));

    // --- Phase 3: live fan-out through the stream predicate. ---
    println!("\n[phase 3] live: alice/pri9 (match), alice/pri1 (no), bob/pri9 (no)");
    pump("alice", "open", 9);
    pump("alice", "open", 1);
    pump("bob", "open", 9);
    let rows = phase("phase 3: only the matching live row landed", &client).await;
    assert_eq!(rows.len(), 3, "one live match joined the 2 backfilled rows");
    assert!(rows
        .iter()
        .any(|(_, p)| p.contains("\"priority\":\"9\"") && p.contains("alice")));

    // --- Phase 4: unsubscribe stops the flow. ---
    println!("\n[phase 4] unsubscribe() — another alice/pri9 row must NOT arrive");
    alice.unsubscribe();
    // `unsubscribe()` is fire-and-forget by design (ADR-0039 v1: no per-stream
    // ack frame) — the command still has to travel the socket and be processed
    // by the server's reader loop. Pumping immediately would race it and the
    // fan-out could deliver the row through the not-yet-removed stream session.
    // The demo owns the in-process server, so wait until the stream's session
    // is actually gone (base subscription = 1 remaining) before pumping.
    wait_for_sessions(&store, 1).await;
    pump("alice", "open", 9);
    let rows = phase("phase 4: post-unsubscribe live row blocked", &client).await;
    assert_eq!(rows.len(), 3, "no new rows after unsubscribe");

    // --- Phase 5: re-parameterize lazily — bob's rows backfill. This is also
    //     the regression proof for `snapshot_base_lsn`: the socket has acked
    //     live traffic by now, and the snapshot still clears the ack gate. ---
    println!(
        "\n[phase 5] sync_stream(\"mine\", {{owner: bob, min: 1}}).subscribe() — re-parameterize"
    );
    let _bob = client
        .sync_stream(
            "mine",
            serde_json::json!({"owner": "bob", "min": 1})
                .as_object()
                .unwrap()
                .clone(),
        )
        .subscribe();
    let rows = phase("phase 5: bob's pre-existing rows backfilled", &client).await;
    assert_eq!(rows.len(), 5, "bob's 2 rows joined alice's 3");
    assert_eq!(rows.iter().filter(|(_, p)| p.contains("bob")).count(), 2);

    // --- Phase 6: an unknown stream name is a non-fatal stream_error. ---
    println!("\n[phase 6] sync_stream(\"nope\", ...) — server answers stream_error (RUST_LOG=info to see it)");
    let _nope = client
        .sync_stream("nope", serde_json::Map::new())
        .subscribe();
    let rows = phase("phase 6: unknown stream changed nothing", &client).await;
    assert_eq!(rows.len(), 5);

    client.disconnect();
    let outcome = run_task
        .await
        .expect("run task")
        .expect("run_with_reconnect");
    println!(
        "\n[client] disconnected cleanly: {} frames received, {} commits, checkpoint {:?}",
        outcome.frames_received, outcome.commits, outcome.checkpoint
    );
    println!("=== demo complete: lazy subscribe → targeted backfill → live gate → unsubscribe → re-parameterize ===");
    let _ = std::fs::remove_file(&db_path);
}
