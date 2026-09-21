//! `nostos-fanout-walk` — isolates the per-event fan-out walk. No sockets, no
//! clients, no replicator.
//!
//! The 100k cliff (benches/results/RESULTS.md, 2026-09-21) was measured through
//! the whole spine, so "the walk is the cost" was an inference from what the
//! `[sys]` sampler falsified, not a direct reading. This probe measures the
//! walk on its own: N sessions in the real `InMemorySessionStore`, E events
//! through the real `FanOutService`, real `TokioEventSink`s with real drain
//! tasks. Whatever slope it shows is the loop's own, with the network and the
//! client swarm removed.
//!
//! NOT a headline number and never comparable to the fan-out figure in
//! RESULTS.md: no wire encoding, no socket, no client apply. It is a
//! before/after instrument for changes to `FanOutService::fan_out` and
//! `SessionStore::candidates_for`.
//!
//! Usage: `nostos-fanout-walk <clients> <events> [sink=tokio|noop|wire] [buffer] [workers]`
//!
//! `sink=wire` is `tokio` plus the real `encode_event` in each session's drain
//! task — the work the transport's write loop actually does. `deliver` only
//! enqueues an `Arc`, so the encode is invisible to `tokio` mode: it is the
//! single biggest thing separating this probe from the full-spine run.
//!
//! `workers=1` is the sequential walk that shipped before 2026-09-21 — the
//! before number. `0` (the default) means `available_parallelism`.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::uninlined_format_args
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{DeliveryDecision, EventSink, SessionStore};
use nostos_application::FanOutService;
use nostos_domain::{ColumnValue, Lsn, Predicate, ReplicationEvent, RowOp, SyncSession};
use nostos_infra::router::SinkMsg;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::TokioEventSink;

/// Counts deliveries and does nothing else — the floor the walk can reach.
struct CountingSink(Arc<AtomicU64>);

#[async_trait]
impl EventSink for CountingSink {
    async fn deliver(&self, _event: Arc<ReplicationEvent>) -> DeliveryDecision {
        self.0.fetch_add(1, Ordering::Relaxed);
        DeliveryDecision::Delivered
    }
}

const TABLE: &str = "tasks";

fn event(lsn: u64) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: TABLE.into(),
            pk: lsn.to_string(),
            payload: Bytes::from_static(b"{\"org_id\":\"acme\"}"),
        },
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let clients: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let events: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let sink_kind = args.get(3).map_or("tokio", String::as_str);
    let buffer: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1024);
    // 0 = leave the service default (available_parallelism); 1 = sequential.
    let workers: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(run(clients, events, sink_kind, buffer, workers));
}

async fn run(clients: usize, events: u64, sink_kind: &str, buffer: usize, workers: usize) {
    let store = Arc::new(InMemorySessionStore::new());
    let counter = Arc::new(AtomicU64::new(0));

    for _ in 0..clients {
        let session = SyncSession::new(Predicate::all(TABLE));
        let sink: Arc<dyn EventSink> = if sink_kind == "noop" {
            Arc::new(CountingSink(Arc::clone(&counter)))
        } else {
            let (sink, mut rx) = TokioEventSink::channel(buffer);
            let drained = Arc::clone(&counter);
            let encode = sink_kind == "wire";
            // A real drain task per session: the wakeup that `try_send` causes
            // is part of what the walk pays for, so it must be real. In `wire`
            // mode it also runs the real `encode_event`, which is where the
            // transport actually serializes — `deliver` only moves an `Arc`.
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    if encode {
                        if let SinkMsg::Event(e) = &msg {
                            std::hint::black_box(nostos_infra::wire::encode_event(e));
                        }
                    }
                    drained.fetch_add(1, Ordering::Relaxed);
                }
            });
            Arc::new(sink)
        };
        store.add(session, sink).await;
    }

    let mut svc = FanOutService::new(Arc::clone(&store) as Arc<_>);
    if workers > 0 {
        svc = svc.with_fanout_workers(workers);
    }
    // `Predicate::all` ignores columns; the extractor is never consulted.
    let extract = |_: &ReplicationEvent, _: &str| -> Option<ColumnValue> { None };

    // Decile buckets answer "does it decay?" without a second run.
    let bucket_size = (events / 10).max(1);
    let mut buckets: Vec<f64> = Vec::new();
    let mut bucket_start = Instant::now();
    let mut delivered = 0u64;
    let mut dropped = 0u64;

    let started = Instant::now();
    for i in 0..events {
        let outcome = svc.fan_out(&event(i + 1), extract).await;
        delivered += outcome.delivered;
        dropped += outcome.dropped;
        if (i + 1) % bucket_size == 0 {
            buckets.push(bucket_start.elapsed().as_secs_f64() / bucket_size as f64);
            bucket_start = Instant::now();
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    // `wall` stops when the last `try_send` returns, not when the last frame
    // reaches the far end of its channel. With `buffer` >= `events` the drains
    // are still running, so `wall` alone under-counts the pipeline. Wait them
    // out and report both.
    while counter.load(Ordering::Relaxed) < delivered {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let drained_wall = started.elapsed().as_secs_f64();

    let per_event = elapsed / events as f64;
    println!(
        "clients={clients} events={events} sink={sink_kind} buffer={buffer} workers={}",
        if workers == 0 {
            "default".to_string()
        } else {
            workers.to_string()
        }
    );
    println!(
        "delivered={delivered} dropped={dropped} counting_sink_hits={}",
        counter.load(Ordering::Relaxed)
    );
    println!(
        "wall={elapsed:.3}s  per_event={:.3}ms  per_delivery={:.3}us",
        per_event * 1e3,
        per_event / clients as f64 * 1e6
    );
    println!("ops_per_sec={:.0}", delivered as f64 / elapsed);
    println!(
        "drained_wall={drained_wall:.3}s  per_event_drained={:.3}ms  drain_tax={:.2}x",
        drained_wall / events as f64 * 1e3,
        drained_wall / elapsed
    );
    let deciles: Vec<String> = buckets.iter().map(|s| format!("{:.2}", s * 1e3)).collect();
    println!("decile_ms_per_event=[{}]", deciles.join(", "));

    // The OTHER O(sessions) per-event path: `run()` recomputes the slowest
    // acked LSN every `ack_progress_every` events, and that is a full scan
    // under the per-table lock. Timed here so a walk number is never read as
    // the whole server-side per-event cost.
    let t = Instant::now();
    let _ = store.min_acked_lsn().await;
    println!(
        "min_acked_lsn_scan={:.3}ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
