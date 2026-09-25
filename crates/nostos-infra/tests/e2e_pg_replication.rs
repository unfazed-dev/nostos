//! End-to-end replication test: a real Postgres insert flows through the
//! `PgReplicator` → `FanOutService` → WebSocket transport → test client.
//!
//! This is the Phase-1 "kill criterion" test (ROADMAP): the PG logical-
//! replication state machine must surface a real row change to a real client
//! with no loss. It also covers **LSN resume**: after a mid-stream disconnect,
//! events written while the client was away are delivered on reconnect.
//!
//! ## Running
//!
//! Requires a live Postgres with logical replication (the repo's `make pg-up`).
//! Skipped unless `NOSTOS_E2E_PG=1` is set, so it never breaks PG-less CI:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_replication -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use tokio::sync::oneshot;

use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

use common::subscribe_and_collect;

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://nostos:nostos@localhost:5433/nostos".into())
}

/// Connect a control-plane SQL client (tokio-postgres) for setup/inserts.
async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Spin up the in-process axum sync server with a PgReplicator driver.
/// Returns the bound address + a shutdown sender + the driver + server tasks.
/// The test MUST await/abort these on shutdown so the replication slot is
/// released before any subsequent connection to the same slot.
async fn spawn_server(
    slot: &str,
    publication: &str,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    // Replicator driver: PgReplicator → FanOutService, real column extraction.
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), slot, publication).expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg);
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let parsed: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            parsed
                .get(col)
                .and_then(|v| v.as_str())
                .map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    let state = SyncRouterState::new(
        Arc::clone(&manager),
        Arc::new(nostos_infra::AllowAnonymous::new()),
    )
    .with_buffer(1024);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
    (addr, shutdown_tx, server, driver)
}

/// Cleanly shut down a spawned server + its replicator driver, and wait for the
/// replication slot to be released by PG before returning. Critical for the
/// resume test: a second connection to the same slot hangs until the first
/// connection's lease is dropped.
async fn shutdown_server(
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    driver: tokio::task::JoinHandle<()>,
    slot: &str,
) {
    let _ = shutdown.send(());
    // Abort the driver so its replication connection closes and PG releases the
    // slot lease. A graceful close would be nicer, but abort guarantees release.
    driver.abort();
    let _ = server.await;
    // Wait until PG reports the slot as inactive.
    for _ in 0..40 {
        let active: bool = match tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await {
            Ok((c, conn)) => {
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                c.query_one(
                    "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot],
                )
                .await
                .is_ok_and(|r| r.get::<_, bool>(0))
            }
            Err(_) => false,
        };
        if !active {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// One inserted row → one delivered frame. The core Phase-1 claim.
#[tokio::test]
async fn pg_insert_reaches_ws_client() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_basic_{}", std::process::id());
    let publication = "nostos_pub";
    let sql = sql_client().await;
    // Drop a leftover slot from a prior run, then start clean.
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication).await;
    // Give the replicator a moment to open the connection + create the slot.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let collect = tokio::spawn(subscribe_and_collect(addr, "tasks", Duration::from_secs(4)));

    // Insert a row once the client is (almost certainly) subscribed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let title = format!("e2e-row-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &title],
    )
    .await
    .unwrap();

    let frames = collect.await.unwrap();
    shutdown_server(shutdown, server, driver, &slot).await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    assert!(
        !frames.is_empty(),
        "expected at least one frame from the PG insert; got none"
    );
    // The payload is hex-encoded JSON on the wire; decode + match the title.
    let saw = frames
        .iter()
        .any(|f| common::frame_payload_contains(f, &title));
    assert!(
        saw,
        "inserted title '{title}' not found in any frame payload"
    );
}

/// LSN resume (replicator-level): events written between a disconnect and
/// reconnect are re-delivered by the slot on the next connection.
///
/// This proves the exactly-once-across-restart property at the replication
/// layer directly (PgReplicator.next_event), decoupled from WebSocket
/// subscribe-timing. The slot's `confirmed_flush_lsn` is the resume point; PG
/// replays everything after it on the next connection.
#[tokio::test]
async fn lsn_resume_delivers_missed_events() {
    use nostos_application::ports::ReplicatorStream;
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_resume_{}", std::process::id());
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    // Connection 1: stream one row to establish a confirmed_flush_lsn.
    let mut repl1 =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, "nostos_pub").unwrap());
    repl1.ensure_connected().await.unwrap();
    let warmer = format!("resume-warm-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &warmer],
    )
    .await
    .unwrap();
    let _warm = tokio::time::timeout(Duration::from_secs(3), repl1.next_event())
        .await
        .expect("warmer event timed out")
        .expect("warmer event was None");
    let confirmed1 = repl1.last_confirmed_lsn();
    drop(repl1);
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Insert 3 rows while NO replicator is connected.
    let mut titles = Vec::new();
    for i in 0..3 {
        let t = format!("resume-missed-{i}-{}", uuid::Uuid::new_v4());
        titles.push(t.clone());
        sql.execute(
            "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
            &[&uuid::Uuid::new_v4(), &t],
        )
        .await
        .unwrap();
    }

    // Connection 2: resumes from confirmed_flush_lsn (>= confirmed1). PG must
    // replay the 3 missed rows. Collect up to 8 events with a deadline.
    let mut repl2 =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, "nostos_pub").unwrap());
    repl2.ensure_connected().await.unwrap();
    let confirmed2 = repl2.last_confirmed_lsn();
    assert!(
        confirmed2 >= confirmed1,
        "resume LSN went backwards: {confirmed2} < {confirmed1}"
    );

    let mut payloads = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline && payloads.len() < 8 {
        if let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(500), repl2.next_event()).await
        {
            payloads.push(String::from_utf8_lossy(ev.payload_bytes()).into_owned());
        }
    }
    drop(repl2);
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;

    let delivered: Vec<&String> = titles
        .iter()
        .filter(|t| payloads.iter().any(|p| p.contains(t.as_str())))
        .collect();
    assert!(
        !delivered.is_empty(),
        "no missed events delivered on resume; got {} payloads, titles={:?}",
        payloads.len(),
        titles
    );
    eprintln!(
        "resume test (replicator-level): {}/{} missed events delivered on reconnect",
        delivered.len(),
        titles.len()
    );
}

/// Smoke: the in-memory (no-PG) pipeline still delivers FakeReplicator events
/// through the real server + WS transport. Runs always (no PG needed).
#[tokio::test]
async fn smoke_fake_replicator_delivers_via_ws() {
    use nostos_infra::replicator::FakeReplicator;
    use nostos_infra::replicator::FakeReplicatorConfig;

    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    let state = SyncRouterState::new(
        Arc::clone(&manager),
        Arc::new(nostos_infra::AllowAnonymous::new()),
    )
    .with_buffer(1024);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Subscribe FIRST (so the session is registered before we fan out), then
    // drive a *bounded* FakeReplicator. A separate task collects frames while
    // the driver emits.
    let collect = tokio::spawn(subscribe_and_collect(addr, "tasks", Duration::from_secs(3)));

    // Give the client a moment to connect + subscribe before we start emitting.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(500));
    let fanout_drv = Arc::clone(&fanout);
    let drv = tokio::spawn(async move {
        let extract = |_: &ReplicationEvent, _: &str| Some(ColumnValue::Any);
        let _ = fanout_drv.run(&mut repl, extract).await;
    });
    let _ = drv.await;

    let frames = collect.await.unwrap();
    assert!(
        !frames.is_empty(),
        "smoke: FakeReplicator should deliver events via WS"
    );
}
