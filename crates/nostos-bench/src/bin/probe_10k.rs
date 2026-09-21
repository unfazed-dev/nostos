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
         delivered : {}\n  ops/sec   : {:.0}\n  drop%     : {:.2}\n  elapsed   : {:.2}s\n  \
         elapsed_to_finish : {}\n  ops/sec (finish)  : {}",
        events * clients as u64,
        result.delivered,
        result.ops_per_sec,
        result.drop_rate * 100.0,
        result.elapsed_secs,
        result
            .finish_secs
            .map_or_else(|| "n/a (window expired)".to_string(), |s| format!("{s:.2}s")),
        result.finish_secs.map_or_else(
            || "n/a".to_string(),
            |s| format!("{:.0}", result.delivered as f64 / s.max(1e-9)),
        ),
    );
    process::exit(0);
}

struct ProbeResult {
    delivered: u64,
    ops_per_sec: f64,
    drop_rate: f64,
    elapsed_secs: f64,
    /// Seconds from fan-out start to the last reachable delivery; `None` when
    /// the window expired first (then `elapsed_secs` is the window, a floor).
    finish_secs: Option<f64>,
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
    // Prices the client swarm's own decode — see `count_frames_cheap`.
    let skip_decode = std::env::var("NOSTOS_PROBE_SKIP_DECODE").is_ok_and(|v| v == "1");
    if skip_decode {
        eprintln!("  [diag] CLIENT DECODE DISABLED (frame counting only) — harness-cost probe");
    }

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
        handles.push(tokio::spawn(client_task(u, c, cs, skip_decode)));
    }

    // Wait for a subscribe quorum (every client subscribed or given up). The
    // wait is progress-based: keep waiting while the settled count is still
    // rising (any growth inside the last `STALL` window), under a hard ceiling
    // scaled to the client count. Two fixed waits were measured wrong before
    // this: an 800ms grace left 1k–9k of 10k clients connecting when the first
    // event fired, and a max(30s, 1s/1k-clients) cap (2026-09-02 ladder) started
    // fan-out with 4–16% of 30k–100k clients still connecting in a container
    // that connects ~900/s. Both charged those clients' early events to the
    // fan-out as "undelivered" although nothing was ever sent to them. The
    // connect rate is a host property, so the wait must follow it, not a clock.
    let stall = Duration::from_secs(5);
    let quorum_ceiling = Duration::from_secs(60.max(clients as u64 / 200));
    let quorum_start = Instant::now();
    let mut last_settled = 0u64;
    let mut last_progress = Instant::now();
    loop {
        let settled =
            conn.subscribed.load(Ordering::Relaxed) + conn.connect_failed.load(Ordering::Relaxed);
        if settled >= clients as u64 {
            break;
        }
        if settled > last_settled {
            last_settled = settled;
            last_progress = Instant::now();
        }
        if last_progress.elapsed() >= stall || quorum_start.elapsed() >= quorum_ceiling {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let subscribed_at_start = conn.subscribed.load(Ordering::Relaxed);
    let quorum_complete =
        subscribed_at_start + conn.connect_failed.load(Ordering::Relaxed) >= clients as u64;
    eprintln!(
        "  [diag] subscribe quorum after {:.2}s: connected={} subscribed={} connect_failed={} \
         complete={quorum_complete} (ceiling {}s, stall {}s)",
        quorum_start.elapsed().as_secs_f64(),
        conn.connected.load(Ordering::Relaxed),
        subscribed_at_start,
        conn.connect_failed.load(Ordering::Relaxed),
        quorum_ceiling.as_secs(),
        stall.as_secs(),
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
    let cpu_at_start = proc_cpu_secs();
    // Diagnostic: set when `run` returns, i.e. the replicator handed out its
    // whole event budget — distinguishes "loop finished" from "loop stalled".
    let fanout_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fanout_task = {
        let fanout = Arc::clone(&fanout);
        let done = Arc::clone(&fanout_done);
        tokio::spawn(async move {
            let outcome = fanout.run(&mut replicator, extract).await;
            done.store(true, Ordering::Release);
            outcome
        })
    };
    // Diagnostic: a periodic progress line so a slow tier shows WHEN it was
    // slow (uniform vs degrading rate) and what the process footprint was at
    // that moment — the end-of-window totals alone can't tell a steady 1 ev/s
    // from a fast start that collapsed. Additive only; no bearing on results.
    let progress_task = {
        let per_client = per_client.clone();
        let metrics = Arc::clone(&metrics);
        let conn = Arc::clone(&conn);
        let done = Arc::clone(&fanout_done);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await; // first tick fires immediately; skip it
            loop {
                tick.tick().await;
                let delivered: u64 = per_client.iter().map(|a| a.load(Ordering::Relaxed)).sum();
                let matched = metrics.matched.load(Ordering::Relaxed);
                let subscribed = conn.subscribed.load(Ordering::Relaxed).max(1);
                let (rss, swap) = rss_now_mib().unwrap_or((0, 0));
                eprintln!(
                    "  [diag] progress t={:.0}s delivered={delivered} matched={matched} \
                     events~={} subscribed={subscribed} rss_mib={rss} swap_mib={swap} fanout_done={}",
                    start.elapsed().as_secs_f64(),
                    matched / subscribed,
                    done.load(Ordering::Acquire),
                );
            }
        })
    };

    // Wait until every *reachable* delivery has landed, or the window elapses.
    // `target` (clients × events) is unreachable once any subscriber arrived
    // after fan-out started (its pre-subscribe events are never matched), so
    // the loop also stops when fan-out has drained the replicator and the
    // clients hold everything the router actually handed to a sink. That
    // instant is the fan-out finish time; `None` means the window expired
    // first. Resolution is the 50 ms poll.
    let target = events.saturating_mul(clients as u64);
    let deadline = Duration::from_secs(window_secs);
    let finish_secs = tokio::time::timeout(deadline, async {
        loop {
            let delivered = sum();
            if delivered >= target {
                break;
            }
            if fanout_task.is_finished() {
                let reachable = reachable_deliveries(
                    metrics.matched.load(Ordering::Relaxed),
                    metrics.dropped.load(Ordering::Relaxed),
                    metrics.faulted.load(Ordering::Relaxed),
                );
                if delivered >= reachable {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        start.elapsed().as_secs_f64()
    })
    .await
    .ok();
    let elapsed = start.elapsed().as_secs_f64();
    let fanout_done = fanout_task.is_finished();

    let delivered = sum();
    let attempted = events.saturating_mul(clients as u64).max(1);
    {
        let subscribed = conn.subscribed.load(Ordering::Relaxed);
        let matched = metrics.matched.load(Ordering::Relaxed);
        let dropped = metrics.dropped.load(Ordering::Relaxed);
        let faulted = metrics.faulted.load(Ordering::Relaxed);
        let undelivered = undelivered_breakdown(
            clients as u64,
            subscribed,
            events,
            matched,
            dropped,
            faulted,
            delivered,
        );
        // Until the replicator is drained the pre-subscribe term also holds
        // events that simply have not been fanned out yet; say so in the label.
        let pre_subscribe_label = if fanout_done {
            "pre_subscribe_loss"
        } else {
            "pre_subscribe_or_not_yet_fanned_out"
        };
        eprintln!(
            "  [diag] at window end: connected={} subscribed={subscribed} connect_failed={} \
             subscribed_at_start={subscribed_at_start} late_subscribers={} \
             (late subscribers' pre-subscribe events stay in the drop count)\n  \
             [diag] router: matched={matched} delivered={} dropped={dropped} faulted={faulted} \
             events_fanned_out~={} (matched/subscribed)\n  \
             [diag] undelivered breakdown (events): never_connected_x_events={} \
             {pre_subscribe_label}={} router_dropped={} router_faulted={} \
             in_flight_or_unaccounted={} sum={} (attempted-delivered={})",
            conn.connected.load(Ordering::Relaxed),
            conn.connect_failed.load(Ordering::Relaxed),
            subscribed.saturating_sub(subscribed_at_start),
            metrics.delivered.load(Ordering::Relaxed),
            matched / subscribed.max(1),
            undelivered.never_connected,
            undelivered.pre_subscribe_loss,
            undelivered.router_dropped,
            undelivered.router_faulted,
            undelivered.in_flight_or_unaccounted,
            undelivered.total(),
            attempted.saturating_sub(delivered),
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
    // Fan-out finish: the instant the last reachable delivery landed, where
    // reachable = matched − dropped − faulted once the replicator is drained.
    // This is the honest denominator for ops/sec when late subscribers make
    // `target` unreachable; "n/a" means the window expired first.
    eprintln!(
        "  [diag] finish: fanout_done={fanout_done} reachable={} delivered={delivered} elapsed_to_finish={}",
        reachable_deliveries(
            metrics.matched.load(Ordering::Relaxed),
            metrics.dropped.load(Ordering::Relaxed),
            metrics.faulted.load(Ordering::Relaxed),
        ),
        finish_secs.map_or_else(|| "n/a".to_string(), |s| format!("{s:.2}s")),
    );

    // THE per-stage split (added 2026-09-21 for the 100k cliff). `busy` is the
    // time the fan-out task actually spent working; `elapsed` is wall-clock.
    // busy/wall near 1 => the loop IS the cost. busy/wall near 0 => the loop is
    // starved, and the cost is the transport writers, the kernel socket path,
    // or the in-process client tasks competing for the same runtime.
    {
        let ev = metrics.stage_events.load(Ordering::Relaxed).max(1);
        let m_ns = metrics.stage_match_nanos.load(Ordering::Relaxed);
        let d_ns = metrics.stage_deliver_nanos.load(Ordering::Relaxed);
        let a_ns = metrics.stage_ack_scan_nanos.load(Ordering::Relaxed);
        let busy = (m_ns + d_ns + a_ns) as f64 / 1e9;
        eprintln!(
            "  [diag] stages events={ev} match={:.2}ms/ev deliver={:.2}ms/ev ack_scan={:.2}ms/ev \
             busy={busy:.2}s wall={elapsed:.2}s busy_frac={:.3}",
            m_ns as f64 / ev as f64 / 1e6,
            d_ns as f64 / ev as f64 / 1e6,
            a_ns as f64 / ev as f64 / 1e6,
            busy / elapsed.max(1e-9),
        );
        // CPU vs wall: the only thing here that separates "slow" from
        // "starved". See `proc_cpu_secs`.
        match (cpu_at_start, proc_cpu_secs()) {
            (Some(a), Some(b)) => {
                let cpu = b - a;
                eprintln!(
                    "  [diag] cpu proc_cpu={cpu:.2}s wall={elapsed:.2}s cores_used={:.2} \
                     nproc={}",
                    cpu / elapsed.max(1e-9),
                    std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
                );
            }
            _ => eprintln!("  [diag] cpu unavailable (no procfs)"),
        }
    }

    // Diagnostic: did the fan-out loop RETURN (replicator budget exhausted) or
    // was it still running when the window closed? A frozen `delivered` with
    // `true` here is a finite event budget, not a stall.
    eprintln!(
        "  [diag] fanout_task_finished={} (true = replicator budget exhausted before the window closed)",
        fanout_task.is_finished(),
    );

    // Best-effort: signal the fan-out task to wind down (don't await — that can
    // hang too if the replicator is still spinning against full buffers).
    fanout_task.abort();
    progress_task.abort();
    for h in &handles {
        h.abort();
    }

    ProbeResult {
        delivered,
        ops_per_sec,
        drop_rate: drop_rate.clamp(0.0, 1.0),
        elapsed_secs: elapsed,
        finish_secs,
    }
}

/// Deliveries that can still land once fan-out has drained the replicator:
/// the router matched `matched` (event, subscriber) pairs and shed `dropped`
/// plus `faulted` of them. Late subscribers' pre-subscribe events never enter
/// `matched`, so this — not clients × events — is the count the wait loop can
/// actually reach.
fn reachable_deliveries(matched: u64, dropped: u64, faulted: u64) -> u64 {
    matched.saturating_sub(dropped).saturating_sub(faulted)
}

/// Where `attempted − delivered` went, every term in *events* (never clients)
/// so the five sum to `attempted − delivered` exactly, saturation aside.
#[derive(Debug, PartialEq, Eq)]
struct Undelivered {
    /// Clients that never subscribed: all `events` of each are lost.
    never_connected: u64,
    /// `subscribed × events − matched`: events fanned out before a late
    /// subscriber arrived. While the replicator is still draining this also
    /// holds events not yet fanned out at all.
    pre_subscribe_loss: u64,
    router_dropped: u64,
    router_faulted: u64,
    /// Matched and not shed by the router, but not yet counted by a client:
    /// sitting in a sink buffer or a socket.
    in_flight_or_unaccounted: u64,
}

impl Undelivered {
    fn total(&self) -> u64 {
        self.never_connected
            .saturating_add(self.pre_subscribe_loss)
            .saturating_add(self.router_dropped)
            .saturating_add(self.router_faulted)
            .saturating_add(self.in_flight_or_unaccounted)
    }
}

/// Splits `clients × events − delivered` into [`Undelivered`] terms. The
/// pre-2026-09-02 version divided `matched / subscribed` first (integer
/// events-per-client) and multiplied back, so 9,870 lost deliveries at 20k
/// clients printed as 19,954 — one per client — and the terms never summed.
fn undelivered_breakdown(
    clients: u64,
    subscribed: u64,
    events: u64,
    matched: u64,
    dropped: u64,
    faulted: u64,
    delivered: u64,
) -> Undelivered {
    Undelivered {
        never_connected: clients.saturating_sub(subscribed).saturating_mul(events),
        pre_subscribe_loss: subscribed.saturating_mul(events).saturating_sub(matched),
        router_dropped: dropped,
        router_faulted: faulted,
        in_flight_or_unaccounted: reachable_deliveries(matched, dropped, faulted)
            .saturating_sub(delivered),
    }
}

#[cfg(test)]
mod tests {
    use super::{reachable_deliveries, undelivered_breakdown, Undelivered};

    /// Numbers from the validated 20k/300 container run (2026-09-02, host
    /// load1 3.30): 46 clients never subscribed, 42 subscribed late.
    #[test]
    fn breakdown_matches_20k_validation_run() {
        let b = undelivered_breakdown(20_000, 19_954, 300, 5_976_330, 0, 0, 5_976_330);
        assert_eq!(
            b,
            Undelivered {
                never_connected: 13_800,
                pre_subscribe_loss: 9_870,
                router_dropped: 0,
                router_faulted: 0,
                in_flight_or_unaccounted: 0,
            }
        );
        assert_eq!(b.total(), 20_000 * 300 - 5_976_330);
    }

    #[test]
    fn breakdown_terms_sum_to_attempted_minus_delivered() {
        // 10 clients × 100 events; 1 never subscribed, router shed 7, 5 in flight.
        let b = undelivered_breakdown(10, 9, 100, 880, 4, 3, 868);
        assert_eq!(b.never_connected, 100);
        assert_eq!(b.pre_subscribe_loss, 20);
        assert_eq!(b.in_flight_or_unaccounted, 5);
        assert_eq!(b.total(), 10 * 100 - 868);
    }

    #[test]
    fn reachable_excludes_router_shed_and_faulted() {
        assert_eq!(reachable_deliveries(1_000, 0, 0), 1_000);
        assert_eq!(reachable_deliveries(1_000, 30, 5), 965);
    }

    #[test]
    fn reachable_saturates_instead_of_underflowing() {
        assert_eq!(reachable_deliveries(10, 20, 0), 0);
        assert_eq!(reachable_deliveries(10, 5, 20), 0);
    }
}

/// Process CPU seconds (utime + stime) from `/proc/self/stat`; `None` where
/// procfs is absent (macOS).
///
/// This is the discriminator the per-stage wall clocks cannot supply on their
/// own. `stage_*_nanos` span `.await`, so a fan-out task that is descheduled
/// mid-walk still books the interval to its stage — wall time cannot tell
/// "slow" from "starved". CPU time can, at the process level:
///
/// - `cpu / wall` near the vCPU count ⇒ the process is CPU-saturated; the work
///   is real instructions somewhere, and the fan-out task is losing the
///   scheduler race against the client/writer tasks it wakes.
/// - `cpu / wall` far below the vCPU count ⇒ the process is idle or blocked;
///   the cost is a wait (kernel socket path, lock, syscall), not compute, and
///   the "42% idle at an 11.4x collapse" observation finally has a home.
///
/// ponytail: USER_HZ is hardcoded to 100. It is 100 on every Linux the bench
/// containers run; reading it properly means `sysconf(_SC_CLK_TCK)`, which
/// means `libc` + `unsafe`, and `unsafe` is forbidden workspace-wide. Upgrade
/// path if a platform ever disagrees: parse it out of the auxiliary vector.
fn proc_cpu_secs() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` is parenthesised and may itself contain spaces + parens, so the
    // field split must start after the LAST ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After comm, field 0 is `state`; utime is field 11, stime field 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / 100.0)
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

/// Current resident set and swapped-out size in MiB (`VmRSS`, `VmSwap` from
/// `/proc/self/status`); `None` where procfs is absent. Sampled by the
/// periodic progress line — a rising `VmSwap` is the direct signature of the
/// VM paging the server out mid-run.
fn rss_now_mib() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |key: &str| -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok()
    };
    Some((
        field("VmRSS:")? / 1024,
        field("VmSwap:").unwrap_or(0) / 1024,
    ))
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
/// Count frames without parsing them.
///
/// Every `WireFrame` serializes `"lsn":` exactly once, `payload` is hex so it
/// cannot contain the needle, and the bench's `table`/`pk` are `tasks` and a
/// decimal integer — so this is exact for this harness.
///
/// ponytail: substring count, not a parser. It exists to price the probe's OWN
/// client-side JSON decode, which in production runs on the user's device and
/// not on the server's CPU — including it in a server throughput number is a
/// harness artifact. Upgrade path: if a frame ever nests an `lsn`, count at
/// brace-depth 1 instead.
fn count_frames_cheap(data: &[u8]) -> u64 {
    data.windows(6).filter(|w| *w == b"\"lsn\":").count() as u64
}

async fn client_task(
    url: String,
    received: Arc<AtomicU64>,
    stats: Arc<ConnStats>,
    skip_decode: bool,
) {
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
        let n = if skip_decode {
            count_frames_cheap(&bytes)
        } else {
            wire::decode_frames(&bytes).len() as u64
        };
        if n > 0 {
            received.fetch_add(n, Ordering::Relaxed);
        }
    }
    let _ = write.close().await;
}
