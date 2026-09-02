//! `nostos-bench-10k` — a lean 10k-client measurement probe (C3 batched-writes).
//!
//! The full `nostos-bench` harness hangs in teardown at 10k clients (the 10k-way
//! per-client latency-histogram mutex merge + the JoinSet of 10k client spawns
//! never reaps cleanly once the wait-loop times out). That hang blocks getting
//! the 10k before/after numbers that ARE the point of C3. This probe reproduces
//! the SAME measurement (real axum server, real `FanOutService`, real
//! `FakeReplicator`, N WebSocket clients counting received FRAMES) but:
//!
//! - drops the per-client histograms entirely (the hang source),
//! - counts frames via `wire::decode_frames` (correct under batched writes),
//! - drives fan-out to exhaustion with a bounded event count,
//! - prints throughput + drop rate and `process::exit`s — no graceful teardown.
//!
//! Not the headline reporter; a measurement shim so the Tier comparison has
//! real 10k numbers. Same-denominator as `nostos-bench` (delivered frames /
//! wall-clock; drop rate = 1 − delivered / attempted).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::uninlined_format_args
)]

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use nostos_application::ports::Metrics;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::ColumnValue;
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::wire;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// CLI: `nostos-bench-10k <clients> <events> <window_secs> [ack_interval] [listeners]`.
///
/// Defaults mirror the gating 10k comparison: 10k clients, 5k events, 60s window.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let clients: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let events: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let window_secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    // Arg 4: ack-progress coalesce interval (1 = every event = exact ADR-0009;
    // >1 = the 10k fix, recomputing the slowest acked LSN every N events). Lets
    // the same binary produce before (1) / after (N) 10k numbers.
    let ack_interval: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
    // Arg 5: number of loopback listener addresses (127.0.0.1 ..= 127.0.0.N).
    // One destination caps the ephemeral 4-tuple space at ~64k on Linux, so
    // tiers above ~60k clients need >1 (docs/plans/scale-ladder-20k-100k.md).
    // Linux routes all of 127/8 to `lo`; macOS only 127.0.0.1 without an alias.
    let listeners: u8 = args
        .get(5)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 254);

    // Raise the FD soft limit to the hard limit — the harness is in-process, so
    // every client costs two sockets (client side + server side): 10k = 20k FDs,
    // 100k = 200k FDs. A fixed 65_536 silently capped the ladder at ~30k.
    #[cfg(unix)]
    {
        use nix::sys::resource::{getrlimit, setrlimit, Resource};
        if let Ok((_, hard)) = getrlimit(Resource::RLIMIT_NOFILE) {
            let _ = setrlimit(Resource::RLIMIT_NOFILE, hard, hard);
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result = rt.block_on(run(clients, events, window_secs, ack_interval, listeners));

    // Print and exit hard — no graceful teardown (that's what hangs at 10k).
    eprintln!(
        "\n=== nostos-bench-10k probe ===\n  clients   : {clients}\n  events    : {events} \
         (attempted deliveries: {})\n  window    : {window_secs}s\n  ---------------------------\n  \
         delivered : {}\n  ops/sec   : {:.0}\n  drop%     : {:.2}\n  elapsed   : {:.2}s",
        events * clients as u64,
        result.delivered,
        result.ops_per_sec,
        result.drop_rate * 100.0,
        result.elapsed_secs,
    );
    process::exit(0);
}

struct ProbeResult {
    delivered: u64,
    ops_per_sec: f64,
    drop_rate: f64,
    elapsed_secs: f64,
}

async fn run(
    clients: usize,
    events: u64,
    window_secs: u64,
    ack_interval: u32,
    listeners: u8,
) -> ProbeResult {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        store.clone(),
        nostos_domain::Tier::Enterprise,
    ));
    // Diagnostic: router-side counters so the report can separate "server shed
    // it (buffer full)" from "server never got to it in the window" from
    // "client never connected". Without these the drop% is one opaque number.
    let metrics = Arc::new(Metrics::new());
    let fanout = Arc::new(
        FanOutService::new(store.clone())
            .with_ack_progress_every(ack_interval)
            .with_metrics(Arc::clone(&metrics)),
    );

    let state = SyncRouterState::new(
        Arc::clone(&manager),
        Arc::new(nostos_infra::AllowAnonymous::new()),
    )
    .with_buffer(1024);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    // One axum app served on `listeners` loopback addresses; every listener
    // shares the same router state, so the server side is still one process
    // with one FanOutService — only the client 4-tuple space is widened.
    let mut urls = Vec::with_capacity(usize::from(listeners));
    for i in 1..=listeners {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::new(127, 0, 0, i), 0))
            .await
            .unwrap_or_else(|e| panic!("bind 127.0.0.{i}:0: {e}"));
        let addr = listener.local_addr().unwrap();
        urls.push(format!("ws://{addr}/sync"));
        let app = app.clone();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
    }
    eprintln!("  [diag] listeners: {}", urls.join(" "));

    // Sharded per-client counters (same pattern as the main harness — avoids a
    // single contended cache line at 10k concurrent incrementers).
    let per_client: Vec<Arc<AtomicU64>> =
        (0..clients).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let conn = Arc::new(ConnStats::default());
    let mut handles = Vec::with_capacity(clients);
    for (i, cnt) in per_client.iter().enumerate() {
        let c = Arc::clone(cnt);
        let u = urls[i % urls.len()].clone();
        let cs = Arc::clone(&conn);
        handles.push(tokio::spawn(client_task(u, c, cs)));
    }

    // Wait for a subscribe quorum (every client subscribed or given up), capped
    // at 30s or 1s per 1k clients, whichever is larger (a 100k connect storm
    // needs well over 30s). A fixed 800ms grace was measured leaving 1k–9k of
    // 10k clients still connecting when the first event fired (2026-09-02
    // diag), which silently charged those events as "undelivered" to the fan-out.
    let quorum_cap = Duration::from_secs(30.max(clients as u64 / 1_000));
    let quorum_start = Instant::now();
    loop {
        let settled =
            conn.subscribed.load(Ordering::Relaxed) + conn.connect_failed.load(Ordering::Relaxed);
        if settled >= clients as u64 || quorum_start.elapsed() >= quorum_cap {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!(
        "  [diag] subscribe quorum after {:.2}s: connected={} subscribed={} connect_failed={}",
        quorum_start.elapsed().as_secs_f64(),
        conn.connected.load(Ordering::Relaxed),
        conn.subscribed.load(Ordering::Relaxed),
        conn.connect_failed.load(Ordering::Relaxed),
    );

    let sum = || {
        per_client
            .iter()
            .map(|a| a.load(Ordering::Relaxed))
            .sum::<u64>()
    };

    // Drive the FakeReplicator through the real FanOutService.
    let mut replicator = FakeReplicator::new(FakeReplicatorConfig::small(events));
    let extract = |_: &nostos_domain::ReplicationEvent, _: &str| Some(ColumnValue::Any);

    let start = Instant::now();
    let fanout_task = {
        let fanout = Arc::clone(&fanout);
        tokio::spawn(async move { fanout.run(&mut replicator, extract).await })
    };

    // Wait until all events delivered, or the window elapses.
    let target = events.saturating_mul(clients as u64);
    let deadline = Duration::from_secs(window_secs);
    let _ = tokio::time::timeout(deadline, async {
        loop {
            if sum() >= target {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let elapsed = start.elapsed().as_secs_f64();

    let delivered = sum();
    let attempted = events.saturating_mul(clients as u64).max(1);
    {
        let subscribed = conn.subscribed.load(Ordering::Relaxed).max(1);
        let matched = metrics.matched.load(Ordering::Relaxed);
        eprintln!(
            "  [diag] at window end: connected={} subscribed={} connect_failed={}\n  \
             [diag] router: matched={} delivered={} dropped={} faulted={} \
             events_fanned_out~={} (matched/subscribed)\n  \
             [diag] undelivered breakdown: never_connected={} not_reached_in_window={} \
             router_dropped={} in_flight_or_unaccounted={}",
            conn.connected.load(Ordering::Relaxed),
            conn.subscribed.load(Ordering::Relaxed),
            conn.connect_failed.load(Ordering::Relaxed),
            matched,
            metrics.delivered.load(Ordering::Relaxed),
            metrics.dropped.load(Ordering::Relaxed),
            metrics.faulted.load(Ordering::Relaxed),
            matched / subscribed,
            (clients as u64).saturating_sub(subscribed) * events,
            subscribed * events.saturating_sub(matched / subscribed),
            metrics.dropped.load(Ordering::Relaxed),
            metrics
                .delivered
                .load(Ordering::Relaxed)
                .saturating_sub(delivered),
        );
    }
    let drop_rate = 1.0 - (delivered as f64 / attempted as f64);
    let ops_per_sec = (delivered as f64) / elapsed.max(1e-9);
    // First-class "did it finish inside the window" flag + peak RSS, so a
    // truncated tier is reported as "X/N delivered in W s", never as a rate.
    eprintln!(
        "  [diag] completed={} (delivered {delivered} of {target} before the {window_secs}s window) peak_rss_mib={}",
        delivered >= target,
        peak_rss_mib().map_or_else(|| "n/a".to_string(), |m| m.to_string()),
    );

    // Best-effort: signal the fan-out task to wind down (don't await — that can
    // hang too if the replicator is still spinning against full buffers).
    fanout_task.abort();
    for h in &handles {
        h.abort();
    }

    ProbeResult {
        delivered,
        ops_per_sec,
        drop_rate: drop_rate.clamp(0.0, 1.0),
        elapsed_secs: elapsed,
    }
}

/// Peak resident set size in MiB from `/proc/self/status` (`VmHWM`); `None`
/// where procfs is absent (macOS). Memory is the likelier wall before ports
/// at 100k in-process clients, so every tier records it.
fn peak_rss_mib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

/// Diagnostic connection counters (see the `[diag]` lines in `run`).
#[derive(Default)]
struct ConnStats {
    connected: AtomicU64,
    subscribed: AtomicU64,
    connect_failed: AtomicU64,
}

/// One client: connect, subscribe, count received FRAMES (not messages — the
/// server may batch N frames per WS message under backlog).
async fn client_task(url: String, received: Arc<AtomicU64>, stats: Arc<ConnStats>) {
    let mut ws = None;
    let mut last_err = String::new();
    for _ in 0..50 {
        match connect_async(&url).await {
            Ok((stream, _)) => {
                ws = Some(stream);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let Some(ws) = ws else {
        // Print the first few failures verbatim — the error text is the
        // evidence (EADDRNOTAVAIL = ephemeral-port exhaustion, ECONNREFUSED =
        // accept backlog, etc.).
        if stats.connect_failed.fetch_add(1, Ordering::Relaxed) < 3 {
            eprintln!("  [diag] connect failed after 50 tries: {last_err}");
        }
        return;
    };
    stats.connected.fetch_add(1, Ordering::Relaxed);
    let (mut write, mut read) = ws.split();

    let sub = serde_json::json!({ "type": "subscribe", "table": "tasks" }).to_string();
    if write.send(Message::Text(sub)).await.is_err() {
        return;
    }
    stats.subscribed.fetch_add(1, Ordering::Relaxed);

    while let Some(Ok(msg)) = read.next().await {
        let bytes: Vec<u8> = match msg {
            Message::Binary(b) => b,
            Message::Text(s) => s.into_bytes(),
            _ => continue,
        };
        let n = wire::decode_frames(&bytes).len() as u64;
        if n > 0 {
            received.fetch_add(n, Ordering::Relaxed);
        }
    }
    let _ = write.close().await;
}
