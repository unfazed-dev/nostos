//! # nostos-bench — Week-1 throughput benchmark harness.
//!
//! Measures the headline moat: how fast can Nostos's server fan replication
//! events out to thousands of concurrent WebSocket clients, compared to
//! PowerSync's published 2–4k ops/sec Node.js ceiling?
//!
//! ## Design
//!
//! One process runs:
//! 1. An in-process `nostos-server` axum app on `127.0.0.1:<ephemeral>`, sharing
//!    its `SessionStore` with the bench driver.
//! 2. N WebSocket client tasks (tokio-tungstenite), each subscribing to `tasks`.
//! 3. A `FakeReplicator` driving the **real** `FanOutService` against the shared
//!    store — so events traverse the production pipeline (predicate index →
//!    bounded sink → WebSocket frame write) end to end.
//! 4. Measurement: wall-clock for M total events to be received, ops/sec,
//!    drop rate, p99 client latency.
//!
//! See `docs/BENCHMARK-METHODOLOGY.md` for the full contract.

// Benchmark/reporting code: the `cast_*` and `format_*` pedantic lints fire on
// routine throughput math and report-string building where the flagged patterns
// are acceptable (values within f64 precision; `push_str(&format!(...))` reads
// fine in presentation code). Allow them here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::uninlined_format_args,
    clippy::manual_is_multiple_of
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::routing::get;
use nostos_application::{FanOutOutcome, FanOutService, SessionManager};
use nostos_domain::ColumnValue;
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::info;

mod report;
mod stats;

use report::write_reports;
use stats::Histogram;

/// CLI configuration.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "nostos-bench",
    version,
    about = "Nostos throughput benchmark — the Week-1 moat"
)]
pub struct BenchConfig {
    /// Comma-separated client counts to test (e.g. 1000,5000,10000).
    #[arg(
        long,
        env = "BENCH_CLIENTS",
        default_value = "1000,5000,10000",
        value_delimiter = ','
    )]
    pub clients: Vec<usize>,

    /// Total events to generate per run.
    #[arg(long, env = "BENCH_EVENTS", default_value_t = 100_000)]
    pub events: u64,

    /// Payload profile: "small" (~100B) or "large" (~4KB).
    #[arg(long, env = "BENCH_PROFILE", default_value = "small")]
    pub profile: String,

    /// Per-session buffer depth.
    /// Recycle primary keys over this many distinct rows. `0` (the default,
    /// and what every historical result in RESULTS.md was measured with)
    /// gives every event its own new row, so nothing can ever conflate.
    ///
    /// Non-zero is how ADR-0045's overflow conflation is measured: a real
    /// app re-updates the same rows, a monotonic key stream never does, and
    /// the difference is the entire effect being tested.
    #[arg(long, env = "BENCH_DISTINCT_KEYS", default_value_t = 0)]
    pub distinct_keys: u64,

    #[arg(long, env = "BENCH_BUFFER", default_value_t = 1024)]
    pub buffer: usize,

    /// Output directory for results.
    #[arg(long, env = "BENCH_OUT", default_value = "benches/results")]
    pub out_dir: String,

    /// Per-run wall-clock timeout (seconds).
    #[arg(long, env = "BENCH_TIMEOUT", default_value_t = 120)]
    pub timeout_secs: u64,

    /// Offered arrival rate, events/sec, held OPEN-LOOP: event `i` is due at
    /// `start + i/rate` no matter how long the router took for `i-1`.
    ///
    /// The rate is events entering the ROUTER, not frames leaving it: one
    /// event fans out to every subscribed client, so the delivery rate this
    /// asks for is `rate * clients` and the ops/sec column stays directly
    /// comparable to an unpaced run.
    ///
    /// `0` (the default, and what every figure in RESULTS.md was measured
    /// with) floods — which answers "where does it fall over", not "does it
    /// meet rate R at under 1% loss". The second question is the one a user
    /// with a workload actually has, and it needs a rate. Ladder the rate up
    /// until the drop bar breaks, and the largest rate that held is the
    /// answer. See `docs/plans/measuring-conflation-honestly.md`, defect 1.
    #[arg(long, env = "BENCH_RATE", default_value_t = 0)]
    pub rate: u64,

    /// Measured repetitions per client tier. Fastest and slowest are dropped
    /// and the rest averaged (MLPerf); the min-max spread is reported beside
    /// the mean, because a tier whose reps disagree has not produced a figure.
    #[arg(long, env = "BENCH_REPS", default_value_t = 5)]
    pub reps: usize,

    /// Warm-up repetitions per tier, run first and DISCARDED. The first run of
    /// a tier pays for cold caches, lazy page faults and an unsettled
    /// allocator; including it makes every later comparison a comparison of
    /// warm-up cost. `0` disables.
    #[arg(long, env = "BENCH_WARMUP", default_value_t = 1)]
    pub warmup_reps: usize,

    /// Seed for the randomised run order. Fixed by default so a run is
    /// reproducible; change it to confirm a result is not an artefact of one
    /// particular interleaving.
    #[arg(long, env = "BENCH_ORDER_SEED", default_value_t = 0x000C_A110_5EED)]
    pub order_seed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub clients: usize,
    /// Which measured repetition this is (0-based). Warm-ups never get here.
    pub rep: usize,
    /// Position in the run schedule (0-based), i.e. the order this run
    /// ACTUALLY executed in, which the shuffle makes different from the order
    /// it is tabled in. It is the x axis of `series.svg`: throttling, a
    /// background process waking up, or a machine going quiet halfway through
    /// a session are all visible in execution order and in no other.
    pub order: usize,
    pub events_total: u64,
    pub events_delivered: u64,
    /// Events the router accepted by REPLACING a still-waiting frame for the
    /// same row (ADR-0045). Convergent, not lost — so it is subtracted from
    /// the drop rate alongside `events_delivered`, and reported on its own so
    /// the conflation benefit is visible rather than inferred.
    pub events_superseded: u64,
    pub ops_per_sec: f64,
    pub drop_rate: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub elapsed_secs: f64,
    /// `false` when the run hit `--timeout-secs` instead of delivering every
    /// event. The window expired, so `ops_per_sec` is a FLOOR (what got out
    /// before we gave up) and `drop_rate` counts events still in flight rather
    /// than events lost. Neither is a measurement, so both are withheld from
    /// the printed table and from every aggregate in RESULTS.md.
    ///
    /// Latency is unaffected: p50/p99 describe the frames that DID land, and a
    /// truncated window does not bias them. They stay reported.
    ///
    /// Caught 2026-09-22 on a 4-core i7-7700HQ, where the 120s default expired
    /// at the *1k* tier and the JSON still read like a result. On faster
    /// hardware the same default never binds, which is exactly why this has to
    /// be a stamp in the output and not a note in a doc.
    pub throughput_valid: bool,
    /// The `--rate` this run offered, events/sec. `0` = unpaced flood.
    pub target_rate: u64,
    /// `false` when a paced run's generator fell behind its own schedule, i.e.
    /// the load OFFERED was below the load requested. The drop rate then
    /// describes the generator, not the system, so it is withheld exactly like
    /// a timed-out run's. Always `true` for an unpaced run — a flood has no
    /// schedule to miss.
    pub rate_held: bool,
    pub profile: String,
}

/// Deterministic Fisher-Yates using the same xorshift64 the `FakeReplicator`
/// seeds with. `rand` would be a new dependency for eight lines.
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut x = seed | 1; // xorshift64 requires a nonzero state
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = BenchConfig::parse();
    info!(?cfg, "starting nostos-bench");

    // Raise file-descriptor limit — 10k clients need ~20k+ FDs (sockets + pipes).
    raise_fd_limit();

    // Warm-ups first, in tier order, and thrown away. They exist to pay the
    // cold-start costs once per tier so the measured reps don't carry them.
    for rep in 0..cfg.warmup_reps {
        for &clients in &cfg.clients {
            println!(
                "warm-up {}/{} @ {clients} clients (discarded)",
                rep + 1,
                cfg.warmup_reps
            );
            run_one(&cfg, clients, rep)
                .await
                .context(format!("warm-up with {clients} clients failed"))?;
        }
    }

    // Defect 4 (docs/plans/measuring-conflation-honestly.md): a fixed
    // `1k,5k,10k` order hands the FIRST tier every cold cache and every
    // unsettled thermal state, run after run. That bias is systematic, not
    // noise — it never averages out, because it lands on the same tier every
    // time. Randomising the interleave spreads it across tiers instead, and is
    // reported to cut run-to-run variance by up to 40% (Google Benchmark).
    let mut schedule: Vec<(usize, usize)> = Vec::new();
    for rep in 0..cfg.reps {
        for &clients in &cfg.clients {
            schedule.push((clients, rep));
        }
    }
    shuffle(&mut schedule, cfg.order_seed);

    let mut results = Vec::with_capacity(schedule.len());
    for (i, &(clients, rep)) in schedule.iter().enumerate() {
        println!(
            "run {}/{}: {clients} clients, rep {}",
            i + 1,
            schedule.len(),
            rep + 1
        );
        let mut r = run_one(&cfg, clients, rep)
            .await
            .context(format!("run with {clients} clients failed"))?;
        r.order = i;
        results.push(r);
    }
    // Report in tier order regardless of the order they were run in.
    results.sort_by_key(|r| (r.clients, r.rep));

    let env = report::Environment::collect(&cfg);
    write_reports(&cfg, &results, &env).context("failed to write reports")?;

    let tiers = report::summarize(&results);
    println!("\n=== Nostos Week-1 Benchmark ===\n");
    if cfg.rate > 0 {
        println!("offered rate: {} events/sec, open-loop\n", cfg.rate);
    }
    println!(
        "{:>8} {:>14} {:>21} {:>8} {:>9} {:>9} {:>7}",
        "clients", "ops/sec", "spread", "drop%", "p50(ms)", "p99(ms)", "reps"
    );
    for t in &tiers {
        // A tier with no valid repetition still prints its row — the delivered
        // count and the latencies are real — but the two figures the invalid
        // runs corrupted are withheld rather than rendered as numbers someone
        // could quote.
        let reps = format!("{}/{}", t.reps_valid, t.reps_total);
        if t.reps_valid == 0 {
            println!(
                "{:>8} {:>14} {:>21} {:>8} {:>9.2} {:>9.2} {:>7}",
                t.clients,
                t.invalid_label(),
                "—",
                "—",
                t.p50_us / 1000.0,
                t.p99_us / 1000.0,
                reps
            );
            continue;
        }
        println!(
            "{:>8} {:>14.0} {:>21} {:>7.2}% {:>9.2} {:>9.2} {:>7}",
            t.clients,
            t.ops_per_sec,
            format!("{:.0}–{:.0}", t.ops_min, t.ops_max),
            t.drop_rate * 100.0,
            t.p50_us / 1000.0,
            t.p99_us / 1000.0,
            reps
        );
    }
    println!(
        "\nops/sec is the trimmed mean of {} reps (fastest and slowest dropped); \
         spread is their min-max.",
        cfg.reps
    );

    let timed_out = results.iter().filter(|r| !r.throughput_valid).count();
    if timed_out > 0 {
        println!(
            "\n!! {timed_out} run(s) hit --timeout-secs {} before delivering every event.",
            cfg.timeout_secs
        );
        println!(
            "   ops/sec and drop% are withheld: they would describe the clock, not the system."
        );
        println!("   Raise --timeout-secs and re-run to get a figure.");
    }
    let rate_missed = results.iter().filter(|r| !r.rate_held).count();
    if rate_missed > 0 {
        println!(
            "\n!! {rate_missed} run(s) could not hold --rate {}: the generator fell behind its",
            cfg.rate
        );
        println!("   own schedule, so the load offered was below the load requested.");
        println!("   Those runs measure the generator. Lower --rate, or generate off-box.");
    }
    println!("\nResults written to {}/", cfg.out_dir);
    Ok(())
}

async fn run_one(cfg: &BenchConfig, clients: usize, rep: usize) -> Result<RunResult> {
    info!(clients, rep, events = cfg.events, "run starting");

    // ---- shared store + use-cases (the same instances the server uses) ----
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    // ADR-0025 slice 2: optionally measure the fan-out-loop cost of the op-log
    // by attaching a RecordingOpLogWriter (its append = try_send + drop-newest,
    // mirroring PgOpLogWriter's hot path). Set NOSTOS_BENCH_OPLOG=1 for the
    // production-shaped run; leave unset for the fan-out-ceiling baseline. The
    // two together are the honest before/after the plan mandates (the real-PG
    // write-amplification cost is a separate real-PG measurement, slice 6).
    let op_log: Option<Arc<nostos_infra::RecordingOpLogWriter>> =
        if std::env::var_os("NOSTOS_BENCH_OPLOG").is_some() {
            Some(Arc::new(nostos_infra::RecordingOpLogWriter::new(4096)))
        } else {
            None
        };
    let op_log_handle = op_log.clone();
    // ADR-0037 plan 1.3: toggle the push doorbell enqueue so the bench can
    // measure its hot-path cost — the same before/after discipline as
    // NOSTOS_BENCH_OPLOG above. The counting consumer (a NoopNotifier plus an
    // AtomicU64) stands in for the coalescer (plan 2.4). Bench sessions are
    // anonymous (no account), so the expected hint count is 0 — this measures
    // the enqueue bookkeeping on the fan-out path, not rail traffic.
    let push_count: Option<Arc<AtomicU64>> =
        std::env::var_os("NOSTOS_BENCH_PUSH").map(|_| Arc::new(AtomicU64::new(0)));
    let fanout = Arc::new({
        let builder = match op_log {
            Some(w) => FanOutService::new(Arc::clone(&store)).with_op_log(w),
            None => FanOutService::new(Arc::clone(&store)),
        };
        match &push_count {
            Some(count) => builder.with_push_notifier(Arc::new(CountingNotifier {
                count: Arc::clone(count),
            })),
            None => builder,
        }
    });

    // ---- in-process axum server on an ephemeral port ----
    // The bench measures raw fan-out throughput; auth isAllowAnonymous (no
    // principal, no tenant filter) so the benchmark isn't gated on JWT minting.
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

    // ---- spawn N client tasks ----
    // Each client owns its OWN atomic counter (sharded) so 10k concurrent
    // incrementers don't serialize on a single cache line. We sum them at the
    // end. A shared counter at 10k clients is a fatal contention point.
    let per_client_received: Vec<Arc<AtomicU64>> =
        (0..clients).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut client_handles = Vec::with_capacity(clients);
    let mut histograms: Vec<Arc<std::sync::Mutex<Histogram>>> = Vec::with_capacity(clients);

    for received_arc in &per_client_received {
        let hist = Arc::new(std::sync::Mutex::new(Histogram::new()));
        histograms.push(Arc::clone(&hist));
        let received_c = Arc::clone(received_arc);
        let url_c = url.clone();
        let h = tokio::spawn(client_task(url_c, received_c, hist));
        client_handles.push(h);
    }

    // give clients a moment to connect + subscribe
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sum_received = || {
        per_client_received
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .sum::<u64>()
    };

    // ---- drive the FakeReplicator through the real FanOutService ----
    let repl_cfg = match cfg.profile.as_str() {
        "large" => FakeReplicatorConfig::large(cfg.events),
        _ => FakeReplicatorConfig::small(cfg.events),
    }
    .recycling_keys(cfg.distinct_keys)
    .paced(cfg.rate);
    let mut replicator = FakeReplicator::new(repl_cfg);
    // Read after the replicator has moved into the fan-out task: how far the
    // generator ever fell behind its own emission schedule.
    let lateness = replicator.lateness_handle();

    // Week-1 extractor: synthetic payload is opaque bytes; match on table only
    // (ColumnValue::Any matches every value). Real column extraction arrives
    // with the PgReplicator, which parses the tuple image.
    let extract = |_e: &nostos_domain::ReplicationEvent, _col: &str| -> Option<ColumnValue> {
        Some(ColumnValue::Any)
    };

    let start = Instant::now();
    // Drive fan-out concurrently with the clients receiving.
    let mut fanout_task = {
        let fanout = Arc::clone(&fanout);
        tokio::spawn(async move { fanout.run(&mut replicator, extract).await })
    };

    // Wait until delivery stops advancing, or timeout.
    //
    // `target` is what a LOSS-FREE run receives. The router is allowed to shed
    // (a full session channel — the `capacity_sheds` path), and a shed event
    // never reaches a client, so the moment anything is shed `target` becomes
    // unreachable and a plain `>= target` loop spins until the deadline. Every
    // figure derived from `elapsed` is then diluted by however much idle the
    // window had left.
    //
    // Measured 2026-09-22 on a 4-core i7-7700HQ: 99.4M events delivered, the
    // rest shed, then ~470s of spinning against a target that could not be
    // reached. Reported 165,712 ops/sec. The same run inside a 120s window
    // reported 824,882. Both were the ratio of real work to an arbitrary
    // window, and on a host that never sheds (Apple Silicon at 1k) the bug is
    // invisible because `target` is always met.
    //
    // So finish on QUIESCENCE: the run is over when delivery stops moving,
    // whether the remainder arrived or was shed. The clock stops at the last
    // delivery, so the quiet grace never enters `elapsed`.
    let quiet = Duration::from_secs(10);
    let target = cfg.events.saturating_mul(clients as u64);
    let deadline = Duration::from_secs(cfg.timeout_secs);
    let wait = async {
        let mut seen = 0_u64;
        let mut last_progress = Instant::now();
        loop {
            let now = sum_received();
            if now >= target {
                return Instant::now();
            }
            if now > seen {
                seen = now;
                last_progress = Instant::now();
            } else if seen > 0 && last_progress.elapsed() >= quiet {
                // `seen > 0` so a slow ramp-up is never mistaken for the end.
                return last_progress;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    // Quiescence is a COMPLETE run — the system stopped delivering because it
    // had nothing left to deliver. Only deadline expiry is invalid, because
    // there the run was still making progress when the window closed and we
    // cannot know what the total would have been.
    let (stopped_at, completed) = match timeout(deadline, wait).await {
        Ok(at) => (at, true),
        Err(_) => (Instant::now(), false),
    };
    let elapsed = stopped_at.duration_since(start);

    // The fan-out task emits events as fast as the router accepts them. At high
    // client counts (10k) with no client ACKs, `FanOutService::run` does a
    // per-event `slowest_session` + `min_acked_lsn` scan over every session —
    // O(N) per event — so finishing all `cfg.events` can take far longer than
    // the delivery window above. Awaiting it unconditionally would hang the
    // harness at 10k (the wait-loop times out, then we block on `run()`).
    //
    // We still await `run()` to completion so `outcome.matched` reflects the
    // REAL number of events the router attempted (the honest denominator for
    // the drop rate) — but cap it with a generous grace window so a genuinely
    // stuck run (10k) terminates instead of hanging forever. The grace is much
    // larger than any legitimate run needs: 1k finishes in ~10s, 5k in ~30s.
    let outcome = if let Ok(joined) = timeout(Duration::from_mins(3), &mut fanout_task).await {
        joined?
    } else {
        info!("fan-out didn't finish within 3min grace; aborting (10k regime)");
        fanout_task.abort();
        FanOutOutcome::default()
    };
    // Keep the in-process server alive for the remainder of this run. Binding
    // (rather than `_`) would trip `let_underscore_future`; we intentionally
    // drop it after the clients finish below.
    drop(server_handle);
    // signal clients to stop
    for h in &client_handles {
        h.abort();
    }

    let delivered = sum_received();
    // Aggregate the per-client latency histograms.
    let mut combined = Histogram::new();
    for h in &histograms {
        let g = h.lock().unwrap();
        combined.merge(&g);
    }

    let ops_per_sec = (delivered as f64) / elapsed.as_secs_f64().max(1e-9);
    let attempted = cfg.events.saturating_mul(clients as u64).max(1);
    // ADR-0045: `delivered` is a CLIENT-side frame count, and a superseded
    // event is deliberately one fewer frame for the same converged state.
    // Left out of the numerator, `drop%` would score conflation as exactly the
    // loss conflation exists to prevent. The router's own count is the only
    // honest source for it — the client cannot see a frame that was never sent.
    let accounted = delivered.saturating_add(outcome.superseded);
    let drop_rate = 1.0 - (accounted as f64 / attempted as f64);
    let (p50, p99) = (combined.percentile(0.5), combined.percentile(0.99));

    info!(
        clients,
        delivered,
        superseded = outcome.superseded,
        matched = outcome.matched,
        ops_per_sec,
        drop_rate,
        "run complete"
    );

    // ADR-0025 slice 2: when the op-log writer is attached, surface its drop
    // count. Must stay 0 — a non-zero value means the writer's bounded buffer
    // couldn't keep up with the FakeReplicator flood (the fan-out loop's
    // try_send cost would be too high, or the drain too slow).
    if let Some(h) = &op_log_handle {
        info!(
            clients,
            oplog_dropped = h.dropped(),
            "op-log recording writer drop count (must be 0)"
        );
    }

    // ADR-0037 plan 1.3: hints consumed by the counting rail. 0 is the
    // expected value — bench sessions are anonymous (no account to doorbell),
    // so a non-zero count here means account derivation is misfiring.
    if let Some(count) = &push_count {
        info!(
            clients,
            push_hints = count.load(Ordering::Relaxed),
            "push doorbell hints consumed (0 expected: bench sessions are anonymous)"
        );
    }

    // A paced run only tests the rate it managed to OFFER. A scheduler hiccup
    // is not a result, so allow slip up to 5% of the run's nominal duration
    // (events / rate), floored at 50 ms so a sub-second run isn't failed by
    // timer granularity. Past that the arrival process was not the one
    // requested, and every figure derived from it describes the generator.
    let max_lateness = Duration::from_nanos(lateness.load(Ordering::Relaxed));
    let rate_held = cfg.rate == 0 || {
        let nominal = cfg.events as f64 / cfg.rate as f64;
        max_lateness.as_secs_f64() <= (nominal * 0.05).max(0.05)
    };
    if !rate_held {
        info!(
            clients,
            rate = cfg.rate,
            slip_ms = max_lateness.as_millis() as u64,
            "generator fell behind its schedule; rate figures withheld"
        );
    }

    Ok(RunResult {
        clients,
        rep,
        // Overwritten by the caller, which is what knows the schedule.
        order: 0,
        events_total: cfg.events,
        events_delivered: delivered,
        events_superseded: outcome.superseded,
        ops_per_sec,
        drop_rate: drop_rate.clamp(0.0, 1.0),
        p50_us: p50,
        p99_us: p99,
        elapsed_secs: elapsed.as_secs_f64(),
        throughput_valid: completed,
        target_rate: cfg.rate,
        rate_held,
        profile: cfg.profile.clone(),
    })
}

/// The NOSTOS_BENCH_PUSH counting rail: a no-op forward that counts hints, so
/// the enqueue toggle both measures the fan-out cost and proves hints flow
/// through the drain task (see `run_one`).
struct CountingNotifier {
    count: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl nostos_application::ports::PushNotifier for CountingNotifier {
    async fn notify(&self, _hint: nostos_application::ports::PushHint) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// One benchmark client: connect, subscribe, count received frames, record latency.
async fn client_task(
    url: String,
    received: Arc<AtomicU64>,
    hist: Arc<std::sync::Mutex<Histogram>>,
) {
    // `received` is THIS client's own sharded counter — no cross-client contention.
    // Retry connect briefly — the server is starting concurrently.
    let mut ws = None;
    for _ in 0..50 {
        match connect_async(&url).await {
            Ok((stream, _)) => {
                ws = Some(stream);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let Some(ws) = ws else { return };
    let (mut write, mut read) = ws.split();

    // Send subscribe frame — must carry the `type` tag the wire contract
    // requires (wire.rs: `ClientMessage` is `#[serde(tag = "type")]`). The
    // old `{"table":"tasks"}` form silently failed to deserialize once the
    // typed `ClientMessage` enum landed, leaving the session unregistered.
    let sub = serde_json::json!({ "type": "subscribe", "table": "tasks" }).to_string();
    if write.send(Message::Text(sub)).await.is_err() {
        return;
    }

    // Read loop — count FRAMES (not messages): C3 batched writes coalesce N
    // frames into one WS message under backlog, so the per-message count would
    // under-report delivered events. `decode_frames` accepts both the batched
    // array and the legacy single-object form. Record per-message inter-arrival
    // as a latency proxy (p99 reflects recv cadence; true send→recv latency
    // needs server-embedded timestamps, Phase 2).
    while let Some(Ok(msg)) = read.next().await {
        let t = Instant::now();
        let bytes: Vec<u8> = match msg {
            Message::Binary(b) => b,
            Message::Text(s) => s.into_bytes(),
            _ => continue,
        };
        let n = nostos_infra::wire::decode_frames(&bytes).len() as u64;
        if n > 0 {
            received.fetch_add(n, Ordering::Relaxed);
            if let Ok(mut g) = hist.lock() {
                g.record(t.elapsed().as_micros() as u64 + 1);
            }
        }
    }
    let _ = write.close().await;
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,nostos_bench=info")),
        )
        .try_init();
}

// Raise the soft file-descriptor limit so 10k client sockets fit. macOS default
// is often 256, which would cap us well below 10k connections.
fn raise_fd_limit() {
    #[cfg(unix)]
    {
        // `setrlimit` returns `()`. Best-effort — ignore errors (e.g. if the
        // hard limit is already lower than our requested soft limit).
        let _ = nix::sys::resource::setrlimit(
            nix::sys::resource::Resource::RLIMIT_NOFILE,
            65_536,
            65_536,
        );
    }
}
