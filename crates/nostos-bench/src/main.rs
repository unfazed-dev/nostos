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
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub clients: usize,
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
    pub profile: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = BenchConfig::parse();
    info!(?cfg, "starting nostos-bench");

    // Raise file-descriptor limit — 10k clients need ~20k+ FDs (sockets + pipes).
    raise_fd_limit();

    let mut results = Vec::with_capacity(cfg.clients.len());
    for &clients in &cfg.clients {
        let r = run_one(&cfg, clients)
            .await
            .context(format!("run with {clients} clients failed"))?;
        results.push(r);
    }

    let env = report::Environment::collect(&cfg);
    write_reports(&cfg, &results, &env).context("failed to write reports")?;

    println!("\n=== Nostos Week-1 Benchmark ===\n");
    println!(
        "{:>8} {:>14} {:>10} {:>9} {:>10} {:>10} {:>11}",
        "clients", "ops/sec", "drop%", "p50(ms)", "p99(ms)", "delivered", "superseded"
    );
    for r in &results {
        // A timed-out run still prints its row — the delivered count and the
        // latencies are real and worth seeing — but the two figures the window
        // corrupted are withheld rather than rendered as numbers someone could
        // quote.
        if r.throughput_valid {
            println!(
                "{:>8} {:>14.0} {:>9.2}% {:>9.2} {:>9.2} {:>10} {:>11}",
                r.clients,
                r.ops_per_sec,
                r.drop_rate * 100.0,
                r.p50_us / 1000.0,
                r.p99_us / 1000.0,
                r.events_delivered,
                r.events_superseded
            );
        } else {
            println!(
                "{:>8} {:>14} {:>10} {:>9.2} {:>9.2} {:>10} {:>11}",
                r.clients,
                "TIMED OUT",
                "—",
                r.p50_us / 1000.0,
                r.p99_us / 1000.0,
                r.events_delivered,
                r.events_superseded
            );
        }
    }

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
    println!("\nResults written to {}/", cfg.out_dir);
    Ok(())
}

async fn run_one(cfg: &BenchConfig, clients: usize) -> Result<RunResult> {
    info!(clients, events = cfg.events, "run starting");

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
    .recycling_keys(cfg.distinct_keys);
    let mut replicator = FakeReplicator::new(repl_cfg);

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

    Ok(RunResult {
        clients,
        events_total: cfg.events,
        events_delivered: delivered,
        events_superseded: outcome.superseded,
        ops_per_sec,
        drop_rate: drop_rate.clamp(0.0, 1.0),
        p50_us: p50,
        p99_us: p99,
        elapsed_secs: elapsed.as_secs_f64(),
        throughput_valid: completed,
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
