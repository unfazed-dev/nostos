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

## Numbers (every measurement, valid or not)

Headroom rule (agreed with the coordinator 2026-09-02):

- START gate: host load1 < 8 (0.8 × 10 cores, the pre-VM baseline) and no
  other bench container up.
- MID-RUN validity: no non-harness process > 20% CPU on any 10 s sample
  (`host-cpu.log`). load1 is recorded alongside (`host-load1.log`) but a run
  is NOT invalidated on load1 alone — the 10-vCPU harness VM itself adds ~4–5
  while fanning out, so "load1 < 8 for the whole run" is unreachable by
  design. That is why a row with load1 = 12 mid-run can still be VALID.

`load1` here is the macOS host's (`sysctl vm.loadavg`), not the VM's
`/proc/loadavg` (the `[sys]` lines carry the VM's).

| run | tier | host load1 start → end | s/event | events fanned | peak RSS | verdict |
|---|---|---|---|---|---|---|
| ladder-rerun linux-100k.log (11:17, commit 1d9de36) | 100k×5000, 1200 s, 2L | not recorded (ladder env.txt: 3.41 at ladder start) | 1.22 | 982 | 3,887 MiB | the finding under test |
| ladder-rerun linux-50k.log (11:06) | 50k×5000, 600 s, 1L | not recorded | 0.12 | 4,971 | 2,044 MiB | control |
| fanout-100k-diag, first attempt (12:10) — `linux-fanout-diag.INVALID-host-load51.log` | 100k×500, 300 s, 2L | 51 → killed in connect phase | — | — | — | **INVALID** (host load 51; killed before any fan-out numbers) |
| fanout-100k-diag run 1 (22:13–22:19) — `linux-fanout-diag.log` tier 100000 | 100k×500, 300 s, 2L | 5.88 → 3.00 (gate waited 120 s) | ~0.19 | 500 (all, by t≈95 s) | 3,906 MiB | **VALID** — no cliff in 500 events; see Run 1 |
| fanout-100k-diag run 1 (22:20–22:22) — `linux-fanout-diag.log` tier 50000 | 50k×500, 120 s, 1L | 3.92 → 3.37 | ~0.066 | 500 (all, by t≈33 s) | 2,858 MiB | **VALID** control; see 50k control |
| fanout-100k-diag run 2 (2026-09-21 03:19–03:41) — `2026-09-21-fanout-100k-run2/` tier 100000 | 100k×5000, 1200 s, 2L | 7.02 → 16.91 (gate waited 60 s) | **3.60** | 331 of 5000 | 3,978 MiB | **VALID** — the discriminating run; the cliff is real |
| fanout-100k-diag run 2 (2026-09-21 03:42–03:53) — `2026-09-21-fanout-100k-run2/` tier 50000 | 50k×5000, 600 s, 1L | 7.12 → 21.96 (gate waited 60 s) | **0.158** | 3,823 of 5000 | 2,712 MiB | **VALID** control, same session/toolchain |

## Phase 3 — after the verdict

Fix only what the run names. Perf changes ship with before/after numbers
(RESULTS.md) or get reverted; a "VM too small" verdict is a methodology note
(RESULTS.md + BENCHMARK-METHODOLOGY.md), not a code change.

## Run 1 result — 100k × 500 events, VALID (host load1 5.88 → 3.00)

Log: benches/results/raw/2026-09-02-fanout-100k-diag/linux-fanout-diag.log
(tier 100000, 12:13:58–12:19:31 VM clock).

- Quorum after 31.78 s with 77,403 subscribed; the rest (22.6k) subscribed
  during fan-out (100,000 by t=130 s).
- Fan-out rate from the progress line: t=30→90 s matched went 7.78M → 41.65M
  = 565k matched/s ≈ **0.15–0.19 s/event at ~86k live sessions**. All 500
  events were fanned by t≈95 s (matched 42,708,863 = 500 × ~85.4k average
  subscribed); `matched` then froze because the FakeReplicator was exhausted,
  not because the loop slowed. The probe's `events_fanned_out~=427` divides
  by the final 100k subscribed — the true count is 500.
- Per-delivery rate 42.7M / 95 s ≈ 450k ops/s — the **same as the 50k tier's
  414k ops/s**. No cliff inside 500 events.
- VM during fan-out (`[sys]`, per 5 s across 10 vCPU = 5000 jiffies): user
  ≈2300–2450, sys ≈800–1100, softirq ≈530–870, idle ≈500–900 → ~80–90% busy,
  half of it user. After t≈95 s: idle ≈5000/5000.
- Kernel memory: sockstat `TCP: mem` peaked ~41k pages (160 MiB) during
  connect, 16–30k during fan-out, 680 after; `PruneCalled=0`,
  `RcvPruned=0`, `TCPRcvCollapsed=0`, `TCPMemoryPressures=0`,
  `TCPAbortOnMemory=0` throughout. Probe `swap_mib=0` throughout, peak RSS
  3,906 MiB.
- Router: matched=delivered=42,708,863, dropped=0, faulted=0.

Verdict for the hypothesis table: **(a1) kernel TCP memory pressure —
falsified** (counters flat, TCP mem tiny). **(a2) swap — falsified** (VmSwap
0, VM idle after the loop finished). **(e) CPU saturation** — the VM was
~85% busy but the loop still ran at the 50k rate, so it is not the cliff
either. The original 1.22 s/event therefore needs one of:

1. an effect that accumulates past ~500 events / ~100 s (channel backlog
   cannot be it — router dropped 0 — but anything indexed by delivered
   events would), or
2. host contention during the original 11:17–11:38 run — the ladder's
   env.txt only records load1=3.41 at ladder START; the host was later seen
   at load 50–80 from desktop apps, and the 100k tier ran last.

Discriminating run 2: the original shape, 100k × 5000 events, 1200 s,
ack=1, 2 listeners, with the progress line, under the headroom gate. A
uniform ~0.19 s/event to completion (≈950 s + connect) ⇒ (2): the ladder
figure is INVALID and must be re-measured; a rate that degrades with event
count ⇒ (1), and the progress line's knee says where to look.

Note: `HOST tier=100000 end … rc=143` is the inner script's own exit status
(its last command is `wait` on the killed sampler), not a probe failure —
`SOAK rc=0`. Fixed in ac672c0 (inner tier now exits 0 after sampler
teardown; outer script takes tier specs from argv).

## 50k control result — 50k × 500 events, VALID (host load1 3.92 → 3.37)

Same log, tier 50000, 22:20:01–22:22:37 host clock.

- Quorum after 34.81 s with 47,099 subscribed; 50,000 by t=80 s.
- Progress line: t=10→30 s matched 6.78M → 22.68M = 795k matched/s at
  ~47.1k live sessions ≈ **0.06 s/event**. All 500 events fanned by t≈33 s
  (matched 23,633,433 = 500 × ~47.3k average subscribed); `matched` then
  froze for the remaining ~87 s of the window — the same shape as the 100k
  tier, so the post-plateau idle is the FakeReplicator budget (500 events)
  running out, not a stall. `completed=false` in both tiers is the probe's
  denominator (clients × events) assuming every client subscribed before
  event 1.
- Per-delivery rate 23.6M / 33 s ≈ 716k ops/s vs 100k's ≈ 450k ops/s. Per
  event the 100k tier is ~2.9× slower for 2× the subscribers, i.e. ~35%
  slower per delivery — a slope, not the ladder's 10× cliff (1.22 vs 0.12
  s/event).
- Router: matched=delivered=23,633,433, dropped=0, faulted=0, swap 0, peak
  RSS 2,858 MiB.

Both tiers reached their plateau with dropped=0 and matched=delivered at
every progress sample, so the 500-event shape holds at both sizes. Run 2
(100k × 5000, 1200 s, per the paragraph above) is what separates "the
effect accumulates past ~500 events" from "the 11:17 ladder tier was taken
under host contention". Run 2 is launched with
`benches/scripts/fanout-100k-diag.sh <src> benches/results/raw/2026-09-02-fanout-100k-diag-run2 100000,5000,1200,1,2`
behind the same headroom gate.

## Run 2, attempt 1 — 100k × 5000, INVALID (host load1 5.41 → 12.6; WindowServer 41%, Brave 19.5%)

Log: `benches/results/raw/2026-09-02-fanout-100k-diag-run2/linux-fanout-diag.INVALID-host-load12-windowserver41.log`
plus `host-load1.INVALID-attempt1.log` (host load1 every 10 s). Started 22:25:54
at load1 5.41 (gate passed); load1 crossed 8 at ≈22:29 and reached 15.0 at
22:30:45; killed at t≈330 s. Non-harness processes at kill time: WindowServer
41% CPU, Brave 19.5%, two claude agents 19%/17.5% (Docker VM itself 440%).

Partial numbers, kept per the headroom rule. **Suggestive, not a verdict**:
the bend coinciding with host load crossing 8, with guest idle RISING while
throughput fell, is the single strongest pointer so far that the ladder's
100k "cliff" was scheduler starvation of the VM rather than the fan-out
loop — but this attempt was contaminated and cannot prove it.

| span | matched Δ | events (≈100k subscribed) | s/event |
|---|---|---|---|
| t=0→100 s | 34.17M | ~382 (subscribed 79.6k→89.3k) | ~0.26 |
| t=100→200 s | 18.35M | ~183 | ~0.55 |
| t=200→300 s | 16.50M | ~165 | ~0.61 |

The bend at t≈100 s coincides with host load1 rising past 8 (≈22:27:45), so
this attempt cannot separate "accumulates with event count" from "host
contention". VM `[sys]` during the slow spans shows idle rising to
1500–1800 / 5000 jiffies per 5 s while throughput fell — the guest had
spare CPU it was not being scheduled to use, which is what host
oversubscription looks like from inside a VM that does not report steal.
`PruneCalled`/`TCPMemoryPressures` stayed 0; swap 0; RSS 4,097 MiB.

Rule note for the coordinator: the 10-vCPU harness VM alone contributes
~4–5 to host load1 while fanning out, so "load1 < 8 for the whole run" is
only attainable on an otherwise idle desktop. The operative mid-run check
is the second clause — no non-harness process > 20% CPU — sampled every
10 s alongside load1. Re-armed behind the same gate.

## Run 2, attempt 2 — 100k × 5000, INVALID #2 (mid-run rule: 17/17 samples with non-harness CPU > 20%)

Log: `benches/results/raw/2026-09-02-fanout-100k-diag-run2/linux-fanout-diag.INVALID-attempt2-windowserver40-vscode61.log`,
`host-cpu.INVALID-attempt2.log`, `host-load1.log`. Started 22:34:26 at load1
4.39 (start gate passed, waited 60 s). Every 10 s sample from start to the
t≈120 s check had a non-harness process > 20% CPU: WindowServer 38–43% on
all 17, plus VS Code 61%, Google 38–40%, ProtonVPN WireGuard 28–30%, node
24%, secd/syspolicyd/ctkd 22–40%. Host load1 ≈ 11 by t=120 s. Killed at
t≈130 s per the agreed rule; no third re-arm.

Partial trace (suggestive only, contaminated): quorum 49.34 s at 82,270
subscribed (run 1: 31.8 s); t=60→100 s ≈0.29 s/event; **t=100→120 s: 8
events in 20 s = 2.5 s/event** — the ladder's 1.22 s/event regime and
worse, arriving exactly as the desktop load did.

**Status: RESOLVED 2026-09-21.** The discriminating run landed on a gated
quiet host (100k started at load1 7.02 after a 60 s wait, 50k at 7.12). Both
tiers VALID. Raw: `benches/results/raw/2026-09-21-fanout-100k-run2/`.

**Env delta, stated up front:** this run is built with **rustc 1.98**, not the
1.95 of every earlier row. The workspace moved to 1.98 in `b40bc65` and the
bench container could no longer build it. The 50k control ran in the same
session on the same toolchain, so the 50k-vs-100k contrast — which is the whole
discriminator — is internally valid; only cross-date absolute comparisons carry
the compiler change.

## Run 2 result — the cliff is real, and it is not the kernel

Steady state (from t=100 s, when all clients are subscribed, to window end):

| tier | ops/sec | s/event | per-delivery | peak RSS |
|---|---|---|---|---|
| 50k × 5000, 1L | 316,314 | 0.158 | 3.16 µs | 2,712 MiB |
| 100k × 5000, 2L | 27,859 | 3.597 | 35.89 µs | 3,978 MiB |

**Doubling the clients costs 22.7× the time per event** — 11.4× worse per
individual delivery. Linear fan-out would cost exactly 2.0×.

### What the sampler falsifies

| hypothesis | verdict | evidence |
|---|---|---|
| (a1) kernel TCP memory pressure | **FALSIFIED** | `PruneCalled`, `RcvPruned`, `TCPRcvCollapsed`, `TCPAbortOnMemory`, `TCPMemoryPressures`, `TCPMemoryPressuresChrono`, `TCPBacklogDrop`, `TCPZeroWindowDrop`, `TCPRcvQDrop` — **all flat at 0 in both tiers**. `tcp_mem_pages` peaked at 79,727 (100k) vs 26,219 (50k): it scales with sockets and never reaches a pressure threshold. |
| (b) VM memory reclaim / swap | **FALSIFIED** | `SwapFree` never moved off 1023 M in either tier; `swap_mib=0` on every probe line; `MemAvailable` bottomed at 2,667 M; `psi_mem` avg60 max 0.08. |
| (e) kernel CPU / scheduler | **FALSIFIED** | sys share is **identical at 13.1% in both tiers**. The box is *more* idle at the slow tier: idle 42.2% (100k) vs 34.5% (50k). `psi_cpu` avg60 max 2.53. A saturated kernel looks like the opposite of this. |
| (d) app-level fan-out loop cost | **the only survivor** | Everything else is flat while throughput collapses 11.4×. |

### It is steady-state, not accumulation

Bucketed into ten slices, neither tier decays: 100k oscillates 18k–33k ops/sec
across the whole 1,200 s with **RSS flat at 3,845–3,849 MiB**, and 50k
oscillates 187k–426k. There is no downward trend and no memory growth, so the
O(N×E) store-scan-that-grows-with-event-count theory does **not** fit. The cost
is a constant per-event price that is super-linear in *client count*.

### The 42% idle is the tell

The sequential fan-out shipped on 2026-09-02 (one task, one shared
`Arc<ReplicationEvent>`, walking every session in order) is single-threaded.
At 100k sessions one event takes ~3.6 s of wall clock to walk the session list,
and the other nine vCPUs have nothing to do — which is exactly the 42% idle plus
flat kernel counters we measured. At 50k it still keeps up. That is the
shape of a serialization ceiling, not a resource exhaustion.

### Host contention was NOT the explanation

This doc's leading hypothesis was that every collapse past 0.5 s/event
coincided with desktop load, and that the ladder's 1.22 s/event was therefore
UNVERIFIED. On a gated quiet host the same shape measures **3.60 s/event** —
three times *worse* than the contended ladder run. Contention was making the
numbers look better, not worse, by starving the client swarm. **That hypothesis
is falsified and the ladder's 1.22 s/event is superseded.**

### The "93.39% drop" is not loss

`router: matched=33,108,868 delivered=33,108,868 dropped=0 faulted=0` — the
router lost nothing. The undelivered 466.9 M is
`pre_subscribe_or_not_yet_fanned_out`: the 1,200 s window expired with the
replicator only ~331 events into its 5,000-event budget. Same at 50k
(`dropped=0`, 23.58% "drop" = 58.8 M not yet fanned out). The probe's `drop%`
label conflates *lost* with *not yet sent*; only `router_dropped` and
`router_faulted` mean loss, and both are zero at every tier measured to date.

### Why run 1 saw no cliff

Run 1 used a 500-event budget and reported 142,357 ops/sec at 100k vs 196,926
at 50k — a 1.38× slope. With a 5,000-event budget the same client counts give
27,543 vs 318,426, an 11.6× cliff. Between the two runs the 50k tier got
*faster* (197k → 318k, amortising the connect phase) while 100k got **5.2×
slower**. The cliff needs sustained replicator pressure to appear; a short
budget drains before the loop falls behind. Run 1's "no cliff in 500 events"
was correct and simply not a long enough run.

## Next — CORRECTED 2026-09-21 (later the same day)

**The section below this one used to say the cliff was the single-threaded
sequential fan-out loop. That attribution is wrong.** It was an inference from
what the `[sys]` sampler falsified (kernel TCP pressure, swap, CPU saturation),
not a direct reading of the loop. A direct reading now exists and it does not
support the conclusion.

`nostos-fanout-walk` (`crates/nostos-bench/src/bin/fanout_walk.rs`) runs the real
`InMemorySessionStore`, the real `FanOutService`, and real `TokioEventSink`s
with real drain tasks — with the network, the client swarm and the replicator
removed. 200 events, macOS 10-core host, release build:

| sessions | per event | per delivery | deciles |
|---|---|---|---|
| 10,000 | 4.58 ms | 0.458 µs | flat |
| 50,000 | 24.24 ms | 0.485 µs | flat |
| 100,000 | 48.58 ms | 0.486 µs | flat |

**The walk is linear.** 10× the sessions costs 10.6× the time; per-delivery cost
moves 6% across the whole range. There is no cliff in it. The other O(sessions)
per-event path, the `min_acked_lsn` scan, is linear too and trivial: 0.072 ms at
10k, 0.324 ms at 50k, 0.723 ms at 100k.

So at 100k the entire server-side per-event cost this probe can see is ~49 ms,
against the **3,597 ms/event** the gated container run measured. The fan-out
loop is **~1.4%** of the observed per-event time. Parallelising it cannot fix a
22.7× cliff.

### What this leaves

Everything the probe removed:

- the transport writer tasks (per-session wire encode + socket write),
- the kernel socket path at 100k concurrent connections,
- the 100k **in-process client tasks** — `nostos-bench-10k` runs the swarm in the
  same process and runtime as the server, so the fan-out task competes with
  ~200k other tasks.

That last one also explains the 42% idle and the "contention was flattering the
numbers" finding without any kernel-level cause: more client tasks, more
scheduler pressure, everything waits.

**The next experiment is per-stage timing inside the container run** — timestamp
`next_event` → `fan_out` return → per-sink queue depth → writer drain, sampled,
at both tiers. Guessing which of the three survivors it is would be repeating
the mistake this section corrects.

## The walk was parallelised anyway — before/after

Not because it fixes the cliff (it does not) but because 2× is 2× and the walk
is the one component now measured end to end.

`FanOutService` splits the delivery walk across
`available_parallelism()` tasks once a matched set exceeds
`PARALLEL_FANOUT_MIN` (8,192); below that it stays on the caller's task, and
`with_fanout_workers(1)` restores the old sequential walk. Every chunk task is
joined before `fan_out` returns, so a sink still sees events in LSN order —
only the visit order *within* one event changes, which was never a guarantee.

Same probe, `workers=1` vs default (10 cores), real `TokioEventSink`s:

| sessions | sequential | parallel | speedup |
|---|---|---|---|
| 10,000 | 4.58 ms/event | 2.03 ms/event | **2.26×** |
| 50,000 | 24.24 ms/event | 12.14 ms/event | **2.00×** |
| 100,000 | 48.58 ms/event | 25.47 ms/event | **1.91×** |

2× on 10 cores, not 10×: the drain tasks already occupy the runtime, so the
producer side is not what was idle. The win is real and it is bounded.

**Counter-case, recorded because it decides the threshold.** With a no-op
counting sink (79 ns/delivery) the parallel walk is *2× slower* at 100k
(7.88 → 15.38 ms/event) — chunking, spawning and joining cost more than the
work. A sink that does nothing is not a deployment shape, but it is why the
`PARALLEL_FANOUT_MIN` floor and the `workers` knob both exist.

## Phase 4 — per-stage timing (the instrument, 2026-09-21 third pass)

The retraction left a hole: the walk is 1.4% of the per-event cost, so where is
the other 98.6%? Every candidate left on the list — transport writers, the
kernel socket path, the 100k in-process client tasks — is *outside* the fan-out
loop. So stop guessing what the loop costs and measure whether the loop is even
**running**.

`Metrics` gained four diagnostic counters (`crates/nostos-application/src/ports.rs`),
written by `FanOutService` and read directly by `nostos-bench-10k`:

| counter | stage |
|---|---|
| `stage_match_nanos` | `store.candidates_for` + the predicate filter |
| `stage_deliver_nanos` | the delivery walk, including the join when it is split |
| `stage_ack_scan_nanos` | `store.min_acked_lsn` — the other O(sessions) scan |
| `stage_events` | denominator |

**Read these as wall-time-per-stage, NOT as CPU time.** `Instant::elapsed()`
spans `.await` points and `deliver_chunk` awaits on every sink, so
`stage_deliver_nanos` includes any time the fan-out task spent *descheduled
inside* the walk. The first framing of this instrument claimed `busy/wall → 1`
would prove the loop is executing rather than starved; that is **wrong** and is
retracted here. A task that is preempted mid-walk still accumulates the whole
interval into the stage it was in.

What the counters do buy, which nothing before them did:

- **Localisation.** They say which of the three O(sessions) stages the
  per-event time sits in. `match` and `ack_scan` have one await each; `deliver`
  has N. If `deliver` dominates, the cost is in the walk or in what the walk
  wakes — and `candidates_for` and `min_acked_lsn` are exonerated.
- **A comparison point for `nostos-fanout-walk`.** Same stage, same code, with
  and without the transport + client swarm. The gap between them is precisely
  the thing the probe removed.

They cannot, on their own, separate "the walk's instructions are slow" from
"the walk is descheduled between sinks". Doing that needs per-thread CPU time
(`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`) next to the wall clock — the next
instrument, not this one.

Deliberately NOT in `MetricsSnapshot`: this is a bench diagnostic, not a
`/metrics` gauge. Cost is ~6 `Instant::now()` per **event** (not per delivery),
against a per-event budget three orders of magnitude larger, and only when a
`Metrics` handle is wired.

Self-check only — **NOT a valid measurement**, native macOS, 2k clients ×
200 events (`nostos-bench-10k 2000 200 60 1 1`), host load1 ≈ 36 on 10 cores:

```
match=0.44 ms/ev  deliver=1.58 ms/ev  ack_scan=0.04 ms/ev
busy=0.41 s  wall=0.51 s  busy_frac=0.801
```

This proves the counters are wired and produce sane magnitudes. It does **not**
establish a baseline, and the headroom rule disqualifies it outright: an
unrelated 10-core `ninja` build was running, and `busy_frac` is precisely a
scheduling measurement, so competing CPU load is the one confound it cannot
tolerate. If anything a contended host *depresses* `busy_frac`, so the real
2k figure is ≥ 0.801 — which is why it is still worth recording as a floor.

The comparison that matters must come from the gated ladder: if `busy_frac`
collapses toward 0 from 10k to 100k while per-delivery cost stays flat
(`nostos-fanout-walk` says it does — 0.458 → 0.486 µs), starvation is proven and
the cliff is a scheduling problem, not a fan-out problem. All four tiers must
come from the same gated session; a 2k number from a loaded host is not a
control for a 100k number from a quiet one.

**Status: instrument landed and self-checked; the 10k/50k/100k ladder has NOT
been run.** The host is executing an unrelated Flutter-engine build
(`caffeinate ninja -j 10`, load1 > 30 on 10 cores) and
`docs/BENCHMARK-METHODOLOGY.md` forbids starting a measurement above load1 8 —
`benches/scripts/fanout-100k-diag.sh` enforces that gate itself and waits. Any
number taken now would measure the other build. Rerun with:

```bash
benches/scripts/fanout-100k-diag.sh "$PWD" benches/results/stage-diag \
  10000,500,120,1,1 50000,500,180,1,1 100000,500,300,1,2
grep -E 'stages|SOAK|HOST tier' benches/results/stage-diag/linux-fanout-diag.log
```

## Phase 4 ladder — RUN, and INVALID. Read the shape, not the numbers (2026-09-21)

All three tiers ran (`benches/results/raw/2026-09-21-stage-timing-INVALID/`).
**Every one of them fails the mid-run validity rule** and no number below may
be cited: a hung `fvm global` (429 min CPU at ~95%) and a WebKit tab (~100%)
were pinned for the whole session, plus intermittent VS Code, Chrome and a
Flutter-engine build. 238 of 238 host samples carry a non-harness process over
20% CPU — the same disqualification as "Run 2, attempt 2" above.

The self-check that proves it: this 50k tier ran at **2.43 s/event** against the
**0.158 s/event** the gated Run 2 measured for the same tier. 15× slower. That
is a measurement of the host, not of Nostos.

| tier | events in window | match ms/ev | deliver ms/ev | ack_scan ms/ev | s/event |
|---|---|---|---|---|---|
| 10k × 500, 120 s, 1L | 500 (all) | 3.53 | 217.31 | 0.34 | 0.22 |
| 50k × 500, 180 s, 1L | 74 | 231.18 | 2,099.05 | 92.86 | 2.43 |
| 100k × 500, 300 s, 2L | 37 | 1,589.93 | 4,702.93 | 1,814.36 | 8.11 |

### The shape — a hypothesis, NOT a result

Absolute times are contaminated. The *share* each stage takes of the same
event, in the same run, under the same conditions, is far more robust — and it
moves monotonically:

| tier | deliver | match | ack_scan | **match + ack_scan** |
|---|---|---|---|---|
| 10k | 98.3% | 1.6% | 0.15% | **1.8%** |
| 50k | 86.6% | 9.5% | 3.8% | **13.3%** |
| 100k | 58.0% | 19.6% | 22.4% | **42.0%** |

`match` (`candidates_for`) and `ack_scan` (`min_acked_lsn`) take the **same
per-table `tokio::Mutex`** in `InMemorySessionStore`, as does `slowest_session`.
Together they go from noise to nearly half the per-event cost.

And the magnitude is not a scan. `min_acked_lsn` at 100k costs 1,814 ms to fold
100k atomics — **18 µs per session**, against the ~1–5 ns an atomic load takes.
Over 99.9% of that is not the scan executing. It is lock acquisition, or being
descheduled while holding/waiting for it.

There is also a bias working *against* this reading, which strengthens it:
`deliver` has N await points per event while `match` and `ack_scan` have one
each, so host contention should inflate `deliver`'s share the most. Its share
fell anyway.

**Hypothesis: the shared per-table store mutex is the 100k cliff**, and the
"table-sharded router" parked in docs/ROADMAP.md is the named fix. That also
retro-explains the 42% idle — a task blocked on a mutex is not burning CPU.

**This is not proven and must not be cited.** What it earns is the right to be
the *first* hypothesis the next clean run tests, instead of a fourth guess.

### What the rerun needs

1. A quiet host. `kill` the hung `fvm global`; close the WebKit tab. The gate
   only checks load1 at start — it cannot see a single pinned core.
2. `proc_cpu_secs()` (landed in `probe_10k.rs`, not yet exercised in a
   container): `cores_used` near the vCPU count means CPU-saturated;
   far below means blocked. That single line separates lock-contention from
   scheduler-starvation, which the wall clocks cannot.
3. All four tiers in ONE gated session, so cross-tier ratios are comparable.

```bash
benches/scripts/fanout-100k-diag.sh "$PWD" benches/results/stage-diag \
  10000,500,120,1,1 50000,500,180,1,1 100000,500,300,1,2
```

## Superseded — the original "Next" (kept for the record)


Shard the fan-out loop across cores. This is the PARKED table-sharded router
(ROADMAP: parked 2026-08-24 on "full-path single-client evidence shows drain is
not scan-bound; revisit on the first multi-client real-PG run that shows the
O(N×E) scan binding"). Strictly the park condition named *real-PG*, and this is
eval-only FakeReplicator loopback — so this run is strong evidence, not the
literal trigger. It does establish that the ceiling is the single-threaded loop
rather than the scan, which changes what the fix should be: parallelise the
walk, do not micro-optimise the scan.

Measure before optimize still applies: any fix ships with a re-run of exactly
this pair of tiers, same gate, same toolchain.

---

## Phase 5 — the in-process ladder. Hypothesis falsified, server-side cost bounded (2026-09-21, fourth pass)

Phase 4's ladder was contaminated and its one surviving lead — "`candidates_for`
+ `min_acked_lsn` share a per-table mutex and their combined share climbs
1.8% → 13.3% → 42.0%" — is **falsified here**. It was an artifact of host
contention, not a property of the store.

### The instrument
`nostos-fanout-walk` gained a `sink=wire` mode. The gap that mattered: the
probe's drain task was `while rx.recv().await.is_some() {}`, a no-op, while the
real transport's write loop runs `encode_event` (serde_json) per session per
event. `TokioEventSink::deliver` only moves an `Arc` into a bounded channel —
so **the encode was invisible to every walk number ever recorded**. `sink=wire`
runs the real encode in each session's drain task. A `drained_wall` readout was
added alongside `wall`, because `wall` stops at the last `try_send`, not when
the last frame reaches the far end of its channel.

### Result — linear, with the encode included
macOS, 10 cores, 200 events, buffer 1024, default workers. 2–3 reps per cell,
spread ≤ 3%.

| sessions | `sink=tokio` µs/delivery | `sink=wire` µs/delivery | ms/event (wire) |
|---|---|---|---|
| 10,000 | 0.170, 0.175 | 0.211, 0.189 | 2.1, 1.9 |
| 50,000 | 0.174, 0.172 | 0.207, 0.219 | 10.3, 11.0 |
| 100,000 | 0.173, 0.176 | 0.223, 0.220 | 22.3, 22.0 |

Ten-fold scale-up costs **+11%** per delivery. The encode is a flat ~27% tax
at every tier, not a cliff. `drain_tax` is 1.00–1.01× everywhere: the drains
keep up with the walk exactly. Decile buckets are flat — no decay within a run.

### `min_acked_lsn` measured directly
0.072 ms @ 10k → 0.35 ms @ 50k → 0.70 ms @ 100k. Linear, ~7 ns per session.
At 100k that is **3% of a 22 ms event** — against the 1,814 ms/event Phase 4
reported. Phase 4's ack_scan figure was measuring scheduler delay, not the
fold. The `with_ack_progress_every` knob is exposed as the probe's 4th CLI arg
(`ack_interval`) and — correcting a claim made earlier in this session — it
**is** wired in `nostos-server` (`--ack-progress-interval` /
`NOSTOS_ACK_PROGRESS_INTERVAL`, `main.rs:265`). What was wrong was its default:
`1`, so every production deploy ran the per-event fold. By these isolated
numbers coalescing looked worth ~3%. Phase 6b shows that reading was wrong.

### What this bounds
At 100k sessions the entire server side — `candidates_for`, predicate
evaluation, the parallel walk, dedup, the bounded-channel enqueue, and the real
wire encode — costs **22 ms per event, or 4.5M deliveries/sec.** Everything
the 100k spine run spends above that is *not* in the fan-out loop, the store,
or the codec.

### Remaining suspects, narrowed to two
1. The loopback socket path at 100k connections (two syscalls + two kernel
   buffer copies per frame per session, both ends on one box).
2. The probe's own 100k in-process **client** tasks, which decode and apply
   every frame while sharing the server's runtime and the container's 10 vCPUs.

Suspect 2 makes the cliff partly a harness-topology artifact rather than a
server defect. Phase 6 (the gated container ladder with the `cores_used`
readout) is what separates them.

---

## Phase 6 — the gated ladder, VALID. The fan-out server is 0.08% of the cliff

Same script, same three tiers, quiet host, nothing else of mine running.
`LINUX_DIAG_EXIT=0`, every tier `rc=0`. The gate did its job: the 100k tier
waited 60 s for host load1 to fall to 4.98 before starting. Raw log archived at
`benches/results/raw/2026-09-21-stage-ladder-VALID/`.

Validity, checked rather than assumed: the 50k tier came in at 0.139 s/event
against the previously gated baseline of 0.158 s/event — the same measurement,
so this run is anchored. (The INVALID Phase 4 ladder was 15× off that anchor.)

| tier | events in window | s/event | ops/sec | `cores_used` / `nproc` | CPU µs per delivery |
|---|---|---|---|---|---|
| 10k × 500, 120 s, 1L | 500 (all) | 0.019 | 534,988 | 8.03 / 10 | 15.0 |
| 50k × 500, 180 s, 1L | 500 (all) | 0.139 | 355,124 | 8.00 / 10 | 22.2 |
| 100k × 500, 300 s, 2L | 77 | 3.896 | 24,495 | 7.37 / 10 | **287** |

### The attribution
Phase 5 measured the entire server side in isolation at **0.222 µs per
delivery** at 100k (fan-out walk + predicate eval + dedup + bounded-channel
enqueue + the real wire encode). Against the 287 µs of CPU the spine actually
burns per delivery at 100k:

| tier | server-side µs/delivery | spine CPU µs/delivery | **server share** |
|---|---|---|---|
| 10k | 0.200 | 15.0 | 1.3% |
| 50k | 0.213 | 22.2 | 1.0% |
| 100k | 0.222 | 287 | **0.08%** |

**The fan-out server is not the cliff and cannot be.** Fixing anything inside
`FanOutService` or `InMemorySessionStore` has at most 1% of the problem
available to it.

### What the stage timers actually said, and why it is consistent
The in-run stage split (match 1,014 ms/ev, deliver 2,363 ms/ev, ack_scan 499
ms/ev at 100k) contradicts Phase 5's direct measurements of the same
operations (1.6 ms, 9 ms, 0.70 ms) by three orders of magnitude. That is not a
conflict — it is the `Instant::elapsed()`-spans-`.await` flaw, retracted in
Phase 4, showing its true size. At 100k the fan-out task is descheduled for
essentially the whole interval it books. **The stage timers measure how long
the loop waits for a scheduler slot, not how long its work takes.** Read them
only as a starvation signal.

### Falsified again, with numbers
- **Memory / swap:** `SwapFree` flat at 1023 MiB every sample, every tier.
  `MemAvailable` bottoms at 2,122 MiB of 8 GiB. `psi_mem avg60 = 0.00`
  throughout — zero memory stall.
- **Kernel TCP pressure:** `PruneCalled`, `TCPRcvCollapsed`,
  `TCPMemoryPressures`, `TCPZeroWindowDrop`, `TCPBacklogDrop` — **all zero
  deltas at all three tiers.** `tcp_mem_pages` peaks at 43,008 (168 MiB) with
  200,026 sockets allocated. Nowhere near a pressure threshold.
- **CPU saturation:** `cores_used` *falls* 8.03 → 8.00 → 7.37 as throughput
  collapses, with `psi_cpu avg60` never above 1.69. The box goes **less** busy
  at the cliff. It is not out of CPU.
- **The shared store mutex (Phase 4's lead):** falsified in Phase 5 by direct
  measurement. `min_acked_lsn` is 0.70 ms/event at 100k, 3% of the event.

### Where it is
CPU per delivery goes 15.0 → 22.2 → **287 µs**: roughly flat from 10k to 50k,
then 13× between 50k and 100k. Real CPU is being burned — 2,210 s of it over a
300 s window — on something that is superlinear in session count, is not the
fan-out loop, is not blocked on memory or the kernel's socket buffers, and
leaves 2.6 cores idle while it runs.

That shape (real CPU, superlinear, idle cores, starved application task) points
at the two things Phase 5 named and this run does not separate: the loopback
socket path at 200k sockets, and the probe's own 100k in-process client tasks
competing for the same runtime.

### Phase 6b — the mutex hypothesis, falsified a second way
A fourth 100k tier runs with the probe's `ack_interval` arg at 100 instead of
1. That arg is **not** a client-side ACK cadence — it is
`FanOutService::with_ack_progress_every`, so it cuts the O(sessions)
`min_acked_lsn` fold from once per event to once per 100 events, changing
nothing else. Phase 5 predicted this was worth ~3% at 100k.

**It was worth 2.23×, and Phase 5's falsification is hereby withdrawn.**

Dose-response, 100k sessions, one script invocation, 300 s windows, gated:

| `ack_progress_every` | events | s/event | ack_scan ms/ev | ops/sec | vs 1 |
|---|---|---|---|---|---|
| 1 | 98 | 3.061 | 348.98 | 32,372 | 1.00× |
| 16 | 207 | 1.449 | 11.02 | 60,387 | **1.87×** |
| 100 | 251 | 1.196 | 1.72 | 72,051 | **2.23×** |

Monotonic in the knob, and the `ack=1` control reproduced across two runs
(77 and 98 events; 24,495 and 32,372 ops/sec), so the effect sits far outside
the ±12% run-to-run spread of this tier.

So Phase 4's hypothesis was **directionally right** — the O(sessions)
`min_acked_lsn` fold is a first-order cost at 100k — even though its stated
magnitude (1,814 ms/ev) was a starvation artifact, and its stated mechanism
(the per-table mutex) is probably wrong too: see below.

### Why isolation understated it by 60×
`nostos-fanout-walk` calls `min_acked_lsn` **once, at the end, with the walk
finished and nothing else running**: 0.70 ms of warm, uncontended fold. In the
spine it runs once per event, interleaved with the fan-out walk over the same
100k `StoredSession`s, and it touches one `acked_lsn` atomic in every one of
them. That is a 100k-line sweep that evicts the cache the walk just warmed,
every event, plus it holds the same per-table `tokio::Mutex` that
`candidates_for` needs.

**The methodological lesson, recorded because it cost this session two
reversals: an isolated microbenchmark measures a component's cost, not its
*interaction* cost. A component that is 3% alone can be 50% in situ.** Neither
Phase 5's number nor Phase 4's was wrong as measured; both were wrong as
interpreted.

Note this makes the *mechanism* cache-line contention, not lock contention —
so Phase 4's "shared per-table mutex" story lands on the right path for the
wrong reason. Unconfirmed either way; the fix does not depend on which it is.

### Shipped
`NOSTOS_ACK_PROGRESS_INTERVAL` default **1 → 16** (`nostos-server/src/main.rs`),
the knee of the curve: 1.87× of the available 2.23×, with slot lag bounded at
16 events. Guarded by a new test,
`fanout::tests::coalesced_ack_progress_lags_but_never_overshoots`, pinning both
the coalescing and the conservatism (a flushed LSN is never above the true min
at that instant). Correction to a claim made earlier in this session: the knob
was already wired into `nostos-server`; only its default was wrong.

What Phase 5 *does* still establish, unaffected: the fan-out walk plus the
real wire encode is linear to 100k at 0.222 µs/delivery. The per-event ack
fold is a separate path in `run()`, not part of that walk.
