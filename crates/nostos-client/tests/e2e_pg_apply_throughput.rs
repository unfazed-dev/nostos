//! Real-Postgres → client-apply END-TO-END throughput harness.
//!
//! Measures the FULL production path sustained rate — Postgres WAL logical
//! decoding (PgReplicator) → FanOutService → real WebSocket → real SyncClient →
//! ApplyEngine → durable SqliteStorage commits — for a bulk-loaded
//! dedicated-table workload. This is the number the Show-HN precondition (RESULTS.md, operator
//! decision 2026-08-19) asked for: a real-path figure alongside the eval-only
//! 833k fan-out ceiling.
//!
//! ## Honest framing (docs/BENCHMARK-METHODOLOGY.md discipline)
//!
//! This is a distinct kind of number from the existing one:
//! - vs the 833k ops/sec aggregate fan-out ceiling: different path (that one is
//!   FakeReplicator→fan-out→WS with NO decode and NO client apply).
//!
//! Loopback networking; single server process; single client; op-log OFF
//! (opt-in writer absent, mirroring the headline bench's configuration).
//!
//! ## Method
//!
//! 1. Slot created on the LIVE (empty-of-ours) table BEFORE any inserts, so
//!    every row streams as a post-snapshot live WAL event.
//! 2. Client subscribes FIRST (fan-out delivers only to registered sessions),
//!    then N rows land as batched multi-row INSERTs (BATCH rows/transaction).
//! 3. Stopwatch starts at the first INSERT and stops when the client's
//!    durable SQLite holds >=N rows matching this run's unique prefix.
//! 4. Prints ops/sec + duration + fan-out matched/delivered/dropped counters.
//!
//! ## Running
//!
//! \`\`\`sh
//! docker compose -f docker/docker-compose.yml up -d
//! NOSTOS_E2E_PG=1 cargo test -p nostos-client --features pg \
//!   --test e2e_pg_apply_throughput -- --nocapture --test-threads=1
//! # Optional: NOSTOS_APPLY_TP_N=50000 cargo test ... --release
//! \`\`\`

#![cfg(feature = "pg")]
// Row counts (i64→usize/f64) feed rates and asserts only; bounds are harness-
// controlled, so truncation/sign cannot bite. House pattern: throughput.rs.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::uninlined_format_args)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use nostos_application::ports::{Metrics, SessionStore, SyncAuth};
use nostos_application::FanOutService;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{ColumnValue, ReplicationEvent, Tier};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::AllowAnonymous;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
/// Rows bulk-loaded and expected on the client. Override: NOSTOS_APPLY_TP_N.
const DEFAULT_N: i64 = 20_000;
/// Rows per INSERT transaction (one WAL transaction per batch).
const BATCH: i64 = 500;
/// Per-session sink buffer. Default matches the server's production default;
/// override via NOSTOS_APPLY_TP_BUF (e.g. 32768) to absorb the startup burst
/// and measure the zero-drop sustained path.
fn session_buffer() -> usize {
    std::env::var("NOSTOS_APPLY_TP_BUF")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024)
}

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

async fn drop_slot(slot: &str) {
    if let Ok((c, conn)) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await {
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let _ = c
            .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
            .await;
    }
}

async fn slot_exists(slot: &str) -> bool {
    match tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await {
        Ok((c, conn)) => {
            tokio::spawn(async move {
                let _ = conn.await;
            });
            c.query_opt(
                "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|o| o.is_some())
        }
        Err(_) => false,
    }
}

/// Dedicated bench table: EMPTY at subscribe time, so the first-connect
/// snapshot is zero rows and the measured window is pure live WAL path.
/// (Reusing `tasks` re-floods the sink with every leftover row from every
/// prior run — observed drop→resnapshot storms that never converge.)
const TABLE: &str = "bench_apply";

/// Idempotently create + publish + empty the bench table.
async fn setup_bench_table() {
    let c = sql_client().await;
    c.batch_execute(
        "CREATE TABLE IF NOT EXISTS bench_apply (
            id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            org_id     UUID NOT NULL,
            title      TEXT NOT NULL,
            completed  BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        ALTER TABLE bench_apply REPLICA IDENTITY FULL;
        DO $$ BEGIN
            ALTER PUBLICATION cairn_pub ADD TABLE public.bench_apply;
        EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
    )
    .await
    .expect("setup bench_apply table");
    // Empty start: any prior run's leftovers would otherwise re-snapshot.
    c.batch_execute("TRUNCATE bench_apply;")
        .await
        .expect("truncate bench_apply");
}

/// Take the bench table back OUT of `cairn_pub` and empty it.
///
/// # Why this is not optional housekeeping
///
/// Leaving `bench_apply` published is a **cross-suite test-isolation leak that
/// silently breaks unrelated e2e tests.** This bench leaves ~40k rows behind;
/// `cairn_pub` is shared, so every later test that opens a FRESH replication
/// slot snapshots those rows too. Tests with a fixed event budget
/// (`collect_events(&mut repl, 8, ..)`) then fill that budget with bench rows
/// before their own row ever arrives, and fail with a message that points at
/// the snapshot/stream boundary — nowhere near the actual cause.
///
/// That is not hypothetical: it is exactly what made
/// `e2e_pg_snapshot::{fresh_slot_yields_snapshot_rows_then_live_stream,
/// concurrent_writes_during_snapshot_appear_exactly_once}` fail
/// deterministically, survive a revert to a pre-session baseline (it is
/// database state, not code), and get recorded in the v0.2.0 security audit as
/// "root cause not established". Both pass the moment the table leaves the
/// publication.
///
/// ponytail: teardown runs on the success path only — a panicking bench still
/// leaves the table published. Ceiling: a crashed bench re-poisons the suite
/// until the next clean run. Upgrade path: a `Drop` guard, which needs a
/// blocking SQL handle in `Drop`; not worth it until a bench actually panics
/// here. The cheap manual antidote is one line:
/// `ALTER PUBLICATION cairn_pub DROP TABLE bench_apply;`
async fn teardown_bench_table() {
    // Connect defensively like `drop_slot`, NOT via `sql_client()` — that
    // helper `.expect()`s on connect, and a teardown that panics turns a bench
    // that already passed its assertions into a red run. Teardown must never
    // be able to fail the test it is cleaning up after.
    let Ok((c, conn)) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await else {
        return;
    };
    tokio::spawn(async move {
        let _ = conn.await;
    });
    // Both statements are best-effort and separate: `ALTER PUBLICATION ... DROP
    // TABLE` ERRORS if the table is not currently published (it is not a
    // no-op), and the two run in one implicit transaction if batched together,
    // so that error would take the TRUNCATE down with it. Discarding each
    // independently means "already unpublished" is simply the state we wanted.
    let _ = c
        .batch_execute("ALTER PUBLICATION cairn_pub DROP TABLE bench_apply;")
        .await;
    let _ = c.batch_execute("TRUNCATE bench_apply;").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_pg_to_client_apply_sustained_throughput() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with docker compose up -d to run)");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=warn")
        .with_test_writer()
        .try_init();

    let n: i64 = std::env::var("NOSTOS_APPLY_TP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_N);
    assert!(
        n >= BATCH && n % BATCH == 0,
        "N must be a multiple of {BATCH}"
    );
    let tenant = uuid::Uuid::new_v4();
    let slot = format!("e2e_apply_tp_{}", std::process::id());
    // Per-run unique title prefix: accumulated rows from prior runs share the
    // table, and only OUR prefix may ever count toward this measurement.
    let run_tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let title_prefix = format!("apply-tp-{run_tag}-");

    // Slot hygiene: clear every inactive harness slot (crashed prior runs leak
    // them — compose headroom is 20; see the ponytail in docker-compose.yml).
    let hygiene = sql_client().await;
    let _ = hygiene
        .batch_execute(
            "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
             WHERE slot_name LIKE 'e2e_apply_tp%' AND active = false;",
        )
        .await;
    drop(hygiene);
    setup_bench_table().await;

    // --- server spine (real handler, real fan-out, real replicator) ---
    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(nostos_application::SessionManager::new(
        Arc::clone(&store),
        Tier::Enterprise,
    ));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let state = SyncRouterState::new(manager, auth).with_buffer(session_buffer());
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let fanout =
        Arc::new(FanOutService::new(Arc::clone(&store)).with_metrics(Arc::clone(&metrics)));
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
    let fanout_drv = Arc::clone(&fanout);
    let _driver = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let p: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            p.get(col).and_then(|v| v.as_str()).map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    // Slot live + epoch bumped before we load (mirrors e2e_pg_write_amp).
    let wait_start = Instant::now();
    loop {
        let epoch_ok = metrics.snapshot().slot_epoch >= 1;
        if slot_exists(&slot).await && epoch_ok {
            break;
        }
        assert!(
            wait_start.elapsed() < Duration::from_secs(20),
            "slot was not created + epoch bumped on initial connect"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // --- client subscribes BEFORE the load lands ---
    let db_path =
        std::env::temp_dir().join(format!("nostos-apply-tp-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let storage =
        SqliteStorage::open(db_path.to_str().expect("utf8 db path")).expect("open sqlite");
    let url = format!("ws://{addr}/sync");
    let config = SyncClientConfig {
        table: TABLE.into(),
        token: Some("anon".into()),
        base_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_secs(2),
        max_retries: Some(10),
        idle_timeout: Some(Duration::from_mins(1)),
        ..SyncClientConfig::default()
    };
    let client = Arc::new(SyncClient::new(url, storage, config));
    let run_client = Arc::clone(&client);
    let run_task = tokio::spawn(async move {
        let _ = run_client.run_once().await;
    });
    tokio::time::sleep(Duration::from_millis(800)).await; // subscribe registration

    // --- drain the initial whole-table SNAPSHOT before measuring ---
    // A fresh client triggers a targeted snapshot of every published row
    // (ADR-0009 first-connect reconcile). With drop-on-full sink semantics,
    // that flood can drop events — including live ones arriving mid-flood
    // (observed: matched=29009 delivered=1000 on a 28k-row table). The
    // measured window below must be PURE live path, so wait until the
    // snapshot fully drains (client row total stable across polls).
    let count_all_sql: &'static str = "SELECT count(*) FROM cairn_data";
    let mut prev_total: i64 = -1;
    let settle_deadline = Instant::now() + Duration::from_mins(3);
    loop {
        assert!(
            Instant::now() < settle_deadline,
            "initial snapshot never drained"
        );
        let cur_total = client
            .with_storage(move |s| {
                let conn = s.conn_for_test();
                conn.query_row(count_all_sql, [], |r| r.get::<_, i64>(0))
                    .unwrap_or(-1)
            })
            .await
            .unwrap_or(-2);
        if cur_total == prev_total {
            break;
        }
        prev_total = cur_total;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let pre = metrics.snapshot();

    // --- bulk load: BATCH-row transactions, stopwatch from first commit ---
    let sql = sql_client().await;
    let batches = n / BATCH;
    let start = Instant::now();
    for b in 0..batches {
        let titles: Vec<String> = (0..BATCH)
            .map(|i| format!("{title_prefix}{}", b * BATCH + i))
            .collect();
        sql.execute(
            &format!(
                "INSERT INTO {TABLE} (org_id, title) SELECT * FROM unnest($1::uuid[], $2::text[])"
            ),
            &[&vec![tenant; BATCH as usize], &titles],
        )
        .await
        .expect("batch insert");
    }

    // --- wait until N of OUR rows are DURABLY applied in client SQLite ---
    // Counting only our tenant-prefix rows makes the figure immune to stray
    // events from other tenants' leftovers sharing the tasks publication.
    let deadline = Instant::now() + Duration::from_mins(5);
    let count_sql = format!(
        "SELECT count(*) FROM cairn_data WHERE payload LIKE '%\"title\":\"{title_prefix}%'"
    );
    let mut db_count: i64 = 0;
    loop {
        if Instant::now() >= deadline {
            let m = metrics.snapshot();
            eprintln!(
                "STALL: applied={db_count} matched={} delivered={} dropped={} faulted={}",
                m.matched, m.delivered, m.dropped, m.faulted
            );
            panic!("client never applied {n} rows within 300s");
        }
        let count_sql_call = count_sql.clone();
        db_count = client
            .with_storage(move |s| {
                let conn = s.conn_for_test();
                conn.query_row(&count_sql_call, [], |r| r.get::<_, i64>(0))
                    .unwrap_or(0)
            })
            .await
            .unwrap_or(-1);
        if db_count >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let total = start.elapsed(); // load+decode+fanout+wire+apply wall clock
    let applied = db_count as usize;

    let post = metrics.snapshot();
    // In-window counters only: the cumulative ones include the warmup flood.
    let window_matched = post.matched.saturating_sub(pre.matched);
    let window_delivered = post.delivered.saturating_sub(pre.delivered);
    let window_dropped = post.dropped.saturating_sub(pre.dropped);
    let secs = total.as_secs_f64().max(1e-9);
    let ops_per_sec = applied as f64 / secs;

    eprintln!("\n=== REAL-PG TO CLIENT-APPLY E2E RESULT ===");
    eprintln!("rows applied          : {applied} (requested {n})");
    eprintln!("total wall clock      : {secs:.3}s");
    eprintln!("end-to-end rate       : {ops_per_sec:.0} rows/sec");
    eprintln!(
        "window matched/delivered/dropped  : {}/{}/{}",
        window_matched, window_delivered, window_dropped
    );
    eprintln!("faulted={} slot_epoch={}", post.faulted, post.slot_epoch);
    eprintln!(
        "config: batch={BATCH} buffer={} oplog=off loopback single-client",
        session_buffer()
    );

    assert_eq!(
        applied, n as usize,
        "exactly the loaded rows must be applied"
    );
    assert_eq!(
        window_dropped, 0,
        "zero-drop contract violated inside the measured live window"
    );

    // PG-side ground truth for the RESULTS.md entry.
    let verify = sql_client().await;
    let total_tasks: i64 = verify
        .query_one(&format!("SELECT count(*) FROM {TABLE}"), &[])
        .await
        .expect("count bench table")
        .get(0);
    let mine: i64 = verify
        .query_one(
            &format!("SELECT count(*) FROM {TABLE} WHERE title LIKE '{title_prefix}%'"),
            &[],
        )
        .await
        .expect("count mine")
        .get(0);
    eprintln!("pg verify: source_total={total_tasks} source_mine={mine} client_applied={db_count}");

    run_task.abort();
    client.disconnect();
    drop_slot(&slot).await;
    // Unpublish before leaving: a published 40k-row bench table breaks every
    // later fresh-slot e2e test in the workspace. See `teardown_bench_table`.
    teardown_bench_table().await;
    let _ = std::fs::remove_file(&db_path);
}

/// ADR-0040 ratified behavior: at the DEFAULT buffer (1024) an unpaced burst
/// sheds events, the server signals `resync_required`, and the client clears +
/// reconciles — the run must still converge to ALL N rows, eventually.
/// Requires NOSTOS_RESYNC_SIGNAL on the server state (set here directly).
///
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resync_signal_recovers_capacity_shed_at_default_buffer() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with docker compose up -d to run)");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=warn")
        .with_test_writer()
        .try_init();

    let n: i64 = std::env::var("NOSTOS_APPLY_TP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_N);
    assert!(n >= BATCH && n % BATCH == 0);
    let tenant = uuid::Uuid::new_v4();
    let slot = format!("e2e_apply_tp_{}", std::process::id());
    drop_slot(&slot).await;

    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(nostos_application::SessionManager::new(
        Arc::clone(&store),
        Tier::Enterprise,
    ));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    // THE DIFFERENCE: default buffer + signal ON — and a REAL snapshotter,
    // because post-resync recovery rides the snapshot-reconcile path.
    let run_tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let title_prefix = format!("apply-tp-{run_tag}-");
    let state = SyncRouterState::new(manager, auth)
        .with_buffer(session_buffer())
        .with_resync_signal(true)
        .with_snapshotter(Arc::new(nostos_infra::PgSnapshotter::new(&pg_url())));
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let fanout =
        Arc::new(FanOutService::new(Arc::clone(&store)).with_metrics(Arc::clone(&metrics)));
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").expect("url");
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
    let fanout_drv = Arc::clone(&fanout);
    tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let p: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            p.get(col).and_then(|v| v.as_str()).map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    let wait_start = Instant::now();
    loop {
        if metrics.snapshot().slot_epoch >= 1 && slot_exists(&slot).await {
            break;
        }
        assert!(wait_start.elapsed() < Duration::from_secs(20), "no slot");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let db_path =
        std::env::temp_dir().join(format!("nostos-apply-resync-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let storage = SqliteStorage::open(db_path.to_str().expect("utf8")).expect("open sqlite");
    let client = Arc::new(SyncClient::new(
        format!("ws://{addr}/sync"),
        storage,
        SyncClientConfig {
            table: TABLE.into(),
            token: Some("anon".into()),
            base_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(2),
            max_retries: Some(20),
            idle_timeout: Some(Duration::from_mins(1)),
            ..SyncClientConfig::default()
        },
    ));
    let run_client = Arc::clone(&client);
    let run_task = tokio::spawn(async move {
        // Reconnecting loop: after `resync_required` -> clear + disconnect, this
        // resumes fresh (epoch None) and snapshot-reconciles the whole table.
        let _ = run_client.run_with_reconnect().await;
    });
    tokio::time::sleep(Duration::from_millis(800)).await;

    let sql = sql_client().await;
    let batches = n / BATCH;
    for b in 0..batches {
        let titles: Vec<String> = (0..BATCH)
            .map(|i| format!("{title_prefix}{}", b * BATCH + i))
            .collect();
        sql.execute(
            &format!(
                "INSERT INTO {TABLE} (org_id, title) SELECT * FROM unnest($1::uuid[], $2::text[])"
            ),
            &[&vec![tenant; BATCH as usize], &titles],
        )
        .await
        .expect("batch insert");
    }

    // Eventual correctness: count reaches N even though the burst shed.
    let deadline = Instant::now() + Duration::from_mins(5);
    let count_sql = format!(
        "SELECT count(*) FROM cairn_data WHERE payload LIKE '%\"title\":\"{title_prefix}%'"
    );
    let mut db_count: i64;
    let mut next_progress = Instant::now();
    loop {
        assert!(
            Instant::now() < deadline,
            "resync never converged to {n} rows within 300s"
        );
        db_count = client
            .with_storage({
                let count_sql = count_sql.clone();
                move |s| {
                    let conn = s.conn_for_test();
                    conn.query_row(&count_sql, [], |r| r.get::<_, i64>(0))
                        .unwrap_or(0)
                }
            })
            .await
            .unwrap_or(-1);
        if db_count >= n {
            break;
        }
        if Instant::now() >= next_progress {
            let m = metrics.snapshot();
            eprintln!(
                "[resync-progress] applied={db_count} matched={} delivered={} dropped={}",
                m.matched, m.delivered, m.dropped
            );
            next_progress = Instant::now() + Duration::from_secs(5);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "=== ADR-0040 RESYNC RECOVERY: converged to {db_count}/{n} rows at default buffer {} ===",
        session_buffer()
    );

    run_task.abort();
    client.disconnect();
    drop_slot(&slot).await;
    // Unpublish before leaving: a published 40k-row bench table breaks every
    // later fresh-slot e2e test in the workspace. See `teardown_bench_table`.
    teardown_bench_table().await;
    let _ = std::fs::remove_file(&db_path);
}
