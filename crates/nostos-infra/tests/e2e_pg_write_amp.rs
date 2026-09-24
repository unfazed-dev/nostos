//! ADR-0025 slice 6 — real-Postgres `cairn_oplog` write-amplification harness.
//!
//! ADR-0025 §Consequences: "every WAL event now also writes a `cairn_oplog` row."
//! ADR-0026: "Real-Postgres `cairn_oplog` INSERT write-amplification is still
//! unmeasured... is the slice-6 open item." `NOSTOS_BENCH_OPLOG` is an in-process
//! recorder (0 PG rows) and `make bench` runs `FakeReplicator`, so the real-PG
//! amplification was never measured — this test closes that gap.
//!
//! Wires the production path (`PgReplicator` → `FanOutService.with_op_log(
//! PgOpLogWriter)`) against real PG in tenant mode, inserts N live rows for a
//! fresh tenant, and asserts the op-log grows **exactly 1:1** (one row per source
//! WAL event) with **zero drops**. Prints `amp` + `events/sec` for RESULTS.md.
//!
//! This is NOT a re-measurement of the 833k ops/sec moat — that number is
//! `FakeReplicator` eval-only by design and the oplog is opt-in/off that hot path
//! (fan-out-side cost already measured invisible in RESULTS.md). This measures the
//! real-PG `PgOpLogWriter` INSERT amplification specifically.
//!
//! ## Running
//! ```sh
//! docker compose -f docker/docker-compose.yml up -d
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn \
//!   cargo test -p nostos-infra --features pg --test e2e_pg_write_amp \
//!   -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use nostos_application::ports::{Metrics, OpLogWriter, SessionStore};
use nostos_application::FanOutService;
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::PgOpLogWriter;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const TENANT_COL: &str = "org_id";
/// Live rows inserted as post-snapshot WAL events (each → exactly one op-log row).
const N: i64 = 200;

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
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

async fn wait_for<F, Fut>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    predicate().await
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

/// `cairn_oplog` rows tagged for `tenant` (the production fan-out chokepoint wrote
/// them via `PgOpLogWriter`; `tenant_id = payload[tenant_column]`).
async fn oplog_count(tenant: &str) -> i64 {
    let c = sql_client().await;
    c.query_one(
        "SELECT count(*)::bigint FROM cairn_oplog WHERE tenant_id = $1",
        &[&tenant],
    )
    .await
    .expect("oplog count")
    .get(0)
}

/// Baseline op-log count for `tenant` is 0 by construction: `tenant` is a fresh
/// per-run UUID, and the op-log writer tags each row with the row's own
/// `org_id` (= this UUID), so no prior run's rows can match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::cast_precision_loss)] // row counts (≤~10³) cast to f64 for the amp ratio
async fn oplog_write_amplification_is_one_to_one_no_drops() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `docker compose up -d` to run)");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=info")
        .with_test_writer()
        .try_init();

    let tenant = uuid::Uuid::new_v4();
    let tenant_str = tenant.to_string();
    let slot = format!("e2e_write_amp_{}", std::process::id());

    let sql = sql_client().await;
    // Drop a leftover slot from a prior run (same PID ⇒ same slot name).
    let _ = sql
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    // Confirm the op-log table exists (docker/pg-init applies it).
    let _ = sql
        .query("SELECT 1 FROM cairn_oplog LIMIT 1", &[])
        .await
        .expect("cairn_oplog table exists (run docker compose up -d)");

    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let oplog: Arc<dyn OpLogWriter> = Arc::new(PgOpLogWriter::new(
        &pg_url(),
        Some(TENANT_COL.to_string()),
        4096,
        Some(Arc::clone(&metrics)),
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)).with_op_log(oplog));

    // Replicator driver. The slot is created on the LIVE table BEFORE the N
    // inserts, so the slot-creation snapshot excludes our rows — they enter
    // strictly as post-snapshot live WAL events (one op-log row each).
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let p: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            p.get(col).and_then(|v| v.as_str()).map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    // Wait for the slot to exist AND slot_epoch ≥ 1 (mirrors e2e_pg_oplog_replay —
    // the epoch bump lands a beat after slot creation, so slot_exists alone races).
    let metrics_for_epoch = Arc::clone(&metrics);
    let slot_owned = slot.clone();
    assert!(
        wait_for(Duration::from_secs(15), || {
            let m = Arc::clone(&metrics_for_epoch);
            let s = slot_owned.clone();
            async move { slot_exists(&s).await && m.snapshot().slot_epoch >= 1 }
        })
        .await,
        "slot was not created + epoch bumped on initial connect"
    );

    // Baseline: no op-log rows for this fresh tenant yet.
    assert_eq!(oplog_count(&tenant_str).await, 0, "fresh-tenant baseline");

    // N live inserts (each its own transaction ⇒ its own WAL event ⇒ one op-log
    // row). Stopwatch the drain from the first insert.
    let drain_start = Instant::now();
    for i in 0..N {
        sql_client()
            .await
            .execute(
                "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
                &[&tenant, &format!("write-amp-{i}")],
            )
            .await
            .expect("insert");
    }

    // Wait for the writer to flush all N (async batched flush loop).
    let landed = wait_for(Duration::from_secs(30), || {
        let t = tenant_str.clone();
        async move { oplog_count(&t).await >= N }
    })
    .await;
    let drain_secs = drain_start.elapsed().as_secs_f64();
    let count = oplog_count(&tenant_str).await;

    let dropped = metrics.snapshot().oplog_dropped;
    // count/N are row counts (≤ ~10³) — the i64→f64 cast loses no real precision,
    // but clippy pedantic flags it, so allow at the fn level (see attribute above).
    let amp = count as f64 / N as f64;
    let events_per_sec = N as f64 / drain_secs.max(1e-9);

    eprintln!(
        "[write-amp] tenant={tenant_str} inserted={N} oplog_rows={count} amp={amp:.3} \
         dropped={dropped} drain={drain_secs:.2}s events/sec={events_per_sec:.0}"
    );

    assert!(
        landed,
        "op-log never reached {N} rows (got {count}) — writer stalled"
    );
    assert_eq!(
        count, N,
        "AMPLIFICATION: expected exactly {N} op-log rows (1:1), got {count} (amp {amp:.3})"
    );
    assert_eq!(
        dropped, 0,
        "oplog_dropped={dropped}: the op-log dropped under a {N}-event load (should be 0)"
    );

    driver.abort();
    let _ = sql_client()
        .await
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    // Clean up the rows we inserted so the shared DB doesn't grow across runs.
    let _ = sql_client()
        .await
        .execute("DELETE FROM tasks WHERE org_id = $1", &[&tenant])
        .await;
}
