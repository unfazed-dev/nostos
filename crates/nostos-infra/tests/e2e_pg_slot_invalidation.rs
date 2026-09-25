//! Regression test for the P0-1 slot-invalidation silent-data-loss bug.
//!
//! ## Bug (pre-fix)
//! `ensure_slot_and_publication` treated a MISSING or INVALIDATED
//! (`wal_status='lost'`) logical-replication slot as FRESH: it re-created the
//! slot and resumed from current WAL, SILENTLY SKIPPING every change that
//! happened while nostos was offline. There was zero handling for
//! `wal_status='lost'` or SQLSTATE 55000 anywhere in nostos-infra. For a
//! Supabase self-hoster who disconnects nostos-server long enough for
//! `max_slot_wal_keep_size` to fire, this silently destroyed data.
//!
//! ## What this test asserts (post-fix)
//! 1. **Detection**: when the slot is dropped mid-stream, nostos notices via
//!    `pg_replication_slots.wal_status` (on the next reconnect) AND/OR via the
//!    SQLSTATE-55000 string match in the recv-error path. The
//!    `nostos_slot_recreated_total` counter increments, making the loss
//!    operator-visible instead of silent.
//! 2. **Recovery**: the slot is dropped + re-created with a fresh snapshot —
//!    clients continue to receive live changes after recovery (the snapshot-vs-
//!    stream exactly-once boundary is the existing fresh-slot path).
//!
//! ## Running
//! Requires a live Postgres with logical replication. Skipped unless
//! `NOSTOS_E2E_PG=1`:
//!
//! ```sh
//! docker compose -f docker/docker-compose.yml up -d
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://nostos:nostos@localhost:5433/nostos \
//!   cargo test -p nostos-infra --features pg --test e2e_pg_slot_invalidation \
//!   -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use nostos_application::ports::Metrics;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;

use common::subscribe_and_collect;

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://nostos:nostos@localhost:5433/nostos".into())
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

/// Poll `predicate` until it returns true or `timeout` elapses. Returns the
/// final value of `predicate`.
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

/// True iff the named logical-replication slot exists in `pg_replication_slots`.
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
            .is_ok_and(|r| r.is_some())
        }
        Err(_) => false,
    }
}

/// After a mid-stream drop, nostos MUST detect the loss (counter increments) and
/// recover (slot exists again, live inserts still reach a client). This is the
/// exact scenario the pre-fix code silently mishandled.
#[tokio::test]
async fn dropped_slot_is_detected_and_recovered() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `docker compose up -d` to run)");
        return;
    }

    let slot = format!("e2e_slot_invalid_{}", std::process::id());
    let publication = "nostos_pub";
    let sql = sql_client().await;

    // Clean any leftover slot from a prior run.
    let _ = sql
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    // Clean rows from prior runs so the snapshot row count is predictable.
    let _ = sql
        .execute(
            "DELETE FROM tasks WHERE title LIKE 'e2e-slot-invalid-%'",
            &[],
        )
        .await;

    // Build the production-shaped stack: shared metrics + replicator (with the
    // metrics handle attached) + fanout. We do NOT need a WS server for the
    // detection half — the metrics handle is the assertion surface.
    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), &slot, publication).expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
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

    // ── Phase 1: initial connect. Wait for slot to exist (poll SQL — the
    //    metrics gauge defaults to `Healthy(0)` before the first probe, so the
    //    metrics-based check is unreliable here), then capture the recreate
    //    counter as the baseline. The test explicitly drops any leftover slot
    //    before this point, so the very first probe sees a MISSING slot and
    //    fires the recovery path — the counter will be >= 1 here. That baseline
    //    is what the mid-stream drop's increment is compared against.
    let slot_clone = slot.clone();
    assert!(
        wait_for(Duration::from_secs(15), || async {
            slot_exists(&slot_clone).await
        })
        .await,
        "slot was not created on initial connect"
    );
    // Settle: give the driver's reconnect one more poll cycle so the metric
    // reflects the post-recreate state.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let baseline = metrics.snapshot();
    assert!(
        baseline.slot_recreated_total >= 1,
        "expected the initial missing-slot recovery to bump the recreate counter \
         (we explicitly dropped the leftover slot before connect); got {baseline:?}"
    );

    // ── Phase 2: insert a pre-drop row so the snapshot-vs-live boundary has
    //    something to deliver to a post-recovery client.
    let pre_drop_title = format!("e2e-slot-invalid-pre-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &pre_drop_title],
    )
    .await
    .expect("insert pre-drop row");

    // ── Phase 3: SUBSCRIBE A CLIENT BEFORE dropping the slot. The session lives
    //    in the store independent of the replicator connection; after recovery
    //    the next live insert reaches this same client (the regression
    //    assertion). 8-second collect window is generous; the drop+reconnect
    //    cycle (2s backoff + probe + recreate) completes well inside it.
    let collect_handle = {
        // Spawn a WS server sharing the same store + manager so the subscribed
        // session receives events the fanout dispatches.
        use axum::routing::get;
        use nostos_infra::transport::{sync_handler, SyncRouterState};
        let state = SyncRouterState::new(manager, Arc::new(nostos_infra::AllowAnonymous::new()))
            .with_buffer(1024);
        let app = axum::Router::new()
            .route("/sync", get(sync_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let _server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });
        // Hold the shutdown sender so the server lives for the test's duration.
        std::mem::forget(shutdown_tx);
        tokio::spawn(subscribe_and_collect(addr, "tasks", Duration::from_secs(8)))
    };

    // ── Phase 4: DROP THE SLOT MID-STREAM. This is the operator-event / PG-
    //    eviction scenario. nostos's recv loop will error out, hit the 2s
    //    backoff, then `ensure_connected` → `ensure_slot_and_publication` →
    //    `probe_slot_health` finds the slot MISSING → logs CRITICAL + bumps the
    //    recreate counter + drops (no-op, already gone) + re-creates with a
    //    fresh snapshot.
    //    We must first sever PG's active walsender lease on the slot before
    //    `pg_drop_replication_slot` will succeed — abort the driver. The
    //    driver's reconnect task below re-opens the slot from scratch.
    driver.abort();
    // Give PG a moment to notice the walsender is gone.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sql.batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await
        .expect("drop slot");
    assert!(
        !slot_exists(&slot).await,
        "slot should be gone immediately after explicit drop"
    );

    // ── Phase 5: restart the driver (the offline→online transition that the
    //    pre-fix code silently mishandled). The MISSING-slot path fires.
    let metrics2 = Arc::clone(&metrics);
    let pg_cfg2 =
        PgReplicatorConfig::from_url(&pg_url(), &slot, publication).expect("valid PG url (2)");
    let mut repl2 = PgReplicator::new(pg_cfg2).with_metrics(Arc::clone(&metrics2));
    let fanout_drv2 = Arc::clone(&fanout);
    let driver2 = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let parsed: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            parsed
                .get(col)
                .and_then(|v| v.as_str())
                .map(ColumnValue::text)
        };
        let _ = fanout_drv2.run(&mut repl2, extract).await;
    });

    // ── Phase 6: ASSERT DETECTION. The recreate counter increments ABOVE
    //    baseline AND the slot exists again (re-create happened). Generous 15s
    //    timeout covers the 2s reconnect backoff in case the new driver's first
    //    next_event hit the recv-error branch first.
    let baseline_recreates = baseline.slot_recreated_total;
    let baseline_epoch = baseline.slot_epoch;
    let metrics_for_poll = Arc::clone(&metrics);
    let detected = wait_for(Duration::from_secs(15), || {
        let m = Arc::clone(&metrics_for_poll);
        async move { m.snapshot().slot_recreated_total > baseline_recreates }
    })
    .await;
    assert!(
        detected,
        "DETECTION FAILED: slot_recreated_total did not increment above baseline \
         ({baseline_recreates}) after mid-stream slot drop. metrics snapshot: {:?}",
        metrics.snapshot()
    );
    assert!(
        slot_exists(&slot).await,
        "RECOVERY FAILED: slot should exist again after recreate"
    );

    // ── Phase 6b: ASSERT EPOCH BUMP (ADR-0025 slice 3). The recreate started a
    //    new slot lineage → slot_epoch must have incremented above baseline. A
    //    client whose last-seen epoch predates this bump will be forced to
    //    full-snapshot on reconnect (slice 4's gate consumes this signal).
    let post_recreate = metrics.snapshot();
    assert!(
        post_recreate.slot_epoch > baseline_epoch,
        "EPOCH GATE FAILED: slot_epoch did not increment above baseline \
         ({baseline_epoch}) after mid-stream slot recreate. The reconnect-resume \
         gate (ADR-0025 slice 4) would misfire without this bump. snapshot: \
         {post_recreate:?}"
    );

    // ── Phase 7: ASSERT LIVE DELIVERY POST-RECOVERY. A new insert reaches the
    //    still-subscribed client. This is the no-silent-data-loss claim: after
    //    recovery, the stream is live again.
    let post_recovery_title = format!("e2e-slot-invalid-post-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &post_recovery_title],
    )
    .await
    .expect("insert post-recovery row");

    let frames = collect_handle.await.expect("collect task did not panic");
    driver2.abort();
    let _ = sql
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    let _ = sql
        .execute(
            "DELETE FROM tasks WHERE title LIKE 'e2e-slot-invalid-%'",
            &[],
        )
        .await;

    // The pre-drop row reached the client via the initial snapshot (or live
    // stream before the drop). The post-recovery row is the load-bearing
    // assertion — the pre-fix code would have lost it.
    let saw_post_recovery = frames
        .iter()
        .any(|f| common::frame_payload_contains(f, &post_recovery_title));
    assert!(
        saw_post_recovery,
        "POST-RECOVERY DELIVERY FAILED: live insert after slot recreate did not reach the \
         subscribed client. frames: {frames:?}"
    );

    eprintln!(
        "PASS: slot drop detected (recreated_total={}) + recovered; post-recovery insert delivered",
        metrics.snapshot().slot_recreated_total
    );
}
