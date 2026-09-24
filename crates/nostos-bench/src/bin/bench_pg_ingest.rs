//! `nostos-bench-pg-ingest` — the real-Postgres INGEST bench leg.
//!
//! Measures ONE stage honestly: real Postgres WAL (pgoutput) → `PgReplicator`
//! → `FanOutService` → N loopback WebSocket frame-counting sinks, under a
//! paced, persistent-connection, multi-row-INSERT generator against the live
//! `tasks` table.
//!
//! ## What this is NOT (honesty framing — echoed into every artifact)
//!
//! These numbers are NEVER comparable to the 833,307 ops/sec eval-only
//! fan-out headline in `benches/results/RESULTS.md` (FakeReplicator flood,
//! same loopback, different pipeline stage and workload shape). Same-stage,
//! same-units comparisons only — `docs/BENCHMARK-METHODOLOGY.md` §2/§8.
//! The sinks here also decode frame payloads for the lag gauge, so they are
//! heavier than the headline harness's bare counters.
//!
//! ## Topology
//!
//! Mirrors `src/main.rs` deliberately (same in-process axum server, same
//! `FanOutService` + `SessionManager` + `InMemorySessionStore`, same
//! frame-counting WS sinks via `wire::decode_frames`) — duplicated rather
//! than refactored out of `main.rs`, per the crate's standalone-bin
//! convention (`probe_10k.rs`, `reconnect_storm.rs`). The replicator is
//! swapped: `PgReplicator` against a real database instead of `FakeReplicator`.
//!
//! ## Slot lifecycle (deviation from e2e_pg_write_amp.rs, deliberate)
//!
//! The e2e lets `PgReplicator` create the slot fresh and waits for
//! `slot_epoch >= 1`. That fresh-create path ALSO captures the initial table
//! snapshot (`pg.rs::ensure_slot_and_publication`), which would stream every
//! pre-existing published row to the sinks and destroy the delivery-ratio
//! denominator. This bench instead PRE-CREATES the slot via
//! `pg_create_logical_replication_slot` on its control connection: the
//! replicator then takes the `SlotProbe::Healthy` path — no snapshot, streams
//! from the slot's consistent point — and `slot_epoch` stays 0 by design (it
//! only bumps on the missing/Lost recreate chute). Readiness is therefore
//! "slot exists AND `pg_replication_slots.active = true`" (the walsender has
//! attached). A `frames_before_load` contamination guard rides every artifact:
//! pre-load frames are SUBTRACTED from `frames_delivered` and a non-zero value is
//! flagged in the summary (the shared-DB note explains why). Uniqueness + cleanup follow the e2e: unique slot name per run,
//! logged; guaranteed drop + row delete on every exit path this binary
//! controls (a panic can still leak the slot — its name is logged so an
//! operator can `pg_drop_replication_slot` it).
//!
//! ## Generator
//!
//! One PERSISTENT tokio-postgres connection issuing multi-row INSERTs
//! (`generate_series` + per-row `clock_timestamp()`), paced to `--rate`
//! rows/sec (0 = unpaced). This is NOT the e2e's connect-per-row pattern
//! (~42 events/sec floor there); the observed rate is reported so the
//! generator is never the silent bottleneck.
//!
//! ## Lag gauge (coarse, disclosed)
//!
//! `tasks.created_at` (timestamptz) is written as `clock_timestamp()` per row
//! and rides the payload JSON through the wire (hex-encoded), so the sink can
//! parse it and compute write→recv lag. PG-in-docker and the host share a
//! clock but are not the same clock: a one-shot skew sample
//! (`SELECT clock_timestamp()` vs local, round-trip mid-pointed) is recorded
//! and added to every sample. Residual drift within a run is unmeasured —
//! treat lag as a coarse gauge, not a latency SLA.

// Benchmark/reporting code: same pedantic-lint pragmatism as `main.rs` and
// `probe_10k.rs` — throughput math on u64 counters and presentation-format
// building trip the cast/format pedantic lints where the flagged patterns
// are acceptable here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::routing::get;
use chrono::{DateTime, Utc};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use nostos_application::ports::Metrics;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::ColumnValue;
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::wire::{self, WireFrame};
use serde::Serialize;
use tokio::time::{sleep, timeout};
use tokio_postgres::Client;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::info;
use uuid::Uuid;

/// The comparability contract every artifact carries verbatim.
const FRAMING: &str = "STAGE MEASUREMENT: real-Postgres logical-replication ingest \
     (pgoutput) -> Nostos fan-out -> N loopback WebSocket frame-counting sinks, under a paced \
     batched-INSERT load. NOT comparable to the 833,307 ops/sec eval-only FakeReplicator \
     fan-out headline in benches/results/RESULTS.md (different replicator, different workload \
     shape, sinks here also decode payloads for the lag gauge). Same-stage, same-units \
     comparisons only, per docs/BENCHMARK-METHODOLOGY.md.";

/// The publication `docker/pg-init/01-sources.sql` creates (tasks + booking
/// tables). Preflight fails fast if it is missing — a missing publication must
/// NOT be silently re-created `FOR ALL TABLES` by the replicator here (that
/// variant would feed `cairn_oplog` back into replication).
const PUBLICATION: &str = "cairn_pub";

/// CLI configuration.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "nostos-bench-pg-ingest",
    version,
    about = "Nostos real-Postgres ingest bench leg (STAGE measurement — see FRAMING)"
)]
struct Config {
    /// Loopback WS sinks counting frames (like nostos-bench).
    #[arg(long, env = "BENCH_PG_CLIENTS", default_value_t = 8)]
    clients: usize,

    /// Total rows to INSERT into tasks (the load volume).
    #[arg(long, env = "BENCH_PG_EVENTS", default_value_t = 2_000)]
    events: u64,

    /// Target write rate in rows/sec (0 = unpaced, as fast as the connection
    /// allows). The observed rate is always reported.
    #[arg(long, env = "BENCH_PG_RATE", default_value_t = 500)]
    rate: u64,

    /// Rows per multi-row INSERT statement (generate_series batch).
    #[arg(long, env = "BENCH_PG_BATCH", default_value_t = 250)]
    batch: u64,

    /// Per-session buffer depth (mirrors nostos-bench --buffer).
    #[arg(long, env = "BENCH_PG_BUFFER", default_value_t = 1_024)]
    buffer: usize,

    /// Output directory for the JSON fragment artifact (NEVER RESULTS.md —
    /// that file belongs to the stock bench / report.rs).
    #[arg(long, env = "BENCH_PG_OUT", default_value = "benches/results/pg")]
    out_dir: String,

    /// Post-load drain timeout (seconds) before the run gives up waiting for
    /// the last frames.
    #[arg(long, env = "BENCH_PG_TIMEOUT", default_value_t = 60)]
    timeout_secs: u64,

    /// Label recorded into the artifact (e.g. SMOKE vs production-quiet-window)
    /// so a reader can tell a seconds-long smoke run from an orchestrated one.
    #[arg(long, env = "BENCH_PG_LABEL", default_value = "run")]
    label: String,

    /// Postgres URL (same convention as the e2e suite / NOSTOS_PG_URL).
    #[arg(
        long,
        env = "NOSTOS_PG_URL",
        default_value = "postgres://cairn:cairn@localhost:5433/cairn"
    )]
    pg_url: String,
}

/// Recorded environment (report.rs convention: rustc, hostname, cores — plus
/// date / uname / load average / chip, and the PG server version).
#[derive(Debug, Serialize)]
struct EnvCapture {
    date_utc: String,
    hostname: String,
    uname: String,
    chip: String,
    load_average: String,
    cpu_cores: usize,
    rustc: String,
    pg_version: String,
    build_profile: String,
}

/// Coarse write_ts→recv lag summary (milliseconds, skew-corrected).
#[derive(Debug, Serialize)]
struct LagSummary {
    samples: u64,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    /// One-shot PG-vs-host clock skew added into every sample (ms).
    clock_skew_correction_ms: f64,
}

/// The measured run (JSON artifact body).
// Evidence flags (slot lifecycle) read as flat JSON keys — a report DTO,
// not a state machine; the bools ARE the artifact.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
struct RunRecord {
    label: String,
    framing: String,
    environment: EnvCapture,
    // config echo
    clients: usize,
    events_target: u64,
    rate_target_rows_per_sec: u64,
    batch_rows: u64,
    buffer: usize,
    pg_url_redacted: String,
    // slot lifecycle evidence
    slot_name: String,
    slot_pre_created: bool,
    initial_snapshot_skipped: bool,
    slot_dropped: bool,
    rows_deleted_after_run: u64,
    // load phase
    rows_written: u64,
    rows_per_sec_observed: f64,
    insert_wall_secs: f64,
    // delivery
    frames_before_load: u64,
    frames_expected: u64,
    frames_delivered: u64,
    delivery_ratio: f64,
    drop_rate: f64,
    fanout_frames_per_sec: f64,
    drain_secs: f64,
    total_secs: f64,
    drain_timed_out: bool,
    // lag
    lag: Option<LagSummary>,
    // shared-DB honesty
    shared_db_note: String,
}

/// Load-phase + drain result handed back to the artifact builder.
struct Measured {
    rows_written: u64,
    insert_wall_secs: f64,
    frames_before_load: u64,
    frames_delivered: u64,
    drain_secs: f64,
    total_secs: f64,
    timed_out: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = nostos_infra::env::parse::<Config>();
    info!(?cfg, "starting nostos-bench-pg-ingest");

    // Modest client counts by default, but keep FD headroom parity with the
    // main harness in case an orchestrated run raises --clients.
    raise_fd_limit();

    // ---- preflight (fail fast, before anything is created) ----
    let sql = connect_pg(&cfg).await?;
    preflight(&sql).await?;

    let org = Uuid::new_v4();
    let slot = format!(
        "bench_pgi_{}_{}",
        std::process::id(),
        &Uuid::new_v4().simple().to_string()[..8]
    );
    // Idempotent leftover drop (same-name collision is ~impossible with the
    // uuid suffix; this mirrors the e2e's defensive pre-drop).
    let _ = sql
        .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
        .await;

    // ---- clock-skew sample (before wiring: sinks need it at spawn) ----
    let (skew, skew_ms) = measure_skew(&sql).await?;
    info!(skew_ms, "PG-vs-host clock skew sample (added into lag)");

    // ---- topology: same wiring as main.rs (duplicated deliberately) ----
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
    .with_buffer(cfg.buffer);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server");
    });
    let url = format!("ws://{addr}/sync");

    // ---- sinks (sharded per-client counters, like main.rs) ----
    let per_client: Vec<Arc<AtomicU64>> = (0..cfg.clients)
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    let lags: Vec<Arc<Mutex<Vec<f64>>>> = (0..cfg.clients)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut sink_handles = Vec::with_capacity(cfg.clients);
    for (i, received) in per_client.iter().enumerate() {
        let h = tokio::spawn(sink_task(
            url.clone(),
            Arc::clone(received),
            Arc::clone(&lags[i]),
            skew,
        ));
        sink_handles.push(h);
    }
    // connect + subscribe grace (same 500ms the main harness gives).
    sleep(Duration::from_millis(500)).await;

    let sum_received = || {
        per_client
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .sum::<u64>()
    };

    // ---- slot + replicator ----
    // PRE-CREATE the slot so the replicator takes the Healthy path (no
    // initial snapshot — see module docs). Streaming starts at the slot's
    // consistent point; every insert happens after `active = true`.
    let consistent: String = sql
        .query_one(
            "SELECT lsn::text FROM pg_create_logical_replication_slot($1, 'pgoutput')",
            &[&slot],
        )
        .await
        .with_context(|| {
            format!(
                "failed to create replication slot '{slot}' \
                 (check max_replication_slots / privileges on {})",
                cfg.pg_url
            )
        })?
        .get(0);
    info!(slot = %slot, consistent_point = %consistent, "slot pre-created (Healthy path: no initial snapshot)");

    let metrics = Arc::new(Metrics::new());
    // Error paths between pre-create and teardown MUST drop the slot
    // explicitly: the teardown below only runs once the driver is spawned,
    // and a WAL-retaining slot otherwise leaks (module doc's "guaranteed
    // drop" was a lie on these two paths until 2026-08-17).
    let pg_cfg = match PgReplicatorConfig::from_url(&cfg.pg_url, &slot, PUBLICATION) {
        Ok(c) => c,
        Err(e) => {
            drop_slot(&sql, &slot).await;
            return Err(anyhow::anyhow!("invalid PG url {}: {e}", cfg.pg_url));
        }
    };
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        // Week-1 extractor (same as main.rs): match on table only — worst-case
        // all-clients fan-out. The payload's real columns exist but this leg
        // deliberately mirrors the main harness's predicate shape.
        let extract = |_e: &nostos_domain::ReplicationEvent, _col: &str| -> Option<ColumnValue> {
            Some(ColumnValue::Any)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    // Readiness: the walsender has attached (epoch stays 0 on the Healthy
    // path — see module docs for why this replaces the e2e's epoch wait).
    if let Err(e) = wait_slot_active(&sql, &slot, Duration::from_secs(15)).await {
        driver.abort();
        drop_slot(&sql, &slot).await;
        return Err(e);
    }

    // ---- measurement ----
    let measurement = measure(&cfg, &sql, sum_received, org).await;

    // ---- teardown + guaranteed cleanup (runs on Ok AND Err measurement) ----
    driver.abort();
    for h in &sink_handles {
        h.abort();
    }
    drop(server_handle);
    let slot_dropped = drop_slot(&sql, &slot).await;
    let rows_deleted = sql
        .execute("DELETE FROM tasks WHERE org_id = $1", &[&org])
        .await
        .unwrap_or(0);

    let m = measurement?;

    // ---- env + artifact ----
    let pg_version: String = sql
        .query_one("SELECT version()", &[])
        .await
        .map_or_else(|_| "unknown".to_string(), |r| r.get(0));
    let env = capture_env(pg_version);

    let mut all_lags = Vec::new();
    for l in &lags {
        if let Ok(g) = l.lock() {
            all_lags.extend_from_slice(&g);
        }
    }
    let lag = summarize_lags(&all_lags, skew_ms);

    let frames_expected = m.rows_written.saturating_mul(cfg.clients as u64);
    let delivery_ratio = if frames_expected == 0 {
        0.0
    } else {
        m.frames_delivered as f64 / frames_expected as f64
    };
    let record = RunRecord {
        label: cfg.label.clone(),
        framing: FRAMING.to_string(),
        environment: env,
        clients: cfg.clients,
        events_target: cfg.events,
        rate_target_rows_per_sec: cfg.rate,
        batch_rows: cfg.batch,
        buffer: cfg.buffer,
        pg_url_redacted: redact_url(&cfg.pg_url),
        slot_name: slot.clone(),
        slot_pre_created: true,
        initial_snapshot_skipped: true,
        slot_dropped,
        rows_deleted_after_run: rows_deleted,
        rows_written: m.rows_written,
        rows_per_sec_observed: m.rows_written as f64 / m.insert_wall_secs.max(1e-9),
        insert_wall_secs: m.insert_wall_secs,
        frames_before_load: m.frames_before_load,
        frames_expected,
        frames_delivered: m.frames_delivered,
        delivery_ratio,
        drop_rate: (1.0 - delivery_ratio).clamp(0.0, 1.0),
        fanout_frames_per_sec: m.frames_delivered as f64 / m.total_secs.max(1e-9),
        drain_secs: m.drain_secs,
        total_secs: m.total_secs,
        drain_timed_out: m.timed_out,
        lag,
        shared_db_note: "the Postgres instance is shared (docker nostos-postgres); \
             concurrent writers (e2e tests, other agents) can contaminate counts. \
             frames_before_load is the pre-load contamination guard and must be 0; \
             a delivery_ratio > 1.0 would indicate mid-run contamination."
            .to_string(),
    };

    write_artifact(&cfg, &record)?;
    print_summary(&record);
    if !record.slot_dropped {
        bail!(
            "slot '{slot_name}' was NOT dropped — PG may retain WAL against it. \
             Drop manually: SELECT pg_drop_replication_slot('{slot_name}');",
            slot_name = record.slot_name
        );
    }
    Ok(())
}

/// Load phase + drain wait. Kept as one fn so the load start instant is a
/// single clock reading shared by insert_wall / total.
async fn measure<F>(cfg: &Config, sql: &Client, sum_received: F, org: Uuid) -> Result<Measured>
where
    F: Fn() -> u64,
{
    // Contamination guard: whatever arrived before load start is NOT this
    // run's payload (pre-created slot ⇒ expected 0).
    let frames_before_load = sum_received();

    // ---- load: persistent connection, paced multi-row INSERTs ----
    let start = Instant::now();
    let mut written: u64 = 0;
    let mut next_at = Instant::now();
    while written < cfg.events {
        let n = cfg.batch.min(cfg.events - written);
        let n_i = i32::try_from(n).context("batch size exceeds i32")?;
        let inserted = sql
            .execute(
                // Multi-row INSERT via generate_series; clock_timestamp() is
                // VOLATILE so it is stamped per output row (the lag gauge's
                // write_ts). One statement = one txn = one WAL burst.
                "INSERT INTO tasks (org_id, title, created_at) \
                 SELECT $1, 'pgi' || g, clock_timestamp() \
                 FROM generate_series(1, $2) AS g",
                &[&org, &n_i],
            )
            .await
            .with_context(|| format!("batched INSERT of {n} rows failed"))?;
        written += inserted;

        // Skip the pace-sleep after the FINAL batch: sleeping there only
        // inflates insert_wall_secs (and understates rows_per_sec_observed)
        // by one batch interval — the drain phase starts after this loop.
        if cfg.rate > 0 && written < cfg.events {
            next_at += Duration::from_secs_f64(n as f64 / cfg.rate as f64);
            let now = Instant::now();
            if next_at > now {
                sleep(next_at - now).await;
            } else {
                // Fell behind the target cadence (insert slower than rate):
                // resync the schedule instead of firing a catch-up burst.
                next_at = now;
            }
        }
    }
    let insert_wall_secs = start.elapsed().as_secs_f64();
    let load_end = Instant::now();
    info!(written, insert_wall_secs, "load phase complete");

    // ---- drain: wait for every written row × every sink ----
    let target = written.saturating_mul(cfg.clients as u64);
    let wait = async {
        loop {
            if sum_received() >= target {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    };
    let timed_out = timeout(Duration::from_secs(cfg.timeout_secs), wait)
        .await
        .is_err();
    let drain_secs = load_end.elapsed().as_secs_f64();
    let total_secs = start.elapsed().as_secs_f64();

    Ok(Measured {
        rows_written: written,
        insert_wall_secs,
        frames_before_load,
        // Subtract pre-load contamination so it cannot inflate the
        // delivery ratio (pre-created slot ⇒ expected 0; a non-zero value
        // is flagged in the summary print).
        frames_delivered: sum_received().saturating_sub(frames_before_load),
        drain_secs,
        total_secs,
        timed_out,
    })
}

/// One-shot PG-vs-host clock skew: `SELECT clock_timestamp()` round-trip,
/// mid-pointed against the local readings. Returns the chrono Duration the
/// sinks add into each lag sample, plus its ms value for the artifact.
async fn measure_skew(sql: &Client) -> Result<(chrono::Duration, f64)> {
    let t0 = Utc::now();
    let pg_now: DateTime<Utc> = sql
        .query_one("SELECT clock_timestamp()", &[])
        .await
        .context("clock skew probe failed")?
        .get(0);
    let t1 = Utc::now();
    let local_mid = t0 + (t1 - t0) / 2;
    let skew = pg_now - local_mid;
    let ms = skew.num_microseconds().unwrap_or(0) as f64 / 1000.0;
    Ok((skew, ms))
}

/// One frame-counting sink — main.rs's `client_task` with the lag gauge
/// added: each frame's payload is hex-decoded, JSON-parsed, and its
/// `created_at` compared against recv time (skew-corrected).
async fn sink_task(
    url: String,
    received: Arc<AtomicU64>,
    lags: Arc<Mutex<Vec<f64>>>,
    skew: chrono::Duration,
) {
    // Retry connect briefly — the server is starting concurrently.
    let mut ws = None;
    for _ in 0..50 {
        match connect_async(&url).await {
            Ok((stream, _)) => {
                ws = Some(stream);
                break;
            }
            Err(_) => sleep(Duration::from_millis(20)).await,
        }
    }
    let Some(ws) = ws else { return };
    let (mut write, mut read) = ws.split();

    // Typed subscribe frame (wire.rs `ClientMessage` is `#[serde(tag=..)]`).
    let sub = serde_json::json!({ "type": "subscribe", "table": "tasks" }).to_string();
    if write.send(Message::Text(sub)).await.is_err() {
        return;
    }

    // Read loop — count FRAMES (not messages): batched writes coalesce N
    // frames into one WS message under backlog (same rationale as main.rs).
    while let Some(Ok(msg)) = read.next().await {
        let bytes: Vec<u8> = match msg {
            Message::Binary(b) => b,
            Message::Text(s) => s.into_bytes(),
            _ => continue,
        };
        for frame in wire::decode_frames(&bytes) {
            received.fetch_add(1, Ordering::Relaxed);
            if let Some(lag_ms) = frame_lag_ms(&frame, skew) {
                if let Ok(mut v) = lags.lock() {
                    v.push(lag_ms);
                }
            }
        }
    }
    let _ = write.close().await;
}

/// Compute one frame's write→recv lag (ms, skew-corrected), or `None` when
/// the payload carries no parseable `created_at` (delete frames, malformed
/// hex/JSON — the frame still counts toward delivery, only lag skips it).
fn frame_lag_ms(frame: &WireFrame, skew: chrono::Duration) -> Option<f64> {
    let payload = frame.payload.as_deref()?;
    let bytes = hex_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let created = v.get("created_at")?.as_str()?;
    let created = DateTime::parse_from_rfc3339(created)
        .ok()?
        .with_timezone(&Utc);
    let lag = Utc::now().signed_duration_since(created) + skew;
    Some(lag.num_microseconds().unwrap_or(0) as f64 / 1000.0)
}

/// Hex decode (wire payloads are hex-encoded JSON — mirrors wire.rs's tiny
/// private helper, which is not exported).
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Lag percentiles over all sinks' samples.
fn summarize_lags(v: &[f64], skew_ms: f64) -> Option<LagSummary> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f64| -> f64 {
        let idx = ((s.len() as f64 - 1.0) * q).round() as usize;
        s[idx.min(s.len() - 1)]
    };
    Some(LagSummary {
        samples: s.len() as u64,
        min_ms: s[0],
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
        max_ms: s[s.len() - 1],
        clock_skew_correction_ms: skew_ms,
    })
}

async fn connect_pg(cfg: &Config) -> Result<Client> {
    let (client, conn) = tokio_postgres::connect(&cfg.pg_url, tokio_postgres::NoTls)
        .await
        .with_context(|| {
            format!(
                "Postgres unreachable at {}. Start it: \
                 docker compose -f docker/docker-compose.yml up -d \
                 (container nostos-postgres, host port 5433)",
                cfg.pg_url
            )
        })?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

/// Fail-fast preflight: everything the run needs from PG before it creates
/// anything. Operator-grade messages, no cleanup needed on these failures.
async fn preflight(sql: &Client) -> Result<()> {
    let wal: String = sql
        .query_one("SHOW wal_level", &[])
        .await
        .context("SHOW wal_level failed")?
        .get(0);
    if wal != "logical" {
        bail!(
            "wal_level is '{wal}', must be 'logical' — the docker compose PG is \
             preconfigured; if this points elsewhere, fix the server config"
        );
    }
    let has_pub: bool = sql
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_publication WHERE pubname = $1)",
            &[&PUBLICATION],
        )
        .await
        .context("publication lookup failed")?
        .get(0);
    if !has_pub {
        bail!(
            "publication '{PUBLICATION}' is missing — (re)apply \
             docker/pg-init/01-sources.sql against the target database"
        );
    }
    let tasks: Option<String> = sql
        .query_one("SELECT to_regclass('public.tasks')::text", &[])
        .await
        .context("to_regclass probe failed")?
        .get(0);
    if tasks.is_none() {
        bail!("table public.tasks is missing — (re)apply docker/pg-init/01-sources.sql");
    }
    Ok(())
}

/// Wait until the replication slot is held by an active walsender — the
/// deterministic readiness signal on the pre-created-slot (Healthy) path.
async fn wait_slot_active(sql: &Client, slot: &str, within: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < within {
        let row = sql
            .query_opt(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await;
        if let Ok(Some(r)) = row {
            if r.get::<_, bool>(0) {
                info!(
                    slot = %slot,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "slot active (walsender attached)"
                );
                return Ok(());
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    bail!(
        "replication slot '{slot}' never became active within {}s — the \
         PgReplicator driver did not attach its walsender. Re-run with \
         RUST_LOG=nostos_infra=debug,bench_pg_ingest=info for the replicator logs",
        within.as_secs()
    );
}

/// Drop the slot, retrying briefly (the walsender may take a moment to
/// release it after the driver is aborted). Returns whether the slot is
/// verifiably GONE — the authoritative check, not the drop call's success.
async fn drop_slot(sql: &Client, slot: &str) -> bool {
    let exists = || async {
        sql.query_opt(
            "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        // Probe failure is treated as "still present" so the loop keeps
        // retrying and the final verdict stays conservative.
        .map_or(true, |o| o.is_some())
    };
    for _ in 0..10 {
        if !exists().await {
            return true;
        }
        let _ = sql
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await;
        sleep(Duration::from_millis(100)).await;
    }
    !exists().await
}

fn write_artifact(cfg: &Config, record: &RunRecord) -> Result<()> {
    fs::create_dir_all(&cfg.out_dir).context("create out dir")?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let path =
        std::path::Path::new(&cfg.out_dir).join(format!("pg-ingest-{}-{}.json", stamp, cfg.label));
    let json = serde_json::to_string_pretty(record).context("serialize artifact")?;
    fs::write(&path, json).with_context(|| format!("write artifact {}", path.display()))?;
    info!(path = %path.display(), "artifact written");
    Ok(())
}

fn print_summary(r: &RunRecord) {
    println!("\n=== nostos-bench-pg-ingest (label: {}) ===\n", r.label);
    println!("slot              : {}", r.slot_name);
    println!(
        "rows written      : {} (target {}/s, observed {:.1}/s)",
        r.rows_written, r.rate_target_rows_per_sec, r.rows_per_sec_observed
    );
    println!("sinks             : {}", r.clients);
    println!("frames expected   : {}", r.frames_expected);
    println!("frames delivered  : {}", r.frames_delivered);
    println!("delivery ratio    : {:.3}%", r.delivery_ratio * 100.0);
    println!("drop rate         : {:.3}%", r.drop_rate * 100.0);
    println!("insert wall       : {:.2}s", r.insert_wall_secs);
    println!(
        "drain             : {:.2}s{}",
        r.drain_secs,
        if r.drain_timed_out {
            " (TIMED OUT — numbers are partial)"
        } else {
            ""
        }
    );
    println!("total             : {:.2}s", r.total_secs);
    println!(
        "fan-out           : {:.0} frames/sec [STAGE — see framing]",
        r.fanout_frames_per_sec
    );
    match &r.lag {
        Some(l) => println!(
            "lag write→recv    : p50 {:.1}ms p95 {:.1}ms p99 {:.1}ms max {:.1}ms ({} samples, skew {:+.1}ms)",
            l.p50_ms, l.p95_ms, l.p99_ms, l.max_ms, l.samples, l.clock_skew_correction_ms
        ),
        None => println!("lag write→recv    : SKIPPED (no parseable created_at in any frame)"),
    }
    if r.frames_before_load == 0 {
        println!("frames before load: 0 (contamination guard — clean)");
    } else {
        println!(
            "frames before load: {} — WARNING: pre-load contamination detected; \
             excluded from frames_delivered, verify no concurrent writer ran",
            r.frames_before_load
        );
    }
    println!("slot dropped      : {}", r.slot_dropped);
    println!("rows deleted      : {}", r.rows_deleted_after_run);
    println!("\nFRAMING: {FRAMING}");
}

// ---- env capture (report.rs convention + date/uname/loadavg/chip) ----

fn capture_env(pg_version: String) -> EnvCapture {
    EnvCapture {
        date_utc: Utc::now().to_rfc3339(),
        hostname: run_capture("hostname", &[]),
        uname: run_capture("uname", &["-a"]),
        chip: chip(),
        load_average: load_average(),
        cpu_cores: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        rustc: run_capture("rustc", &["--version"]),
        pg_version,
        build_profile: "--release (lto=fat, codegen-units=1)".to_string(),
    }
}

/// Shell out once, returning trimmed stdout or "unknown" — the report.rs
/// sentinel convention for uncapturable values.
fn run_capture(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn chip() -> String {
    #[cfg(target_os = "macos")]
    {
        run_capture("sysctl", &["-n", "machdep.cpu.brand_string"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split(':').nth(1).map(|v| v.trim().to_string()))
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg").ok().map_or_else(
        || run_capture("sysctl", &["-n", "vm.loadavg"]),
        |s| s.trim().to_string(),
    )
}

/// Strip the password from a URL for the artifact (secrets do not belong in
/// results files).
fn redact_url(url: &str) -> String {
    match (url.split_once("//"), url.split_once('@')) {
        (Some((scheme, _)), Some((_, host))) => format!("{scheme}//***@{host}"),
        _ => "redaction-failed".to_string(),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,bench_pg_ingest=info")),
        )
        .try_init();
}

// Raise the soft FD limit for large --clients orchestrated runs (parity with
// main.rs; macOS defaults are far below what 10k sockets need).
fn raise_fd_limit() {
    #[cfg(unix)]
    {
        let _ = nix::sys::resource::setrlimit(
            nix::sys::resource::Resource::RLIMIT_NOFILE,
            65_536,
            65_536,
        );
    }
}
