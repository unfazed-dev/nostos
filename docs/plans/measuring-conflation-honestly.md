# Measuring conflation honestly — why ADR-0045's gate is the wrong instrument

**Date:** 2026-09-22. **Status:** research complete, not implemented.
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

## Separately: four harness defects the same research names

These are real and independent of ADR-0045.

1. **"Pedal to the metal" is not a benchmark.** `FakeReplicator` floods as fast as it can, so the
   drop rate measures where the system falls over, not whether it meets a rate. The standard fix
   is a **constant arrival rate** (`wrk2 --rate`, open-loop). The <1% bar should read "at target
   rate R, drops < 1%", and the ladder should search for the largest R that holds. This reframes
   the entire drop-rate ladder and is probably the highest-value harness change in this document
   after AoI.
2. **A run that hit its timeout must not report a number.** The 10k rung's `elapsed_secs: 120.00`
   is the `--timeout-secs` default, i.e. window expiry. `nostos-bench` should stamp
   `throughput_valid: false` and refuse the figure, the way `fanout-100k-diag.sh` already does
   per tier. Today the JSON looks like a measurement.
3. **Repetition policy instead of ad-hoc run counts.** MLPerf's template: fix N per benchmark,
   drop fastest and slowest, report the mean of the rest, and state the tolerance the N was
   chosen to hold (5 runs → 90% within 5%). Turns variance from an argument into a number.
4. **Randomise arm order within a session.** Attempt 3's A/B ran a fixed `A,B,A,B,A,B`, so the
   parent always took the cold-cache slot. Random interleaving is reported to cut run-to-run
   variance by up to 40%; fixed order reintroduces an ordering bias. Also discard the warm-up
   run, and plot the series — throttling shows as a step change that a median hides.

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
