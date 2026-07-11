//! Real-`PgReplicator` + real `nostos_client::SyncClient` end-to-end proof.
//!
//! This exact combination — a real tokio `SyncClient` driving a real
//! `ApplyEngine`/`SqliteStorage` against a real Postgres logical-replication
//! stream — had never been exercised anywhere in this repo before this file:
//! `nostos-infra`'s own e2e suite drives a raw `tokio-tungstenite` client, and
//! `nostos-client`'s test suite never touched `PgReplicator`. That gap is
//! exactly why the launch-blocking bug this file proves-fixed survived (see
//! `docs/adr/0016-client-sdk-and-wal-bloat-protection.md`'s addendum).
//!
//! Two scenarios, both against an otherwise-idle real Postgres table:
//!
//! 1. [`single_external_write_on_idle_table_applies_and_advances_checkpoint`]
//!    — the PRIMARY bug. `ApplyEngine::feed` used to buffer a solitary
//!    transaction's frames forever (nothing ever closed the boundary on an
//!    idle table). Proves the new `flush_quiesce` bound closes it within
//!    seconds, and the row becomes visible via `SqliteStorage::rows_for`
//!    with the checkpoint advanced.
//! 2. [`write_enqueued_mid_session_reaches_postgres_without_reconnect`] — the
//!    SECONDARY bug. `SyncClient::write()` called AFTER the connection is
//!    already established used to sit in the outbox until the next
//!    reconnect (the startup flush is one-shot). Proves the new
//!    `write_notify` wakeup gets it onto the wire — and all the way through
//!    a real `PgWriteBack` into Postgres — without a reconnect.
//!
//! ## Running
//!
//! Requires a live Postgres with logical replication (`make pg-up`) and the
//! `tasks` table (`docker/pg-init/01-sources.sql`). Skipped unless
//! `NOSTOS_E2E_PG=1` is set, so it never breaks PG-less CI:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-client --features pg --test e2e_pg_sync -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use tokio::sync::oneshot;

use nostos_application::ports::WriteBack;
use nostos_application::{FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{Outbox, PendingWrite, WriteOp};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, Tier};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::{AllowAnonymous, PgWriteBack};

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

/// Connect a control-plane SQL client (tokio-postgres) for setup/teardown and
/// for asserting what actually landed in Postgres, independent of the client
/// under test.
async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// A fresh temp dir for the client's durable SQLite file (stdlib only, no
/// `tempfile` crate — mirrors `crates/nostos-client/tests/common::tempfile_dir`,
/// duplicated rather than shared per that module's own documented reasoning).
fn tempfile_dir() -> String {
    let base = std::env::temp_dir();
    let mut nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_string();
    nanos.push_str("-nostos-client-e2e-pg-sync");
    let dir = base.join(nanos);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.to_string_lossy().into_owned()
}

/// Spawn the in-process axum sync server wired to a REAL `PgReplicator` (read
/// path). `write_back` optionally wires a real `PgWriteBack` for `tasks`
/// (scenario 2 needs it; scenario 1 — a purely external write — does not).
async fn spawn_server(
    slot: &str,
    publication: &str,
    write_back: bool,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(Arc::clone(&store), Tier::Enterprise));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    // Replicator driver: PgReplicator → FanOutService. No predicate/tenant
    // filtering in play (anonymous auth, no where_sql), so a trivial
    // extractor is enough — matches the shape nostos-client's own
    // FakeReplicator-driven chaos tests already use.
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), slot, publication).expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg);
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        let extract =
            |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> { Some(ColumnValue::Any) };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    let mut state =
        SyncRouterState::new(Arc::clone(&manager), Arc::new(AllowAnonymous::new())).with_buffer(64);
    if write_back {
        let mut allowlist = HashSet::new();
        allowlist.insert("tasks".to_string());
        let wb: Arc<dyn WriteBack> = Arc::new(PgWriteBack::new(&pg_url(), allowlist.clone()));
        state = state.with_write_back(wb).with_write_tables(allowlist);
    }
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

/// Clean shutdown + wait for the slot to be released, matching the pattern
/// `nostos-infra`'s e2e_pg_* tests use (a second connection to the same slot
/// hangs until the first's lease is dropped).
async fn shutdown_server(
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    driver: tokio::task::JoinHandle<()>,
    slot: &str,
) {
    let _ = shutdown.send(());
    driver.abort();
    let _ = server.await;
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
                .ok()
                .is_some_and(|r| r.get::<_, bool>(0))
            }
            Err(_) => false,
        };
        if !active {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Drop a leftover slot from a prior run (best-effort — "does not exist" is
/// not an error we care about).
async fn drop_stale_slot(slot: &str) {
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;
}

/// THE PRIMARY launch-blocking bug, proven fixed: a client that is already
/// connected and settled on an otherwise-idle table receives ONE row from an
/// external writer (direct SQL — nothing about this write goes through the
/// client under test). Before this fix, that single transaction's frame(s)
/// had no follow-up frame to close the boundary and buffered in
/// `ApplyEngine::feed` forever. `flush_quiesce` now closes it within a bounded
/// window.
#[tokio::test]
async fn single_external_write_on_idle_table_applies_and_advances_checkpoint() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_client_sync_{}_solo", std::process::id());
    let publication = "cairn_pub";
    drop_stale_slot(&slot).await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication, false).await;
    // Give the replicator a moment to connect + create the slot before the
    // client subscribes (mirrors nostos-infra's e2e_pg_* timing).
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dir = tempfile_dir();
    let db_path = format!("{dir}/solo-write.sqlite");
    let storage = SqliteStorage::open(&db_path).unwrap();
    let client = Arc::new(SyncClient::new(
        format!("ws://{addr}/sync"),
        storage,
        SyncClientConfig {
            table: "tasks".to_string(),
            // Long-lived: no session-level idle disconnect. If the fix
            // regressed to "only flushes on the next frame or reconnect",
            // this test would time out rather than pass — the quiesce
            // mechanism is what closes the batch here, not idle_timeout.
            idle_timeout: None,
            ..SyncClientConfig::default()
        },
    ));
    let run_client = Arc::clone(&client);
    let run_task = tokio::spawn(async move { run_client.run_once().await });

    // Let the subscribe land and the session settle — genuinely idle, no
    // traffic at all — before the one external write happens.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let row_id = uuid::Uuid::new_v4();
    let org_id = uuid::Uuid::new_v4();
    let title = format!("solo-write-{row_id}");
    {
        let sql = sql_client().await;
        sql.execute(
            "INSERT INTO tasks (id, org_id, title) VALUES ($1, $2, $3)",
            &[&row_id, &org_id, &title],
        )
        .await
        .expect("direct insert");
    }

    // Poll the CLIENT's own storage (not a second file handle — SQLite
    // locking/WAL semantics aren't the thing under test here) for a bounded
    // time. Comfortably above network + replication latency and the default
    // 50ms flush_quiesce; comfortably below "looks like it hung forever."
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut landed = false;
    while tokio::time::Instant::now() < deadline {
        let rows = client
            .with_storage(|s: &SqliteStorage| s.rows_for("tasks"))
            .await
            .expect("with_storage join")
            .expect("rows_for");
        if rows.iter().any(|(pk, _)| pk == &row_id.to_string()) {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        landed,
        "a solitary write on an otherwise-idle table must apply within a \
         bounded time (flush_quiesce) — before this fix it buffered forever \
         (ApplyEngine::feed never saw a follow-up frame to close the \
         transaction boundary)"
    );

    let checkpoint = client.checkpoint().await.expect("checkpoint");
    assert!(
        checkpoint > Lsn::ZERO,
        "checkpoint must advance past the applied write, not stay at zero \
         while the row silently sits unflushed"
    );

    run_task.abort();
    shutdown_server(shutdown, server, driver, &slot).await;
}

/// THE SECONDARY bug, proven fixed: a write enqueued via `SyncClient::write`
/// AFTER the connection is already established (not part of the startup
/// backlog flush) must still reach Postgres without waiting for a reconnect.
/// Asserted by querying Postgres DIRECTLY (bypassing this client's own
/// read-back entirely) — this is the wire round-trip, not just "the outbox
/// has an entry."
#[tokio::test]
async fn write_enqueued_mid_session_reaches_postgres_without_reconnect() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_client_sync_{}_midwrite", std::process::id());
    let publication = "cairn_pub";
    drop_stale_slot(&slot).await;

    let (addr, shutdown, server, driver) = spawn_server(&slot, publication, true).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dir = tempfile_dir();
    let db_path = format!("{dir}/mid-session-write.sqlite");
    let storage = SqliteStorage::open(&db_path).unwrap();
    let client = Arc::new(SyncClient::new(
        format!("ws://{addr}/sync"),
        storage,
        SyncClientConfig {
            table: "tasks".to_string(),
            idle_timeout: None,
            ..SyncClientConfig::default()
        },
    ));
    let run_client = Arc::clone(&client);
    let run_task = tokio::spawn(async move { run_client.run_once().await });

    // Let the connection fully establish AND its startup outbox-flush
    // (empty — nothing queued yet) complete, before enqueueing anything.
    // This is exactly the race that used to strand a write until reconnect:
    // `write()` here happens well after `run_once`'s one-shot pre-loop
    // flush has already run.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let row_id = uuid::Uuid::new_v4();
    let org_id = uuid::Uuid::new_v4();
    let title = format!("mid-session-{row_id}");
    client
        .write(PendingWrite {
            table: "tasks".to_string(),
            op: WriteOp::Upsert,
            pk: row_id.to_string(),
            payload_json: Some(format!(
                r#"{{"id":"{row_id}","org_id":"{org_id}","title":"{title}"}}"#
            )),
        })
        .await
        .expect("enqueue mid-session write");

    let sql = sql_client().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut landed = false;
    while tokio::time::Instant::now() < deadline {
        if sql
            .query_opt("SELECT title FROM tasks WHERE id = $1", &[&row_id])
            .await
            .expect("query tasks")
            .is_some()
        {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        landed,
        "a write enqueued mid-session (after the connection is already \
         established) must reach Postgres without waiting for a reconnect — \
         before this fix, run_once's outbox flush was a one-shot, pre-loop \
         step with nothing to wake it again"
    );

    // The outbox drains once the WriteResult ack lands.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut drained = false;
    while tokio::time::Instant::now() < deadline {
        let pending = client
            .with_storage(|s: &SqliteStorage| Outbox::pending(s))
            .await
            .expect("with_storage join")
            .expect("pending");
        if pending.is_empty() {
            drained = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(drained, "the outbox must drain once the write is ack'd");

    run_task.abort();
    shutdown_server(shutdown, server, driver, &slot).await;
}
