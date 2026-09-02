# 10k soak root cause — 2026-09-02

Systematic-debugging record for the 10k-client soak (`nostos-bench-10k 10000 5000 60`)
that reported 52% / 76% undelivered in run3 (`benches/results/raw/2026-09-02-run3/`).
Raw logs for every experiment below: `benches/results/raw/2026-09-02-soakdiag/`.

## Phase 1 — evidence (before any fix)

Instrumented the probe (`crates/nostos-bench/src/bin/probe_10k.rs`) with
connection counters (connected / subscribed / connect_failed) and the router's
own `Metrics` (matched / delivered / dropped / faulted). Three runs, host idle
(load ~2), all `ack_interval=1` unless noted:

| run | subscribed @ start | subscribed @ end | events fanned out in 60 s | router dropped | probe drop% |
|---|---|---|---|---|---|
| E1 fresh | 8 944 / 10k | 10 000 | 312 | 0 | 94.2 |
| E2 immediate rerun | 9 174 / 10k | 10 000 | 711 | 0 | 86.1 |
| E3 fresh, ack_interval=5000 | 1 207 / 10k | 4 664 | 3 058 | 0 | 71.5 |

What the numbers say:

- **Nothing is shed.** `router dropped = 0` in every run. The "drop%" the probe
  prints is the fan-out not reaching events inside the 60 s window — a
  throughput ceiling, not slow-client backpressure.
- **The fan-out was starving everything else.** At 10k sessions the router got
  through 5–50 events/s, and client connects that take 0.3 s at 1k took 8–30 s
  (E3 never got past 4 664 connected). The fan-out task and the 20k
  writer/reader tasks share one tokio runtime.
- **Run-to-run variance was 10×** (312 vs 3 058 events) — a scheduler
  pathology, not a clean CPU bound.

### Falsified hypothesis: ephemeral-port exhaustion

macOS has 16 384 ephemeral ports (`net.inet.ip.portrange.first=49152`) and
holds closed sockets in TIME_WAIT for 2×MSL = 30 s (`net.inet.tcp.msl=15000`);
soak 2 started 1 s after soak 1 exited, so the 2× gap between soaks looked like
~10k ports still in TIME_WAIT. Measured: TIME_WAIT after a run was 305, not
10k (`process::exit` tears the sockets down without TIME_WAIT), and E2 had
`connect_failed=0`. Rejected. Sources consulted:
[raby.sh — OSX TIME_WAIT](https://raby.sh/osx-where-are-my-time_wait.html),
[hurl #4 — ephemeral port exhaustion](https://github.com/pquerna/hurl/issues/4).

## Phase 2/3 — hypothesis

`FanOutService::fan_out` spawned **one tokio task per matched session per
event** into a `JoinSet` (10 000 spawns + 10 000 joins per event at 10k), on the
theory that this "spreads across the runtime". But `EventSink::deliver` is a
non-blocking `try_send` (~100 ns) — there is nothing to parallelise, and the
spawn storm competes with the 20k I/O tasks that actually move bytes.
Hypothesis: replacing the JoinSet with a sequential loop on the fan-out task
removes the dominant per-event cost. Panic isolation (the one thing the JoinSet
gave) is kept with `futures_util::FutureExt::catch_unwind`; the
`faulting_delivery_task_is_counted_as_faulted_not_dropped` test still passes.
Reference: [tokio `JoinSet`](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html)
(each `spawn` allocates a task and goes through the scheduler),
[tokio `broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html)
(the alternative shape if per-session channels are ever replaced).

## Phase 4 — one change, measured A/B

Probe also changed to wait for a subscribe quorum (all clients subscribed or
30 s) instead of a fixed 800 ms grace, so "client not connected yet" is no
longer charged to the fan-out. Same probe binary shape, old vs new `fan_out`:

| run | events fanned out | delivered frames | ops/sec | router dropped | probe drop% |
|---|---|---|---|---|---|
| baseline 10k/5000/60 | 174 | 1 547 134 | 25 629 | 0 | 96.9 |
| **fixed 10k/5000/60** | **3 032** | **25 462 314** | **424 363** | 2 270 (0.008%) | 49.1 |
| baseline 1k/100000/60 | 48 920 (window expired) | 48 917 439 | 815 278 | 0 | 51.1 |
| **fixed 1k/100000/60** | **100 000 (done in ~39 s)** | **100 000 000** | **2 570 903** | 0 | **0.00** |

- 10k: 17× more events per window. Real backpressure sheds now appear
  (2 270 of 27.8M, 0.008%) — the first time the 10k soak has shown the router
  reaching a client buffer at all.
- 1k: 3.15× throughput, no regression, zero drops, finished the whole event
  budget inside the window.

## Still open (not fixed here)

> **Status 2026-09-02 (later the same day) — all three items closed or moved; see
> `docs/plans/close-soak-10k-open-items.md`.** (1a) shipped: one `Arc<ReplicationEvent>`
> per event; on Linux 10k delivers 50M/50M in 58.5 s, 0.00% drops, 854,631 ops/sec
> (baseline 28% undelivered). (1b)/(1c) not needed for the goal — not built.
> (2) worked around, not fixed: 10k runs in Docker (`benches/scripts/linux-soak.sh`);
> the macOS socket ceiling is a host limit. (3) Linux 10k A/B + 3 × 1k A/B recorded in
> RESULTS.md; the macOS-native 3-pass headline re-run on the fixed build is the one
> remaining item (`benches/scripts/remeasure.sh`).

1. **10k is still ~49% short of the 60 s budget.** Next candidates, in order,
   each to be measured alone: (a) `event.clone()` per session — `RowOp` holds
   two `String`s, so 20k allocations per event; share via `Arc` or pre-encoded
   `Bytes`; (b) the per-event `min_acked_lsn` full scan at `ack_interval=1`
   (E3 suggests it matters less than the spawn storm, but re-test on the fixed
   build); (c) the writer task re-encodes the same event once per session.
2. **Host limit at ~9.2k loopback sockets on this Mac:** the fixed build
   connects fast enough to hit `ENOBUFS (os error 55)` — macOS mbuf-cluster
   exhaustion (`kern.ipc.nmbclusters=131072`, 128 KiB send+recv space per
   socket). 834 of 10k clients never connected in the fixed 10k run; their
   4.17M attempted deliveries are counted against the probe. Not nostos code;
   a 10k measurement on macOS needs a smaller per-socket buffer in the probe or
   a Linux box. Sources:
   [Rolande — tuning the macOS network stack](https://rolande.wordpress.com/2014/05/17/performance-tuning-the-network-stack-on-mac-os-x-part-2/),
   [JDK-8273158 — ENOBUFS on macOS](https://bugs.java.com/bugdatabase/view_bug?bug_id=8273158).
3. Official numbers: the 3-pass `nostos-bench` re-measure + 10k soak
   (`/tmp/nostos-remeasure3.sh` shape) must be re-run on the fixed build before
   `benches/results/RESULTS.md`, `docs/ROADMAP.md` (P2-1/P2-2 in the soundness
   audit) and `docs/BENCHMARK-METHODOLOGY.md` are updated. The 833 307 ops/sec
   headline was measured on the old fan-out and is now a floor, not the number.
