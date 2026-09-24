//! `nostos-reconnect-storm` — C3 Step 4 probe (advisor-flagged decision point).
//!
//! Batching (C3's main change) fixes *steady-state* throughput. A reconnect
//! storm is a different failure mode: when many clients drop + re-subscribe
//! simultaneously (each re-attaching with a `resume_lsn`), the server must
//! absorb a burst of reconnects while continuing to feed survivors. If the
//! per-session bounded buffers overflow during the re-subscribe window —
//! reconnecting clients aren't draining while they handshake — events drop and
//! a sustained-drop tail can persist after the storm.
//!
//! This probe is the MEASUREMENT, not an admission-control build. Design:
//! 1. `clients` WS clients connect + subscribe; a steady event stream flows.
//! 2. At the storm mark, `storm` of them are told to drop + reconnect with
//!    `resume_lsn` = the max LSN they've received.
//! 3. Compare the drop rate over a window BEFORE the storm vs AFTER it.
//!
//! Verdict (printed, not enacted):
//! - post-storm drop rate > 1% sustained → admission control / token-bucket
//!   follow-up indicated (filed in the report + ROADMAP, NOT built here).
//! - drains to ≤1% → batching held through the storm; move on.
//!
//! Kept deliberately simple — this is a decision-point probe, not a perf tool.

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
use futures_util::{SinkExt, StreamExt};
use nostos_application::{FanOutService, ReplicatorStream, SessionManager};
use nostos_domain::ColumnValue;
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::wire;
use tokio::sync::Notify;
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let clients: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let storm: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    // events emitted before the storm / after the storm (each fanned to all).
    let pre_events: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4_000);
    let post_events: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4_000);

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

    let r = rt.block_on(run(clients, storm, pre_events, post_events));

    let verdict = if r.post_storm_drop_rate > 0.01 {
        "SUSTAINED POST-STORM DROPS — admission control / token-bucket follow-up indicated"
    } else {
        "DRAINS CLEANLY — batching held through the storm; no admission control needed yet"
    };

    eprintln!(
        "\n=== nostos-reconnect-storm probe ===\n  clients            : {clients}\n  storm (drop+reconn): {storm}\n  pre-storm events   : {pre_events} (×{clients} = {} deliveries)\n  post-storm events  : {post_events} (×{clients} = {} deliveries)\n  -------------------------------------\n  pre-storm drop%    : {:.2}\n  post-storm drop%   : {:.2}\n  storm reconnect ms : {:.0}\n  verdict            : {verdict}",
        pre_events.saturating_mul(clients as u64),
        post_events.saturating_mul(clients as u64),
        r.pre_storm_drop_rate * 100.0,
        r.post_storm_drop_rate * 100.0,
        r.storm_reconnect_ms,
    );
    process::exit(0);
}

struct StormResult {
    pre_storm_drop_rate: f64,
    post_storm_drop_rate: f64,
    storm_reconnect_ms: f64,
}

async fn run(clients: usize, storm: usize, pre_events: u64, post_events: u64) -> StormResult {
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

    let stats: Vec<Arc<ClientStat>> = (0..clients).map(|_| Arc::new(ClientStat::new())).collect();
    let mut handles = Vec::with_capacity(clients);
    for stat in &stats {
        let s = Arc::clone(stat);
        let u = url.clone();
        handles.push(tokio::spawn(client_loop(u, s)));
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    let sum_delivered = || {
        stats
            .iter()
            .map(|s| s.delivered.load(Ordering::Relaxed))
            .sum::<u64>()
    };

    let extract = |_: &nostos_domain::ReplicationEvent, _: &str| Some(ColumnValue::Any);

    // ONE continuous stream: pre_events, then storm, then post_events. LSNs are
    // monotonically increasing across the whole stream, so a reconnecting
    // client's resume_lsn (= its max received LSN) only suppresses re-delivery
    // of events it ALREADY got — post-storm events (higher LSNs) always pass.
    // This makes the pre/post drop comparison free of dedup contamination.
    let total = pre_events + post_events;
    let mut repl = FakeReplicator::new(FakeReplicatorConfig::small(total));

    // ---- PRE-STORM window: emit pre_events, let them drain. ----
    let pre_attempted = pre_events.saturating_mul(clients as u64);
    for _ in 0..pre_events {
        if let Some(ev) = repl.next_event().await {
            fanout.fan_out(&ev, &extract).await;
        }
    }
    // Let pre-storm deliveries settle (sample the steady-state drop rate).
    let pre_before = sum_delivered();
    let _ = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let d = sum_delivered();
            if d >= pre_attempted || d == pre_before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    // Give a little more time for full drain, then measure.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pre_delivered = sum_delivered();
    let pre_drop = 1.0 - (pre_delivered as f64 / pre_attempted.max(1) as f64);

    // ---- STORM: drop + reconnect `storm` clients with resume_lsn. ----
    let storm_start = Instant::now();
    let storm_clients = storm.min(clients);
    for stat in stats.iter().take(storm_clients) {
        stat.drop_notify.notify_one();
    }
    // Let the reconnects land (close + reconnect + resubscribe with resume).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let storm_ms = storm_start.elapsed().as_secs_f64() * 1000.0;

    // ---- POST-STORM window: emit post_events, measure delivered delta. ----
    let post_before = sum_delivered();
    let post_attempted = post_events.saturating_mul(clients as u64);
    for _ in 0..post_events {
        if let Some(ev) = repl.next_event().await {
            fanout.fan_out(&ev, &extract).await;
        }
    }
    // Drain window for post-storm deliveries.
    let _ = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let d = sum_delivered();
            let delta = d.saturating_sub(post_before);
            if delta >= post_attempted {
                break;
            }
            // Plateau check: if delivered hasn't moved in 300ms, stop.
            let snap = d;
            tokio::time::sleep(Duration::from_millis(300)).await;
            if sum_delivered() == snap {
                break;
            }
        }
    })
    .await;
    let post_delivered_delta = sum_delivered().saturating_sub(post_before);
    let post_drop = 1.0 - (post_delivered_delta as f64 / post_attempted.max(1) as f64);

    for h in &handles {
        h.abort();
    }

    StormResult {
        pre_storm_drop_rate: pre_drop.clamp(0.0, 1.0),
        post_storm_drop_rate: post_drop.clamp(0.0, 1.0),
        storm_reconnect_ms: storm_ms,
    }
}

struct ClientStat {
    delivered: AtomicU64,
    max_lsn: AtomicU64,
    drop_notify: Notify,
}

impl ClientStat {
    fn new() -> Self {
        Self {
            delivered: AtomicU64::new(0),
            max_lsn: AtomicU64::new(0),
            drop_notify: Notify::new(),
        }
    }
}

/// One client: connect, subscribe, drain frames until told to drop, then
/// reconnect with `resume_lsn` = max received LSN, and keep draining.
async fn client_loop(url: String, stat: Arc<ClientStat>) {
    let mut resume_lsn: Option<u64> = None;
    loop {
        // Connect.
        let mut ws = None;
        for _ in 0..50 {
            if let Ok((stream, _)) = connect_async(&url).await {
                ws = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let Some(ws) = ws else { return };
        let (mut write, mut read) = ws.split();

        // Subscribe with resume_lsn if reconnecting.
        let sub = match resume_lsn {
            Some(l) => serde_json::json!({
                "type": "subscribe", "table": "tasks", "resume_lsn": l
            })
            .to_string(),
            None => serde_json::json!({ "type": "subscribe", "table": "tasks" }).to_string(),
        };
        if write.send(Message::Text(sub)).await.is_err() {
            return;
        }

        // Drain until dropped. We poll read with a short timeout so we can also
        // check the drop notify between frames.
        loop {
            // If a storm drop was requested, close + reconnect.
            // Use a short-timeout recv so the drop_notify is observed promptly.
            tokio::select! {
                () = stat.drop_notify.notified() => {
                    let _ = write.close().await;
                    resume_lsn = Some(stat.max_lsn.load(Ordering::Acquire));
                    break; // reconnect (outer loop)
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            let bytes: Vec<u8> = match m {
                                Message::Binary(b) => b,
                                Message::Text(s) => s.into_bytes(),
                                _ => continue,
                            };
                            for f in wire::decode_frames(&bytes) {
                                stat.delivered.fetch_add(1, Ordering::Relaxed);
                                let mut cur = stat.max_lsn.load(Ordering::Relaxed);
                                while f.lsn > cur {
                                    match stat.max_lsn.compare_exchange_weak(
                                        cur, f.lsn, Ordering::Relaxed, Ordering::Relaxed,
                                    ) {
                                        Ok(_) => break,
                                        Err(o) => cur = o,
                                    }
                                }
                            }
                        }
                        _ => return, // closed
                    }
                }
            }
        }
    }
}
