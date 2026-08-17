//! # apply_bench — the client-apply benchmark leg (a nostos-client example).
//!
//! The stock `nostos-bench` harness measures the SERVER fan-out stage:
//! FakeReplicator → FanOutService → WebSocket frame write, counted by frame
//! sinks that never apply a row. This example measures the NEXT stage: N real
//! [`SyncClient`]s, each backed by its own `SqliteStorage`, applying those
//! frames for real — rusqlite writes, durable checkpoints, acks.
//!
//! ## Wiring (the nostos-bench main.rs / reactive_scroll.rs idioms)
//!
//! One process runs:
//!
//! 1. the real `/sync` axum handler (`SyncRouterState` + `InMemorySessionStore`
//!    + `AllowAnonymous`) on `127.0.0.1:<ephemeral>` — the real WS transport
//!      from nostos-infra, not a mock;
//! 2. a `FanOutService` driven by a `FakeReplicator`, wrapped in a counting
//!    proxy so the harness learns the stream's final LSN from the outside
//!    instead of reaching into the replicator's LSN arithmetic;
//! 3. N `SyncClient`s, each with its own `SqliteStorage` (`:memory:` by
//!    default; `--on-disk` profiles a tempdir file DB per client).
//!
//! ## Metrics (and their honest limits)
//!
//! - `rows_applied` — the sum of `ApplyOutcome::rows_applied` over every
//!   commit broadcast, counted by a per-client drain task (not the run loop).
//! - aggregate apply ops/sec — `rows_applied / (start → every client's durable
//!   checkpoint observed at the final LSN)`. Coarse wall-clock; INCLUDES the
//!   fan-out window, so it is a lower bound on pure apply throughput.
//! - drops — `events × clients − rows_applied` (client-side deficit: events
//!   the router dropped, never matched, or that predated a session's
//!   subscribe), cross-checked against the router's own `dropped`/`faulted`
//!   counters: router dropped+faulted must be ≤ the deficit — a violation
//!   fails the run (the two counting paths would be disagreeing).
//! - drain lag — last event emitted (`FanOutService::run` returns) → every
//!   client's durable checkpoint observed at the final LSN. Coarse
//!   wall-clock at 25 ms poll granularity.
//! - checkpoint durability — every client's durable checkpoint must equal the
//!   stream's final LSN; with `--on-disk` each DB is additionally reopened
//!   and its checkpoint re-read from disk.
//!
//! ## Honesty framing (docs/BENCHMARK-METHODOLOGY.md)
//!
//! This leg measures its OWN stage — client apply on loopback against a
//! synthetic replicator — and its numbers are NEVER comparable to the
//! eval-only fan-out headline (833,307 ops/sec aggregate @ 1k clients in
//! benches/results/RESULTS.md): different stage, different units, and that
//! number's sinks do not apply a row. Numbers from short runs are SMOKE
//! evidence that the leg works, not results; production quiet-window runs are
//! the orchestrator's job.
//!
//! Known risk, deliberately not chased: at high client counts N concurrent
//! rusqlite applies share tokio's blocking pool; contention there is a real
//! ceiling of this harness and gets reported, not tuned around.

// Benchmark/reporting code: the `cast_*` and `format_*` pedantic lints fire on
// routine throughput math and report-string building where the flagged patterns
// are acceptable (values within f64 precision; `push_str(&format!(...))` reads
// fine in presentation code). Mirrors nostos-bench's allow for the same
// reporting pattern.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::format_push_string,
    clippy::uninlined_format_args
)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::routing::get;
use clap::Parser;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{sleep, timeout};

use nostos_application::ports::{ReplicatorStream, SessionStore, SyncAuth};
use nostos_application::{FanOutOutcome, FanOutService, SessionManager};
use nostos_client::{SessionOutcome, SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::Storage;
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, Tier};
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

/// Completion-poll granularity for "durable checkpoint reached the final LSN".
/// Coarse on purpose — this is a wall-clock leg, not a latency probe.
const POLL: Duration = Duration::from_millis(25);

/// Settle window for client sessions to connect + subscribe before the
/// FakeReplicator starts emitting (the nostos-bench idiom: fan-out delivers
/// only to sessions registered at fan-out time).
const SETTLE: Duration = Duration::from_millis(500);

/// The sentinel returned when a real value can't be captured (binary missing,
/// non-zero exit, non-UTF-8 output) — the nostos-bench report.rs convention.
const UNKNOWN: &str = "unknown";

/// CLI configuration.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "apply_bench",
    version,
    about = "Nostos client-apply benchmark leg — real SyncClients applying FakeReplicator events"
)]
struct BenchConfig {
    /// Number of concurrent SyncClients (each with its own SqliteStorage).
    #[arg(long, env = "APPLY_BENCH_CLIENTS", default_value_t = 8)]
    clients: usize,

    /// Total replication events driven through the FakeReplicator (each is
    /// fanned out to every client).
    #[arg(long, env = "APPLY_BENCH_EVENTS", default_value_t = 2000)]
    events: u64,

    /// Profile a tempdir file DB per client instead of :memory:.
    #[arg(long)]
    on_disk: bool,

    /// Output directory for the JSON artifact fragment.
    #[arg(long, env = "APPLY_BENCH_OUT", default_value = "benches/results/apply")]
    out_dir: String,

    /// Per-session buffer depth (the router's bounded sink).
    #[arg(long, default_value_t = 1024)]
    buffer: usize,

    /// Per-phase wall-clock timeout (seconds).
    #[arg(long, default_value_t = 120)]
    timeout_secs: u64,

    /// Report a non-zero drop count instead of failing the run (exploratory
    /// high-N legs; a drop is a real finding either way).
    #[arg(long)]
    allow_drops: bool,

    /// Emit pace in events/sec (0 = unpaced burst, the nostos-bench idiom).
    /// Unpaced runs answer "how honestly does it shed under a burst"; paced
    /// runs answer "what rate sustains 0 drops" — both get recorded.
    #[arg(long, env = "APPLY_BENCH_RATE", default_value_t = 0)]
    rate: u64,
}

/// Recorded environment for the artifact (reproducibility — the nostos-bench
/// report.rs convention, plus this leg's own inputs).
#[derive(Debug, Clone, Serialize)]
struct Environment {
    rustc: String,
    hostname: String,
    os: &'static str,
    cpu_cores: usize,
    profile: &'static str,
    clients: usize,
    events: u64,
    on_disk: bool,
    buffer: usize,
    timeout_secs: u64,
    rate_events_per_sec: u64,
}

impl Environment {
    fn collect(cfg: &BenchConfig) -> Self {
        Self {
            rustc: run_capture("rustc", &["--version"]),
            hostname: run_capture("hostname", &[]),
            os: std::env::consts::OS,
            cpu_cores: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            clients: cfg.clients,
            events: cfg.events,
            on_disk: cfg.on_disk,
            buffer: cfg.buffer,
            timeout_secs: cfg.timeout_secs,
            rate_events_per_sec: cfg.rate,
        }
    }
}

/// Router-side counters over the whole run (`FanOutService::run` aggregate) —
/// the honest cross-check for the client-side drop deficit.
#[derive(Debug, Clone, Copy, Serialize)]
struct RouterCounters {
    matched: u64,
    delivered: u64,
    dropped: u64,
    faulted: u64,
}

impl From<FanOutOutcome> for RouterCounters {
    fn from(o: FanOutOutcome) -> Self {
        Self {
            matched: o.matched,
            delivered: o.delivered,
            dropped: o.dropped,
            faulted: o.faulted,
        }
    }
}

/// One client's row in the artifact.
#[derive(Debug, Clone, Serialize)]
struct ClientReport {
    id: usize,
    db: String,
    rows_applied: u64,
    commits_observed: u64,
    broadcast_lagged: u64,
    frames_received: u64,
    session_commits: u64,
    session_checkpoint_lsn: Option<u64>,
    durable_checkpoint_lsn: Option<u64>,
    reopened_checkpoint_lsn: Option<u64>,
    session_error: Option<String>,
}

/// Aggregate numbers for the artifact. Field names carry their own caveats
/// (`coarse`, `poll`) so a reader of the JSON alone cannot mistake a
/// wall-clock aggregate for a stage-isolated rate.
#[derive(Debug, Clone, Serialize)]
struct Totals {
    expected_rows: u64,
    rows_applied: u64,
    drops_client_side: u64,
    drop_rate: f64,
    ops_per_sec_coarse: f64,
    completed_all_checkpoints: bool,
    elapsed_secs: f64,
    emit_wall_secs: f64,
    drain_lag_ms_poll25: Option<f64>,
}

/// The full JSON fragment written to `<out_dir>/run.json`. A FRAGMENT, never
/// benches/results/RESULTS.md — the stock bench owns that file.
#[derive(Serialize)]
struct Artifacts {
    label: &'static str,
    stage_note: &'static str,
    environment: Environment,
    totals: Totals,
    router: RouterCounters,
    final_lsn: u64,
    per_client: Vec<ClientReport>,
}

/// Runtime stats for one bench client, filled by the drain task.
#[derive(Debug, Default)]
struct BenchClient {
    rows_applied: u64,
    commits_observed: u64,
    lagged: u64,
    last_apply: Option<Instant>,
    session: Option<Result<SessionOutcome, String>>,
}

impl BenchClient {
    fn record_outcome(&mut self, outcome: nostos_core::ApplyOutcome) {
        self.rows_applied += outcome.rows_applied as u64;
        self.commits_observed += 1;
        self.last_apply = Some(Instant::now());
    }
}

/// A pass-through around the FakeReplicator that remembers the last emitted
/// event's LSN, so the harness learns the stream's final LSN (the checkpoint
/// target) without depending on the replicator's internal LSN arithmetic.
struct CountingReplicator {
    inner: FakeReplicator,
    last_lsn: Arc<AtomicU64>,
    /// Per-event spacing when `--rate` > 0 (`None` = the unpaced burst).
    interval: Option<Duration>,
    next_at: Option<Instant>,
}

#[async_trait]
impl ReplicatorStream for CountingReplicator {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        if let Some(interval) = self.interval {
            // Token-pace (the bench_pg_ingest shape): schedule per event;
            // a behind-schedule tick simply doesn't sleep — no catch-up
            // burst, the pace is a ceiling not a compensation.
            let at = self.next_at.get_or_insert_with(Instant::now);
            let now = Instant::now();
            if *at > now {
                tokio::time::sleep(*at - now).await;
            }
            *at += interval;
        }
        let event = self.inner.next_event().await?;
        self.last_lsn.store(event.lsn.raw(), Ordering::Relaxed);
        Some(event)
    }
}

/// Week-1 extractor (the nostos-bench idiom): the synthetic payload is opaque
/// bytes; match on table only (`ColumnValue::Any` matches every value), so
/// every event fans out to every subscribed session.
// The Option return is pinned by the FanOutService::run extractor callback type.
#[allow(clippy::unnecessary_wraps)]
fn extract_any(_event: &ReplicationEvent, _column: &str) -> Option<ColumnValue> {
    Some(ColumnValue::Any)
}

/// Spawn the in-process sync server: the real `/sync` handler on an ephemeral
/// loopback port, anonymous auth (no principal, no tenant filter — the bench
/// is not gated on JWT minting). Mirrors reactive_scroll's `spawn_server`
/// minus the write-back rail (this leg only reads).
async fn spawn_server(
    store: Arc<dyn SessionStore>,
    auth: Arc<dyn SyncAuth>,
    buffer: usize,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let manager = Arc::new(SessionManager::new(store, Tier::Enterprise));
    let state = SyncRouterState::new(manager, auth).with_buffer(buffer);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (addr, handle)
}

/// One bench client: run the sync loop while a sidecar drain in THIS task
/// counts `ApplyOutcome` broadcasts. Subscribing before the run starts (and
/// the post-run `try_recv` drain) makes the `rows_applied` count exact up to
/// broadcast capacity — a `Lagged` receiver is counted and reported honestly
/// rather than silently under-counted.
async fn client_task(client: Arc<SyncClient<SqliteStorage>>) -> BenchClient {
    let mut changes = client.subscribe_changes();
    let mut run = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.run_with_reconnect().await }
    });
    let mut bench = BenchClient::default();
    loop {
        tokio::select! {
            outcome = changes.recv() => match outcome {
                Ok(o) => bench.record_outcome(o),
                Err(RecvError::Lagged(n)) => bench.lagged += n,
                // The client (and its broadcast sender) outlives the loop, so
                // Closed is not expected — handle it defensively anyway.
                Err(RecvError::Closed) => {
                    while let Ok(o) = changes.try_recv() {
                        bench.record_outcome(o);
                    }
                    bench.session =
                        Some(run.await.expect("client run task must not panic").map_err(|e| e.to_string()));
                    return bench;
                }
            },
            joined = &mut run => {
                // Final drain: the last flush's broadcast can land after the
                // run loop resolved but before this branch observed it.
                while let Ok(o) = changes.try_recv() {
                    bench.record_outcome(o);
                }
                bench.session =
                    Some(joined.expect("client run task must not panic").map_err(|e| e.to_string()));
                return bench;
            }
        }
    }
}

/// Shell out once, returning trimmed UTF-8 stdout or [`UNKNOWN`] on any error
/// (missing binary, non-zero exit, non-UTF-8) — the nostos-bench report.rs
/// `run_capture` convention.
fn run_capture(cmd: &str, args: &[&str]) -> String {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
            Ok(s) => s.trim().to_string(),
            Err(_) => UNKNOWN.to_string(),
        },
        _ => UNKNOWN.to_string(),
    }
}

/// Write the JSON artifact fragment into the out-dir. Creates the dir on
/// demand; NEVER touches benches/results/RESULTS.md (the stock bench owns it).
fn write_artifacts(out_dir: &str, artifacts: &Artifacts) {
    std::fs::create_dir_all(out_dir).expect("create out dir");
    let json = serde_json::to_string_pretty(artifacts).expect("serialize artifacts");
    let path = Path::new(out_dir).join("run.json");
    std::fs::write(&path, json).expect("write run.json");
    println!("Artifact fragment written to {}", path.display());
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cfg = BenchConfig::parse();
    let storage_mode = if cfg.on_disk {
        "on-disk tempdir"
    } else {
        ":memory:"
    };
    println!(
        "=== nostos apply_bench — client-apply leg: {} clients × {} events ({storage_mode}) ===",
        cfg.clients, cfg.events
    );
    println!("HONESTY: this leg measures client apply on loopback against a synthetic replicator;");
    println!(
        "it is NEVER comparable to the eval-only fan-out headline (833,307 ops/sec — different"
    );
    println!("stage, different units; see docs/BENCHMARK-METHODOLOGY.md). Label short-run numbers SMOKE.\n");

    let mut failures: Vec<String> = Vec::new();

    // ---- in-process server: the real /sync handler on an ephemeral port ----
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server_guard) =
        spawn_server(Arc::clone(&store), Arc::clone(&auth), cfg.buffer).await;
    let url = format!("ws://{addr}/sync");
    println!(
        "[server] real /sync transport on {url} (buffer {})",
        cfg.buffer
    );

    // ---- N SyncClients, each with its own SqliteStorage ----
    // On-disk mode uses one tempdir per run (PID-suffixed, the reactive_scroll
    // idiom) with one file DB per client, left in place for post-run inspection.
    let db_dir = format!(
        "{}/nostos-apply-bench-{}",
        std::env::temp_dir().display(),
        std::process::id()
    );
    if cfg.on_disk {
        std::fs::create_dir_all(&db_dir).expect("create client tempdir");
    }
    let mut db_paths: Vec<Option<String>> = Vec::with_capacity(cfg.clients);
    let mut clients: Vec<Arc<SyncClient<SqliteStorage>>> = Vec::with_capacity(cfg.clients);
    for i in 0..cfg.clients {
        let (storage, db) = if cfg.on_disk {
            let path = format!("{db_dir}/client-{i}.db");
            (
                SqliteStorage::open(&path).expect("open on-disk sqlite"),
                path,
            )
        } else {
            (
                SqliteStorage::open_in_memory().expect("open in-memory sqlite"),
                String::new(),
            )
        };
        // idle_timeout: None — the client runs until the harness raises the
        // non-destructive disconnect() gate (ADR-0037 task 5.1); no aborts
        // mid-apply. flush_quiesce keeps its 50ms default so the final
        // transaction's frames close without a disconnect.
        let config = SyncClientConfig {
            table: "tasks".to_owned(),
            idle_timeout: None,
            max_retries: None,
            ..SyncClientConfig::default()
        };
        clients.push(Arc::new(SyncClient::new(url.clone(), storage, config)));
        db_paths.push(if cfg.on_disk { Some(db) } else { None });
    }

    let mut handles = Vec::with_capacity(clients.len());
    for client in &clients {
        let client = Arc::clone(client);
        handles.push(tokio::spawn(client_task(client)));
    }

    // Let the sessions connect + subscribe before any event is emitted —
    // fan-out only reaches sessions registered at fan-out time, and a
    // late subscriber's deficit is honestly counted as a drop below.
    sleep(SETTLE).await;

    // ---- drive the FakeReplicator through the real FanOutService ----
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));
    let mut replicator = CountingReplicator {
        inner: FakeReplicator::new(FakeReplicatorConfig::small(cfg.events)),
        last_lsn: Arc::new(AtomicU64::new(0)),
        interval: (cfg.rate > 0).then(|| Duration::from_secs_f64(1.0 / cfg.rate as f64)),
        next_at: None,
    };
    let last_lsn_handle = Arc::clone(&replicator.last_lsn);

    let start = Instant::now();
    let mut fanout_task =
        tokio::spawn(async move { fanout.run(&mut replicator, extract_any).await });
    let outcome = if let Ok(joined) =
        timeout(Duration::from_secs(cfg.timeout_secs), &mut fanout_task).await
    {
        joined.expect("fan-out task must not panic")
    } else {
        fanout_task.abort();
        failures.push(format!(
            "fan-out did not finish within {}s; run aborted (numbers below are partial)",
            cfg.timeout_secs
        ));
        FanOutOutcome::default()
    };
    let last_emit = Instant::now();
    let final_lsn = Lsn::new(last_lsn_handle.load(Ordering::Relaxed));
    let emit_wall = last_emit.duration_since(start);
    println!(
        "[fan-out] {} events emitted through the real router in {:.1?} (final LSN {final_lsn}; router matched={}, delivered={}, dropped={}, faulted={})",
        cfg.events, emit_wall, outcome.matched, outcome.delivered, outcome.dropped, outcome.faulted
    );

    // ---- wait until every client's DURABLE checkpoint reaches the final LSN ----
    let wait_budget = Duration::from_secs(cfg.timeout_secs);
    let reached = timeout(wait_budget, async {
        loop {
            let mut all = true;
            for c in &clients {
                if !c.checkpoint().await.is_ok_and(|cp| cp >= final_lsn) {
                    all = false;
                    break;
                }
            }
            if all {
                break;
            }
            sleep(POLL).await;
        }
    })
    .await
    .is_ok();
    let all_applied_at = Instant::now();
    let elapsed = all_applied_at.duration_since(start);
    if !reached {
        failures.push(format!(
            "durable checkpoints did not all reach the final LSN {final_lsn} within {}s              (a client-side drop deficit is the likely cause — see drops below)",
            cfg.timeout_secs
        ));
    }

    // ---- non-destructive shutdown: the disconnect() gate, no task aborts ----
    for client in &clients {
        client.disconnect();
    }
    let mut benches: Vec<BenchClient> = Vec::with_capacity(clients.len());
    for (i, handle) in handles.into_iter().enumerate() {
        match timeout(Duration::from_secs(30), handle).await {
            Ok(Ok(bench)) => benches.push(bench),
            Ok(Err(e)) => {
                failures.push(format!("client {i}: bench task failed: {e}"));
                benches.push(BenchClient::default());
            }
            Err(_) => {
                failures.push(format!(
                    "client {i}: did not wind down within 30s of disconnect()"
                ));
                benches.push(BenchClient::default());
            }
        }
    }

    // ---- durable checkpoint assertions (post-disconnect readback) ----
    let mut reopened_lsns: Vec<Option<u64>> = vec![None; clients.len()];
    let mut durable_lsns: Vec<Option<u64>> = vec![None; clients.len()];
    for (i, (client, bench)) in clients.iter().zip(&benches).enumerate() {
        match client.checkpoint().await {
            Ok(cp) => {
                durable_lsns[i] = Some(cp.raw());
                if cp != final_lsn {
                    failures.push(format!(
                        "client {i}: durable checkpoint {cp} != final LSN {final_lsn}"
                    ));
                }
                if let Some(Err(e)) = &bench.session {
                    failures.push(format!("client {i}: session ended with error: {e}"));
                }
                // On-disk mode: reopen the file and re-read the checkpoint from
                // disk — the durability proof independent of the live engine.
                if let Some(path) = &db_paths[i] {
                    let reopened = SqliteStorage::open(path).expect("reopen on-disk sqlite");
                    let disk_cp = reopened.checkpoint().expect("reopened checkpoint");
                    if disk_cp != final_lsn {
                        failures.push(format!(
                            "client {i}: reopened checkpoint {disk_cp} != final LSN {final_lsn}"
                        ));
                    }
                    reopened_lsns[i] = Some(disk_cp.raw());
                }
            }
            Err(e) => failures.push(format!("client {i}: checkpoint readback failed: {e}")),
        }
    }

    // ---- aggregate + honest drop accounting ----
    let rows_total: u64 = benches.iter().map(|b| b.rows_applied).sum();
    let lagged_total: u64 = benches.iter().map(|b| b.lagged).sum();
    let expected = cfg.events.saturating_mul(cfg.clients as u64);
    let drops = expected.saturating_sub(rows_total);
    let drop_rate = if expected > 0 {
        drops as f64 / expected as f64
    } else {
        0.0
    };
    let ops_per_sec = rows_total as f64 / elapsed.as_secs_f64().max(1e-9);
    if rows_total > expected {
        failures.push(format!(
            "rows_applied ({rows_total}) exceeds events × clients ({expected}) —              over-count (replayed frames?) makes every other number suspect"
        ));
    }
    if drops > 0 && !cfg.allow_drops {
        failures.push(format!(
            "{drops} drops (events × clients − rows_applied); at smoke scale this should be 0              (pass --allow-drops to record-and-continue on an exploratory leg)"
        ));
    }
    // Router cross-check — actually performed, not just recorded: every
    // frame the router dropped or faulted never reached a client, so the
    // router-accounted loss must be ≤ the client-side deficit (the deficit
    // additionally covers in-flight/unapplied frames). A violation means
    // the two counting paths disagree and every drop number is suspect.
    let router_accounted = outcome.dropped.saturating_add(outcome.faulted);
    if router_accounted > drops {
        failures.push(format!(
            "router cross-check FAILED: router dropped+faulted ({router_accounted}) exceeds the client-side deficit ({drops}) — counting paths disagree; numbers suspect"
        ));
    }
    if lagged_total > 0 {
        failures.push(format!(
            "broadcast drain lagged {lagged_total} commits — rows_applied UNDER-countS;              the metric is compromised at this scale"
        ));
    }

    // ---- report ----
    println!(
        "\n=== SMOKE results (own-stage, loopback; NOT comparable to the fan-out headline) ==="
    );
    println!(
        "{:>8} {:>9} {:>12} {:>14} {:>8} {:>12}",
        "clients", "events", "rows_applied", "ops/sec(coarse)", "drop%", "drain_lag_ms"
    );
    let drain_lag_ms = reached.then(|| {
        let us = all_applied_at.duration_since(last_emit).as_secs_f64() * 1000.0;
        if us < 0.0 {
            0.0
        } else {
            us
        }
    });
    println!(
        "{:>8} {:>9} {:>12} {:>14.0} {:>7.2}% {:>12}",
        cfg.clients,
        cfg.events,
        rows_total,
        ops_per_sec,
        drop_rate * 100.0,
        drain_lag_ms.map_or("-".to_string(), |v| format!("{v:.1}"))
    );
    println!(
        "expected rows (events × clients): {expected}; emit wall: {:.3}s; total wall: {:.3}s",
        emit_wall.as_secs_f64(),
        elapsed.as_secs_f64()
    );
    println!(
        "router cross-check: dropped+faulted {router_accounted} accounts for {:.1}% of the {drops} client-side deficit (rest = in-flight/unapplied at drain)",
        if drops > 0 {
            router_accounted as f64 / drops as f64 * 100.0
        } else {
            100.0
        }
    );
    for (i, (bench, db)) in benches.iter().zip(&db_paths).enumerate() {
        let session_cp = bench
            .session
            .as_ref()
            .and_then(|s| s.as_ref().ok())
            .map(|o| o.checkpoint.raw());
        println!(
            "  client {i}: rows={}, commits={}, frames={:?}, session_cp={:?}, db={}",
            bench.rows_applied,
            bench.commits_observed,
            bench
                .session
                .as_ref()
                .and_then(|s| s.as_ref().ok())
                .map(|o| o.frames_received),
            session_cp,
            db.as_deref().map_or(":memory:", |p| p)
        );
    }

    let per_client = benches
        .iter()
        .enumerate()
        .map(|(i, bench)| ClientReport {
            id: i,
            db: db_paths[i]
                .clone()
                .unwrap_or_else(|| ":memory:".to_string()),
            rows_applied: bench.rows_applied,
            commits_observed: bench.commits_observed,
            broadcast_lagged: bench.lagged,
            frames_received: bench
                .session
                .as_ref()
                .and_then(|s| s.as_ref().ok())
                .map_or(0, |o| o.frames_received),
            session_commits: bench
                .session
                .as_ref()
                .and_then(|s| s.as_ref().ok())
                .map_or(0, |o| o.commits),
            session_checkpoint_lsn: bench
                .session
                .as_ref()
                .and_then(|s| s.as_ref().ok())
                .map(|o| o.checkpoint.raw()),
            durable_checkpoint_lsn: durable_lsns[i],
            reopened_checkpoint_lsn: reopened_lsns[i],
            session_error: bench
                .session
                .as_ref()
                .and_then(|s| s.as_ref().err().cloned()),
        })
        .collect();

    let artifacts = Artifacts {
        label: "SMOKE",
        stage_note: "client-apply leg: N SyncClients applying FakeReplicator events through the \
real /sync WS transport on 127.0.0.1 loopback. Measures its own stage only; NOT \
comparable to the eval-only fan-out headline in benches/results/RESULTS.md \
(different stage, different units — docs/BENCHMARK-METHODOLOGY.md).",
        environment: Environment::collect(&cfg),
        totals: Totals {
            expected_rows: expected,
            rows_applied: rows_total,
            drops_client_side: drops,
            drop_rate,
            ops_per_sec_coarse: ops_per_sec,
            completed_all_checkpoints: reached && drops == 0,
            elapsed_secs: elapsed.as_secs_f64(),
            emit_wall_secs: emit_wall.as_secs_f64(),
            drain_lag_ms_poll25: drain_lag_ms,
        },
        router: RouterCounters::from(outcome),
        final_lsn: final_lsn.raw(),
        per_client,
    };
    write_artifacts(&cfg.out_dir, &artifacts);

    if failures.is_empty() {
        println!("\nOK: 0 drops; every durable checkpoint reached the final LSN {final_lsn}.");
    } else {
        println!("\nFAILURES (reported honestly, not spun):");
        for f in &failures {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}
