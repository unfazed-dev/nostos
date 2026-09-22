# Measuring conflation honestly — why ADR-0045's gate is the wrong instrument

**Date:** 2026-09-22. **Status:** all five harness defects built (2026-09-22);
the convergence-lag instrument landed as a router test (`47c178c`); the gate
replacement below is still an open operator decision.
**Prompted by:** ADR-0045 attempt 3 shipping a correct metric fix and still failing to
demonstrate a benefit, with the blame placed on the harness's single host.

## The finding, in one line

The drop-rate gate cannot score conflation **no matter how good the harness gets**, because
drop rate counts frames and conflation's benefit is not a frame-count benefit. The missing
instrument is **Age of Information**, and it does not need a second host.

## Age of Information (AoI)

The formal metric for staleness at a receiver: `Δ(t) = t − U(t)`, where `U(t)` is the largest
generation time among packets that have reached the destination by `t`. Age rises linearly
with time and **resets on arrival of a packet with a larger generation time**.

That reset rule is the entire argument. A conflating queue is a device for making the reset
happen sooner. In age terms:

> Dropping 500 superseded ticks costs nothing. Dropping one final tick costs everything.

Which is exactly what ADR-0045 asserts about Nostos's row plane — every frame is a complete row
image, so only the newest one matters — and exactly what `drop_rate` is structurally unable to
express. Attempt 3 fixed `drop_rate` to stop *penalising* conflation. It still cannot *credit*
it, because there is no frame count that can.

Two results from the AoI literature worth knowing before any further design:

- **Last-generated-first-served (LGFS) and push-out queue disciplines are the formal analogues
  of conflation, and have known AoI-optimality results.** ADR-0045's move-to-tail overflow is a
  push-out discipline. The design has theory behind it; what it lacks is an instrument.
- **The penalty function matters more than raw age.** `g(Δ)` encodes the use case: a threshold
  indicator `1(Δ > d)` for "did any client ever hold stale state longer than d", a stair
  function for periodically-checked data (a UI that repaints on a frame boundary — i.e. Nostos's
  actual consumer), exponential for control loops. Nostos wants the threshold form.

## The metric to build, and why it is cheap

`ReplicationEvent` carries no timestamp (`crates/nostos-domain/src/events.rs:128` — `lsn`, `op`,
`txn_id`). It does not need one. The benchmark owns both ends, so age is measurable **without
touching the domain type or the wire format**:

1. **Convergence lag (discrete AoI), end of run.** For each key, compare the highest LSN the
   replicator emitted against the highest LSN the client holds. Conflation should hold this at
   **0 for every key** while shedding arbitrarily many intermediate frames. No clocks, no wire
   change, and it is the question ADR-0045 actually raises: *after the flood, does every client
   hold the latest value for every row?*
2. **Peak age, if a time-domain number is wanted later.** A shared `lsn → Instant` emit table in
   the bench process; client records receive time per frame; report peak and mean age per key.
   Prefer **peak** over average — it bounds worst-case staleness instead of averaging it away.

Item 1 alone settles ADR-0045. Item 2 is optional and strictly larger.

## The gate ADR-0045 should have had

Current (binding, unmeetable here):

> Ship only if 50k stays sub-1% and at least one rung above 50k moves from over-1% to under-1%.

That demands the drop-rate ladder, which demands 50k+ clients, which demands the second host.
It also asks a frame-count metric to score a non-frame-count change.

Proposed replacement, measurable at 1k on one host:

> Under induced backlog (small buffer, recycled keys), conflation must hold **convergence lag at
> 0 for every key** where the baseline sheds, at no throughput cost on the unbacklogged path.

The backlog is already inducible and already demonstrated: `--buffer 16 --distinct-keys 64` at
1k clients produced 380,637 real supersedes. That run is the fixture.

## Separately: five harness defects

Four came from the research below. The fifth came from running on a second
machine, and could not have come from anywhere else — see "What a slow host
found" at the end. These are real and independent of ADR-0045.

**All five are built as of 2026-09-22.** Two were found by a slow host and two
by reading the standard load-testing literature; none were found by the figures
themselves, which is the point of the closing section.

1. ~~**"Pedal to the metal" is not a benchmark.**~~ **BUILT 2026-09-22 (`0d9a7c1`).**
   `FakeReplicator` flooded as fast as the consumer would take it, so the drop rate measured
   where the system falls over, not whether it meets a rate. `--rate` now holds a **constant
   arrival rate, open-loop**: event `i` is due at `start + i/R` regardless of what the router did
   with `i-1`.

   The pacing that already existed was worse than none: `sleep(1/R)` *after* each event, so a
   consumer taking `d` per event quietly dropped the real rate to `1/(1/R + d)` — textbook
   coordinated omission, the generator slowing to whatever the system could absorb and then
   reporting no problem. A run whose generator now misses its own schedule is stamped
   `rate not held` and excluded from every figure, because it offered less load than it claimed.
   The default stays `0` (flood), so every historical figure keeps its meaning. With a rate set,
   the <1% bar reads "at rate R, drops < 1%" and the ladder becomes a search for the largest R
   that holds.
2. ~~**A run that hit its timeout must not report a number.**~~ **BUILT 2026-09-22 (`1449664`).**
   The 10k rung's `elapsed_secs: 120.00` is the `--timeout-secs` default, i.e. window expiry, and
   the JSON looked like a measurement. `RunResult` now carries `throughput_valid`; an invalid run
   is withheld from the printed table and from **every aggregate** in RESULTS.md, and all tiers
   timing out prints an explicit refusal rather than a `max` over an empty set (which returns
   `0.0` and reads as a measured collapse). Latency is still reported — a truncated window does
   not bias the frames that did land. Predicted here for the 10k rung; found corrupting the
   **1k** rung on a 4-core host.
3. ~~**Repetition policy instead of ad-hoc run counts.**~~ **BUILT 2026-09-22 (`0d9a7c1`).**
   `--reps` (default 5) fixes N *before* the run, MLPerf-style: fastest and slowest dropped, mean
   of the rest, with the min-max **spread** printed beside it as part of the figure rather than a
   footnote to it. `--warmup-reps` (default 1) runs and discards a warm-up per tier. RESULTS.md
   now tables one row per **tier**, not per run, and the headline is a max over tier means —
   never over raw repetitions, which reports the luckiest run of the session. An ad-hoc run count
   is an invitation to choose N after seeing the numbers; a fixed policy removes the choice.
4. ~~**Randomise arm order within a session.**~~ **BUILT 2026-09-22 (`0d9a7c1`).** Attempt 3's
   A/B ran a fixed `A,B,A,B,A,B`, so the parent always took the cold-cache slot — and the bench's
   own `1k,5k,10k` had the same shape, handing the first tier every cold cache and every
   unsettled thermal state, run after run. That bias is systematic, not noise: it never averages
   out, because it lands on the same tier every time. The `(tier, rep)` schedule is now shuffled
   with a seeded Fisher-Yates (`--order-seed`, recorded in the report), so the order is
   randomised and still reproducible. Random interleaving is reported to cut run-to-run variance
   by up to 40%. Still open from this item: **plot the series** — throttling shows as a step
   change that any single summary statistic hides.
5. ~~**The wait loop could not finish a lossy run.**~~ **BUILT 2026-09-22 (`59af8b7`).** It waited
   for `sum_received() >= events × clients` — the count a *loss-free* run receives. The router is
   allowed to shed on a full session channel and a shed event never reaches a client, so a single
   shed made the target unreachable and the loop spun to the deadline. Runs now end on
   **quiescence** (delivery stops advancing), with the clock stopped at the last delivery so the
   quiet grace never enters `elapsed`.

## What a slow host found (2026-09-22)

A second machine became available: `unfazed-rog`, Intel i7-7700HQ, 4c/8t, 2017 mobile part,
Arch. Slower than the Mac in every dimension. It found both bugs above within four runs, and
**neither is reproducible on Apple Silicon**, because both require the router to shed and the Mac
sheds nothing at the 1k tier. `target` is always met there, so `elapsed` is always honest.

Defect 5 is the instructive one. The same workload, same binary, same host:

| window | delivered | elapsed | reported |
|---|---|---|---|
| 120 s | 98,987,756 | 120.0 s | 824,882 ops/sec |
| 600 s | 99,427,257 | 600.0 s | **165,712 ops/sec** |

Five times the window, 0.4% more work, a 5× "slower" result. Both figures were work ÷ an
arbitrary window. A reader comparing them would have concluded the system had collapsed.

The methodological point, which generalises past this harness: **a benchmark validated on one
machine is validated against that machine's failure modes.** Every figure in RESULTS.md was
produced on hardware fast enough to hide two defects that make throughput unfalsifiable. The fix
is not a faster host, it is a *different* one — and preferably a worse one, since a slow machine
enters the regimes a fast one never reaches. Results live in
`benches/results/linux-unfazed-rog-2026-09-22.md`.

This does not retire the second-host requirement in the next section. That is about the generator
competing with the server for CPU, which is unchanged: this box runs the same in-process harness,
and its LAN is Wi-Fi, so it cannot host a network-separated arm either.

## What still genuinely needs a second host

CPU pinning and core isolation (`isolcpus`, cgroup v2 cpusets, disabling boost/HT) are the
one-host mitigation for a co-located generator — and **they are Linux-only**. macOS exposes no
supported CPU-affinity API on Apple Silicon, so none of it is available on this box. The
`docker/` path could host it; bare macOS cannot.

So the honest split is:

- **Conflation benefit** → measurable here today, via convergence lag. No second host.
- **Absolute throughput at 50k–100k** → still needs the generator off the server's host.
  Unchanged: *do not cite the 100k tier as a Nostos server limit.*

## Sources

- Age of Information: "Minimizing the Age of Information through Queues"
  (arxiv.org/pdf/1709.04956); "Sampling for Data Freshness Optimization: Non-linear Age
  Functions" (arxiv.org/pdf/1812.07241); "Overage and Staleness Metrics for Status Update
  Systems" (arxiv.org/pdf/2109.14062).
- Coordinated omission / open-loop load: Gil Tene, "How Not to Measure Latency";
  github.com/giltene/wrk2; ScyllaDB, "On Coordinated Omission".
- Variance and interleaving: "The Right Call for Software Benchmarking" (arxiv.org/html/2606.17261);
  Google Benchmark random interleaving; MLPerf Training Benchmark (arxiv.org/pdf/1910.01500).
- One-host isolation: pyperf "Tune the system for benchmarks"; cgroup v2 cpuset shielding
  (testbit.eu/2023/cgroup-cpuset); the-benchmarker/web-frameworks PR #9662 (server-saturation
  validity check).
