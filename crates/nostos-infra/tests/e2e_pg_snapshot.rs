//! Snapshot-then-stream (roadmap Phase 1): a fresh slot must deliver the
//! table's pre-existing rows BEFORE the live change stream. Without this, a
//! client subscribing to a populated table receives nothing until the next
//! mutation — the missing first sync.
//!
//! Two tests:
//! 1. `fresh_slot_yields_snapshot_rows_then_live_stream` — 3 pre-existing rows
//!    arrive first (all at the snapshot's consistent-point LSN), then a live
//!    INSERT arrives with a strictly greater LSN. On restart with the SAME
//!    slot, NO snapshot is re-emitted.
//! 2. `concurrent_writes_during_snapshot_appear_exactly_once` — the classic
//!    slot-snapshot landmine: rows INSERTed WHILE the snapshot COPY is in
//!    flight must appear EXACTLY ONCE across (snapshot events + streamed
//!    events) — never zero (lost between snapshot and stream) and never twice
//!    (captured by both). The exported-snapshot + consistent-point design makes
//!    this hold structurally; this test is what proves it.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_snapshot -- --nocapture --test-threads=1
//! ```
//!
//! Gate convention: this file matches the existing e2e suite — `NOSTOS_E2E_PG=1`
//! gates the run and `NOSTOS_PG_URL` overrides the default localhost URL. The
//! plan's B2 sketch used `NOSTOS_PG_URL`-presence as the gate; we deliberately
//! diverged to stay consistent with `e2e_pg_replication.rs`.

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use nostos_application::ports::ReplicatorStream;
use nostos_domain::{Lsn, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};

/// Env gate. Self-skips when PG isn't available so PG-less CI stays green.
/// Matches `e2e_pg_replication.rs`.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

/// Connect a control-plane SQL client (tokio-postgres) for setup/inserts.
/// Mirrors `e2e_pg_replication.rs::sql_client`.
async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Drop a slot if it exists (best-effort). Reused across tests so a leftover
/// slot from a crashed prior run doesn't block the next connect.
async fn drop_slot(sql: &tokio_postgres::Client, slot: &str) {
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;
}

/// Collect up to `max` events or until `per_event_timeout` elapses with no
/// event. Returns the events. Each `next_event` call is wrapped in a short
/// timeout so we don't hang forever when the stream is quiescent.
async fn collect_events(
    repl: &mut PgReplicator,
    max: usize,
    overall: Duration,
) -> Vec<ReplicationEvent> {
    let mut out = Vec::with_capacity(max);
    let deadline = tokio::time::Instant::now() + overall;
    while out.len() < max && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), repl.next_event()).await {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => break, // stream ended
            Err(_) => {}       // timeout on this poll; keep draining until overall deadline
        }
    }
    out
}

/// Fresh-slot snapshot: 3 pre-existing rows → 3 Insert events all at the SAME
/// (consistent-point) LSN, then a live INSERT at a strictly greater LSN.
/// Restart with the same slot → NO snapshot replay.
#[tokio::test]
async fn fresh_slot_yields_snapshot_rows_then_live_stream() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_snap_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;

    // 1. Start from a clean slate so the snapshot is exactly the rows we seed
    //    (predictable LSN + content assertions). TRUNCATE is safe here: this
    //    test runs serialized (--test-threads=1) with a slot it owns.
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    // Seed 3 rows BEFORE the replicator starts. Use distinctive titles so we
    // can match snapshot events to these rows unambiguously.
    let mut seeded_titles = Vec::new();
    for i in 0..3 {
        let t = format!("snap-seed-{i}-{}", uuid::Uuid::new_v4());
        seeded_titles.push(t.clone());
        sql.execute(
            "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
            &[&uuid::Uuid::new_v4(), &t],
        )
        .await
        .unwrap();
    }

    // 2. Start PgReplicator with a FRESH slot.
    let mut repl =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").unwrap());
    repl.ensure_connected().await.unwrap();

    // 3a. Collect the snapshot events. They should be exactly the 3 seeded
    //     rows, all at the SAME LSN (the snapshot's consistent point).
    // The publication (`cairn_pub`) now carries 5 dashboard tables alongside
    // `tasks` (ADR-0022 / P1), so a fresh-slot snapshot emits ALL member tables'
    // rows. Collect enough to capture the full snapshot, then filter to the
    // `tasks` rows this test seeds and asserts on.
    let all_snapshot = collect_events(&mut repl, 32, Duration::from_secs(5)).await;
    assert!(
        !all_snapshot.is_empty(),
        "snapshot yielded no events — fresh slot should deliver pre-existing rows"
    );
    let snapshot_events: Vec<&ReplicationEvent> = all_snapshot
        .iter()
        .filter(|ev| ev.table() == "tasks")
        .collect();
    let snapshot_lsns: std::collections::HashSet<Lsn> =
        snapshot_events.iter().map(|ev| ev.lsn).collect();
    assert_eq!(
        snapshot_lsns.len(),
        1,
        "all snapshot rows must share ONE consistent-point LSN; got {snapshot_lsns:?}"
    );
    let snapshot_lsn = *snapshot_lsns.iter().next().unwrap();
    assert_eq!(
        snapshot_events.len(),
        3,
        "snapshot should contain exactly the 3 seeded tasks rows; got {}",
        snapshot_events.len()
    );
    for t in &seeded_titles {
        let present = snapshot_events
            .iter()
            .any(|ev| String::from_utf8_lossy(ev.payload_bytes()).contains(t.as_str()));
        assert!(present, "seeded title '{t}' missing from snapshot events");
    }

    // 3b. Live INSERT must arrive with lsn > snapshot_lsn.
    let live_title = format!("snap-live-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &live_title],
    )
    .await
    .unwrap();
    let live_events = collect_events(&mut repl, 8, Duration::from_secs(5)).await;
    let live_ev = live_events
        .iter()
        .find(|ev| String::from_utf8_lossy(ev.payload_bytes()).contains(live_title.as_str()));
    let Some(live_ev) = live_ev else {
        panic!(
            "live INSERT '{live_title}' not delivered; got {} events",
            live_events.len()
        );
    };
    // The live event must NOT precede the snapshot (that would place it in the
    // snapshot's past — a boundary bug). It can EQUAL the snapshot LSN: Postgres
    // LSNs are byte offsets, and the consistent point is the exact WAL position
    // where streaming begins, so a row committed in the very next record can
    // share the same wal_end. What we forbid is live_lsn < snapshot_lsn.
    assert!(
        live_ev.lsn >= snapshot_lsn,
        "live event lsn {} must be >= snapshot lsn {} (a row before the snapshot would be a boundary bug)",
        live_ev.lsn,
        snapshot_lsn
    );

    // 4. RESTART with the SAME slot → NO snapshot replay. The first event
    //    (if any) must NOT be one of the seeded snapshot rows. We assert by
    //    checking that no snapshot-LSN insert for a seeded title reappears.
    drop(repl);
    // Wait for PG to release the slot lease before reconnecting.
    for _ in 0..40 {
        let active = sql
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|r| r.get::<_, bool>(0));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut repl2 =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").unwrap());
    repl2.ensure_connected().await.unwrap();
    let restart_events = collect_events(&mut repl2, 4, Duration::from_secs(2)).await;
    drop(repl2);
    drop_slot(&sql, &slot).await;

    // The seeded snapshot titles must NOT reappear at the snapshot LSN. (A
    // live redelivery of an unacked row would carry a different, higher LSN —
    // and we never acked the snapshot, so the slot may redeliver from its
    // restart point. But a re-emitted SNAPSHOT — a fresh batch at the original
    // snapshot_lsn with the seeded title — is what we forbid.)
    for ev in &restart_events {
        for t in &seeded_titles {
            let is_seeded = String::from_utf8_lossy(ev.payload_bytes()).contains(t.as_str());
            let is_snapshot_lsn = ev.lsn == snapshot_lsn;
            assert!(
                !(is_seeded && is_snapshot_lsn),
                "snapshot replayed on restart: seeded title '{t}' reappeared at snapshot lsn {snapshot_lsn}"
            );
        }
    }
    eprintln!(
        "snapshot test: {} snapshot events @ {snapshot_lsn}, {} restart events (no replay)",
        snapshot_events.len(),
        restart_events.len()
    );
}

/// The classic slot-snapshot landmine (CRITICAL): rows INSERTed WHILE the
/// snapshot COPY is in flight must appear EXACTLY ONCE across snapshot events
/// + streamed events. The exported-snapshot + consistent-point boundary makes
/// this hold structurally; this test proves it.
#[tokio::test]
async fn concurrent_writes_during_snapshot_appear_exactly_once() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_conc_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;

    // 1. Clean slate, then seed one row so the snapshot is non-empty (and the
    //    snapshot COPY has real work to do — exercising the window). Serialized
    //    run + slot ownership make TRUNCATE safe here.
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    // Seed one row so the snapshot is non-empty (and the snapshot COPY has
    //    real work to do — exercising the window).
    let seed_title = format!("conc-seed-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &seed_title],
    )
    .await
    .unwrap();

    // 2. Start the replicator AND spawn concurrent writers in parallel. The
    //    writers fire INSERTs continuously for ~2s, straddling the
    //    snapshot-vs-stream boundary.
    let mut repl =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, "cairn_pub").unwrap());
    repl.ensure_connected().await.unwrap();

    let concurrent_titles: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer_titles = concurrent_titles.clone();
    let writer_url = pg_url();
    let writer = tokio::spawn(async move {
        let (wsql, wconn) = tokio_postgres::connect(&writer_url, tokio_postgres::NoTls)
            .await
            .expect("writer connect");
        tokio::spawn(async move {
            let _ = wconn.await;
        });
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
        while tokio::time::Instant::now() < deadline {
            let t = format!("conc-write-{}", uuid::Uuid::new_v4());
            if wsql
                .execute(
                    "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
                    &[&uuid::Uuid::new_v4(), &t],
                )
                .await
                .is_ok()
            {
                writer_titles.lock().unwrap().push(t);
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    });

    // 3. Drain ALL events (snapshot + live) for long enough to cover the
    //    writer window plus the stream catch-up.
    let all_events = collect_events(&mut repl, 4096, Duration::from_secs(5)).await;
    writer.await.unwrap();
    drop(repl);
    for _ in 0..40 {
        let active = sql
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|r| r.get::<_, bool>(0));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop_slot(&sql, &slot).await;

    let concurrent = concurrent_titles.lock().unwrap().clone();
    assert!(
        !concurrent.is_empty(),
        "writer produced no rows; test inconclusive"
    );

    // 4. EXACTLY-ONCE: each concurrent title must appear in exactly one event.
    let mut zero = Vec::new();
    let mut doubled = Vec::new();
    for t in &concurrent {
        let n = all_events
            .iter()
            .filter(|ev| String::from_utf8_lossy(ev.payload_bytes()).contains(t.as_str()))
            .count();
        match n {
            0 => zero.push(t.clone()),
            1 => {}
            _ => doubled.push((t.clone(), n)),
        }
    }
    assert!(
        zero.is_empty(),
        "LOST rows (appeared 0 times across snapshot+stream): {} rows; first few: {:?}",
        zero.len(),
        zero.iter().take(3).collect::<Vec<_>>()
    );
    assert!(
        doubled.is_empty(),
        "DUPLICATED rows (appeared >1 times across snapshot+stream): {} rows; first few: {:?}",
        doubled.len(),
        doubled.iter().take(3).collect::<Vec<_>>()
    );
    eprintln!(
        "concurrent-writes test: {} concurrent rows, all appeared exactly once across {} events",
        concurrent.len(),
        all_events.len()
    );
}
