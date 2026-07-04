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
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::ColumnValue;
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::wire;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// CLI: `nostos-bench-10k <clients> <events> <window_secs>`.
///
/// Defaults mirror the gating 10k comparison: 10k clients, 5k events, 60s window.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let clients: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let events: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let window_secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);

    // Raise FD limit — 10k clients need ~20k+ FDs.
    #[cfg(unix)]
    {
        let _ = nix::sys::resource::setrlimit(
            nix::sys::resource::Resource::RLIMIT_NOFILE,
            65_536,
            65_536,
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result = rt.block_on(run(clients, events, window_secs));

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

async fn run(clients: usize, events: u64, window_secs: u64) -> ProbeResult {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        store.clone(),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(store.clone()));

    let state = SyncRouterState::new(
        Arc::clone(&manager),
        Arc::new(nostos_infra::AllowAnonymous::new()),
    )
    .with_buffer(1024);
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let url = format!("ws://{addr}/sync");

    // Sharded per-client counters (same pattern as the main harness — avoids a
    // single contended cache line at 10k concurrent incrementers).
    let per_client: Vec<Arc<AtomicU64>> =
        (0..clients).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut handles = Vec::with_capacity(clients);
    for cnt in &per_client {
        let c = Arc::clone(cnt);
        let u = url.clone();
        handles.push(tokio::spawn(client_task(u, c)));
    }

    // Let clients connect + subscribe.
    tokio::time::sleep(Duration::from_millis(800)).await;

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
    let drop_rate = 1.0 - (delivered as f64 / attempted as f64);
    let ops_per_sec = (delivered as f64) / elapsed.max(1e-9);

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

/// One client: connect, subscribe, count received FRAMES (not messages — the
/// server may batch N frames per WS message under backlog).
async fn client_task(url: String, received: Arc<AtomicU64>) {
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

    let sub = serde_json::json!({ "type": "subscribe", "table": "tasks" }).to_string();
    if write.send(Message::Text(sub)).await.is_err() {
        return;
    }

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
