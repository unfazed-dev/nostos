# 100k fan-out cliff — root-cause diagnosis plan

Finding under test (benches/results/RESULTS.md § "Re-run with progress-based
quorum", raw: benches/results/raw/2026-09-02-ladder-rerun/linux-{50k,100k}.log):

| tier | events fanned in window | s/event | ops/s | dropped | peak RSS |
|---|---|---|---|---|---|
| 50k × 5000, 1 listener, 600 s | ~4,971 | 0.12 | 414,312 | 0 | 2,044 MiB |
| 100k × 5000, 2 listeners, 1200 s | ~982 | 1.22 | 81,901 | 0 | 3,887 MiB |

Doubling clients made each event **10× slower** (5× slower per delivery) with
nothing shed. Container: rust:1.95-bookworm on Docker Desktop linuxkit
7.0.12, 10 vCPU, 8,124,512 kB RAM, 1 GiB swap, tcp_rmem `4096 131072
33554432`, tcp_wmem `4096 16384 4194304`.

## Phase 1 — what the code says (no bench)

Read: crates/nostos-application/src/fanout.rs, crates/nostos-infra/src/{router,
transport,store}.rs, crates/nostos-bench/src/bin/probe_10k.rs.

Per-event work in `FanOutService::fan_out` is strictly **O(N)**:

1. `store.candidates_for(table)` — takes one tokio Mutex, clones N
   `SessionCandidate { predicate: Predicate{String, PredicateExpr},
   principal: Option<Principal>, sink: Arc }` → ~2 mallocs per candidate,
   200k mallocs + frees per event at 100k.
2. N × `sink.deliver()` → `try_send` on a bounded(1024) tokio mpsc per
   session. Never `Full` in either run (router `dropped=0`), so the loop never
   stalls on backpressure; it never awaits a writer.
3. `ack_progress_every=1` (probe arg 4 = 1 in both runs) → `min_acked_lsn()`
   and `slowest_session()` each scan N atomics under the same Mutex —
   **once per event, not per ack** (fanout.rs:495, :507). The probe clients
   never send acks at all (`client_task` only reads), so there is no per-ack
   path. O(N²) is ruled out.
4. `FakeReplicator::next_event` with `events_per_sec = 0` neither sleeps nor
   yields.

Linear model from the 50k figure predicts **0.24 s/event at 100k; observed
1.22 s**. A 5× residual that the fan-out loop's own instructions cannot
produce ⇒ the loop is being *starved or stalled* by something that scales
worse than N. Candidates, ranked:

- **(a) VM memory pressure** — RSS is linear (~40 KB per client pair) but the
  VM is fixed: idle VM shows 6.0 of 8.1 GiB available (≈2.1 GiB used by
  Docker Desktop + others). 100k: 3.9 GiB RSS + 2.1 = 6.0 GiB before any
  kernel socket memory (200k in-VM sockets, rmem autotunes to 32 MiB/socket)
  and slab ⇒ ≈2 GiB headroom, swap present. 50k: 2.0 + 2.1 = 4.1 GiB ⇒ ≈4 GiB
  headroom. Two sub-mechanisms, same fix: (a1) kernel TCP memory pressure
  (`tcp_mem` is host-global and not visible in the container; its pressure
  line on an 8 GiB kernel is typically ~0.5 GiB of socket pages) — under
  pressure every send is throttled and receive queues are pruned/collapsed;
  (a2) page reclaim / swap of the 3.9 GiB process. Per-session channel
  buffer: `with_buffer(1024)` × 100k is only *capacity*; frames are only
  resident when queued, and the router never reported Full, so the mpsc
  capacity does **not** explain the RSS — the RSS is sockets + tasks + WS
  buffers on both sides.
- **(e) scheduler / CPU saturation** — 200k+ tokio tasks on 10 vCPU; if the
  kernel side (loopback softirq, epoll wakeups, memory pressure work) eats
  the CPUs, the single fan-out task gets a shrinking share. Distinguishable
  from (a) by user-vs-sys CPU split with pressure counters flat.
- **(b) 2-listener split** — both listeners share one `SyncRouterState`; no
  cross-listener serialisation found in code. Low prior; kept as the
  fallback run (100k with 1 listener is impossible — 4-tuple cap ~64k — so
  the discriminating run would be 50k with 2 listeners).
- **(c) sequential loop + writer backpressure** — ruled out by `dropped=0`
  and the loop never awaiting a writer.
- **(d) frame encode / clone** — linear; cannot make a 5× residual.

## Phase 2 — smallest discriminating run

`benches/scripts/fanout-100k-diag.sh` (own container `nostos-linux-fanout-100k`,
own volumes `nostos-linux-target-fanout` / `nostos-linux-cargo-registry-fanout`,
honours `/tmp/nostos-bench.lock`). One container, two tiers, both with:

- probe `[diag] progress` line every 10 s (additive to probe_10k.rs):
  delivered, matched, events≈, live `VmRSS`, `VmSwap` — shows whether the
  rate is uniform from t=0 (structural) or collapses (pressure), and whether
  the process is being paged out.
- in-container `[sys]` sampler every 5 s: `/proc/net/sockstat` `TCP: mem`
  pages (VM-global), `TcpExt` PruneCalled / RcvPruned / TCPRcvCollapsed /
  TCPMemoryPressures(Chrono) / TCPBacklogDrop / TCPAbortOnMemory,
  MemAvailable / SwapFree / Slab, PSI memory+cpu, loadavg, cumulative
  `/proc/stat` user/sys/idle/softirq (diffed offline).

Runs: `100000 500 300 1 2` (original shape, short window — the progress
line yields the rate without completion) then `50000 500 120 1 1` (control
with identical instrumentation).

Decision table:

| signature | verdict |
|---|---|
| `TCP: mem` climbs into the 100k's; TCPMemoryPressures / PruneCalled / RcvCollapsed advance only at 100k | (a1) kernel TCP memory pressure |
| MemAvailable → ~0, SwapFree falls, PSI mem full > 0, probe `swap_mib` > 0 | (a2) VM reclaim / swap |
| all memory counters flat; sys jiffies ≫ user; PSI cpu high | (e) kernel CPU / scheduler, not memory — next run: perf/softirq |
| all flat, rate uniform from t=0, user CPU dominates | app-level; next: phase timers in fan_out, 50k with 2 listeners for (b) |

## Phase 3 — after the verdict

Fix only what the run names. Perf changes ship with before/after numbers
(RESULTS.md) or get reverted; a "VM too small" verdict is a methodology note
(RESULTS.md + BENCHMARK-METHODOLOGY.md), not a code change.
