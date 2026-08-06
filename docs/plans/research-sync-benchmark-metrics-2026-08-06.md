# Research: Sync-Engine Benchmark Metrics & Fairness Methodology

Date: 2026-08-06
Scope: research only — no code. Informs design of an in-app, on-device analytics
comparison between Nostos and PowerSync.

## 1. House rules already in force (read from repo)

From `docs/BENCHMARK-METHODOLOGY.md` and `benches/results/RESULTS.md`:

- Report **drop rate alongside throughput** — a throughput number with a high drop rate
  is not honest throughput (>1% flagged).
- Record environment in every results artifact: Rust version, host, core count,
  profile/event count, buffer sizes, build flags.
- **Never mix eval-only and end-to-end numbers.** Nostos's 833,307 ops/sec / 208.3× figure
  is the *fan-out path only* (`FakeReplicator` on loopback, no client SQLite apply, no WAN
  latency, single server process) — this is stated as a caveat every time the number is
  cited. The real-PG write-amp harness (slice 6) measured a *different* path (~42 events/sec,
  test-driver-bound) and the doc explicitly says the two numbers are **not comparable**.
- Claim only what's proven: "We do not claim end-to-end superiority — only that the server
  fan-out path is materially faster."
- Quote PowerSync's published **high** ceiling (4,000 ops/sec → 208.3×), never the low
  (2,000 ops/sec → 416.7×) as the headline, per project CLAUDE.md.

> **Correction 2026-08-06:** the rule above is retired — the N× vs PowerSync framing
> compared fan-out to replication-ingest (unit mismatch); see benches/results/RESULTS.md
> §Correction. CLAUDE.md no longer states this rule.

This existing discipline (drop-rate honesty, environment recording, eval-only vs
end-to-end separation) is the right foundation for the in-app comparison and should be
extended, not replaced.

## 2. What public sync-engine benchmarks actually measure

### PowerSync — official Flutter SDK benchmarks
Source: [How Fast Is PowerSync? Performance Benchmarks For Flutter](https://powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter)

Two metrics only:
1. **Initial sync time** — wall-clock from "sync connection opens" to "all data available
   locally." Measured across 10k–10M rows, on Linux desktop (Ryzen 9 7900X), Android
   (Pixel 8a), and web (IndexedDB vs OPFS). Numbers are the **mean of 10 runs**.
   Desktop up to 27.6k rows/sec; Android ~7.1k rows/sec for 10k rows; throughput decreases
   as row count grows.
2. **Incremental sync latency** — round-trip: client writes a local record → uploaded to
   server → persisted in Postgres → processed by PowerSync → **streamed back to the same
   client**. They detect round-trip completion via a **server-populated default column**
   (e.g. a Postgres-side timestamp/default) rather than trusting client clocks — this
   sidesteps clock-skew entirely by using a single clock (the server's) as the sole time
   authority and measuring elapsed wall-clock only on the client that issued the write.
   Tested at 1, 100, 1000 updates (batched as one API call / one Postgres transaction).
   Android ~200ms for 100 updates; desktop ~40ms.

Explicit methodology choices for fairness: server stack run **locally** to minimize
client↔server network variance and server-side variability (isolates client-side
performance); iOS excluded until measured (no unverified claim); web split by storage
backend because it materially changes results (OPFS vs IndexedDB).

### PowerSync vs ElectricSQL — architecture, not head-to-head numbers
Source: [ElectricSQL electric-next vs PowerSync](https://powersync.com/blog/electricsql-electric-next-vs-powersync)

No independent apples-to-apples latency/throughput benchmark exists between the two
products. Comparisons are qualitative/architectural: Electric is **read-path only**
(writes go through your own backend API, out of scope for its benchmarks); PowerSync is
**bidirectional** (client writes flow through a persistent upload queue with its own
retry/backoff). This is a real fairness trap: comparing "sync latency" across engines
with different write paths silently compares different systems unless the write path is
held constant or explicitly excluded.

### ElectricSQL — official benchmark reference
Source: [Benchmarks – ElectricSQL docs](https://electric-sql.com/docs/reference/benchmarks)

- Explicit disclaimer up top: "Benchmarks are always highly workload, version and hardware
  dependent... not in any way guaranteed to be representative... you must test yourself
  with a representative workload on your own infrastructure." Worth adopting verbatim in
  spirit for the in-app comparison's UI copy.
- Four benchmark shapes: (1) many concurrent clients syncing a small shape — initial sync;
  (2) a single client syncing a large shape (up to 1M rows) — sync time is linear, memory
  stable; (3) write-propagation latency — "time for a write operation to reach a client
  subscribed to the relevant shape," parameterized by **number of active shapes**, with
  each shape kept independent so one write only affects one shape (isolates fan-out cost
  from cross-shape interference); (4) cloud-scale latency/resource use from 100k–1M
  concurrent clients at a fixed write rate (960 tx/min), showing latency and memory as
  flat lines — i.e., the metric reported is stability under scale, not just a point value.
- Optimized (`field = constant`) vs non-optimized (`ILIKE`) predicates produce very
  different scaling curves (flat 6ms vs. linear degradation) — the benchmark separates by
  query shape because conflating them would misrepresent typical-case latency.
- The 6ms optimized-path number is itself decomposed: 3ms Postgres write-processing + 3ms
  Electric propagation — i.e., they attribute latency to sub-phases rather than reporting
  only a lump sum.

### General findings on methodology gaps in this space
- No industry-standard benchmark suite for local-first sync engines exists yet (confirmed
  via search — only vendor self-reported numbers and qualitative comparison posts such as
  QueryPlane's ElectricSQL vs PowerSync vs Replicache write-up).
- The most-cited practitioner advice on latency measurement across two machines is to
  avoid relying on synchronized wall clocks (NTP skew is tens of ms) and instead use
  **round-trip measurement anchored to one clock** — exactly what PowerSync does with its
  server-populated column trick.

Sources:
- [How Fast Is PowerSync? Performance Benchmarks For Flutter](https://powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter)
- [ElectricSQL electric-next vs PowerSync](https://powersync.com/blog/electricsql-electric-next-vs-powersync)
- [Benchmarks – ElectricSQL docs](https://electric-sql.com/docs/reference/benchmarks)
- [ElectricSQL vs PowerSync vs Replicache – QueryPlane](https://queryplane.com/blog/electricsql-vs-powersync-vs-replicache/)
- [ElectricSQL (Legacy) vs PowerSync](https://powersync.com/blog/electricsql-vs-powersync)

## 3. Candidate metric set for an on-device Nostos-vs-PowerSync comparison app

For each metric: what it is, the instrumentation point, and why it's fair when both
engines sit behind one adapter interface.

| # | Metric | Instrumentation points | Fairness notes |
|---|---|---|---|
| 1 | **Cold initial sync time** | t0 = adapter's `connect()`/subscribe call returns; t1 = adapter reports "initial sync complete" (Nostos: watch-query first full snapshot; PowerSync: `hasSynced`/checked-status event). Client-side wall clock only — single clock, no skew. | Row count, schema shape, and network conditions must be identical inputs to both adapters in the same run (not just "similar"). Report rows/sec, not just total time, since both vendors do (throughput degrades with volume). |
| 2 | **Incremental change propagation latency (server-commit → on-device-visible)** | Server-side: Postgres transaction commit timestamp (`pg_commit_timestamp` or a trigger-populated column) as the single time authority — mirrors PowerSync's own technique. Client-side: timestamp when the watched query/collection re-emits with the new row. Elapsed = client-observed time − server commit time, both read relative to **server clock**, avoiding NTP client-clock skew. | This is the metric most exposed to write-path asymmetry (ElectricSQL is read-only; Nostos and PowerSync are bidirectional) — must document that this specifically measures **Postgres commit → client-visible**, not the write itself, so it's comparable across architectures. |
| 3 | **Local write → server-ack round trip** | t0 = client-side write call returns (optimistic apply); t1 = adapter's "server acknowledged" callback (Nostos: write confirmation frame; PowerSync: upload queue drained for that op). | Client clock only (both ends on same device) — no skew risk here. Must hold batch size constant (PowerSync benchmarks at 1/100/1000-op batches because batching changes the number materially) and disclose whether server-side processing was local or WAN. |
| 4 | **Offline queue drain time** | Simulate offline (adapter-level network kill, not OS airplane mode, for reproducibility) → queue N writes → reconnect → measure until queue empty / all acked. | Must fix N and payload size identically for both engines; drain time is sensitive to batching strategy (PowerSync batches; verify Nostos's collapsed-write behavior is compared like-for-like, not batched-vs-unbatched). |
| 5 | **Reconnect / resume time** | t0 = adapter reports "reconnecting" (or network restored); t1 = adapter reports "caught up" / watch queries stable again. | Neither vendor publishes this metric publicly (gap identified in research) — this is a genuine differentiator opportunity for Nostos, but also the highest-risk metric to get wrong: must ensure both engines are given an **identical outage duration and identical amount of missed server-side churn**, not just "reconnect after N seconds," since catch-up cost scales with backlog size, not just time. |
| 6 | **Storage bytes on device** | Read file size of the on-device DB (SQLite file for both PowerSync and Nostos) after a fixed sync scenario; separately report op-log/WAL-journal overhead if retained. | Apples-to-apples only if both use SQLite with comparable indexing; report before/after VACUUM or checkpoint since journal mode affects raw file size independent of "real" data volume. |
| 7 | **Battery/CPU proxy** | Platform-provided proxies only: OS-reported CPU time attributed to the process/isolate during a fixed sync scenario (Android `Process.getElapsedCpuTime`-style, iOS `os_signpost`/energy log, desktop `getrusage`). True battery draw needs on-device power harnesses (e.g., Android Battery Historian) that vendor self-benchmarks don't use either — flag as "proxy, not calibrated joules" in the UI. | Neither PowerSync nor ElectricSQL publish this at all — biggest methodology gap to fill carefully; a bad proxy metric that looks precise is worse than an honest qualitative note. |

Recommended top 5 to ship first (highest signal-to-effort, most directly comparable to
published vendor numbers so a skeptic can cross-check): **1, 2, 3, 4, 6**. Reconnect/resume
(5) is valuable but needs a harder fault-injection harness before it's trustworthy; battery
proxy (7) should wait until there's a real device-power harness, not a CPU-time stand-in
presented as if it were power.

## 4. Fairness risks of a shared adapter interface

1. **Abstraction-layer bias.** A single adapter interface, written by whoever builds it,
   will inevitably map more naturally onto one engine's native event model. E.g. if the
   adapter's "sync complete" signal is defined in terms of Nostos's watch-query semantics,
   PowerSync's `hasSynced` boundary may fire at a structurally different point in its
   pipeline (e.g., before vs. after local index rebuild). **Mitigation:** define each
   instrumentation point by its *externally observable contract* (row is queryable via
   the app's normal read path) rather than by internal engine state, and have someone
   from outside the Nostos team sanity-check the PowerSync-side hook placement — mirrors
   PowerSync's own choice to anchor on a server-populated column rather than internal
   client state.
2. **Clock source.** Cross-machine elapsed-time metrics (initial sync, propagation
   latency) must never diff two independently-running client clocks. Follow PowerSync's
   pattern: anchor to one authoritative clock (Postgres commit time) for propagation
   latency, and use single-device elapsed time (no cross-device diff) for write-ack and
   drain metrics. Document which clock backs each number, per metric, in the UI.
3. **Warmup / JIT / cold-cache effects.** PowerSync reports the **mean of 10 runs**, not a
   single run — first-run numbers are typically dominated by connection setup, OS file
   cache misses, and (on managed runtimes) JIT warmup, none of which reflect steady-state
   sync performance. The comparison app should discard run 1 or run a fixed warmup pass,
   and report both the mean and the run-to-run spread (Nostos's own bench already reports
   p50/p99 — reuse that convention here).
4. **Network normalization.** Both vendors minimize WAN variance by running the server
   stack locally for their headline numbers. If the comparison app runs over a real
   network, that variance will dominate small true differences between engines unless the
   network path is held identical for both engines in the same run (same Wi-Fi, same
   backend region, ideally same physical server process serving both, or a network
   emulator with a fixed profile).
5. **Write-path architecture mismatch.** Since PowerSync is bidirectional but the market
   also includes read-only engines (ElectricSQL), be explicit in the UI about which phase
   each metric covers (write-issue → server-commit vs. server-commit → client-visible) so
   the numbers can't be misread as claiming symmetric capability.
6. **Batch size and payload shape.** Both PowerSync's and ElectricSQL's own benchmarks
   show throughput/latency is highly sensitive to batch size and query-predicate shape
   (optimized vs. non-optimized). The comparison must run identical, disclosed batch
   sizes and payload shapes for both engines, not "whatever each engine defaults to."

Top 2 fairness risks for the reply to team-lead: **(1) abstraction-layer bias in where the
adapter places its instrumentation hooks**, and **(2) clock source** — both are the exact
places where an in-app comparison can produce a number that *looks* rigorous but silently
favors whichever engine the adapter was designed around first.

## 5. Existing public comparisons (for context, not to imitate uncritically)

- PowerSync's own Flutter SDK benchmark post (self-reported, methodology disclosed,
  mean-of-10, environment specified) — best model for rigor.
- ElectricSQL's benchmark reference page (self-reported, explicit "test yourself, don't
  trust vendor numbers" disclaimer, sub-phase latency attribution) — best model for
  honesty framing.
- PowerSync's "electric-next vs PowerSync" post and third-party posts (QueryPlane,
  BuildPilot) — architecture/feature comparisons, **not** measured latency/throughput
  head-to-heads. No credible independent head-to-head benchmark of PowerSync vs
  ElectricSQL exists as of this research (2026-08-06); Nostos would be filling a genuine
  gap, which raises the bar for methodology rigor since there's no existing standard to
  hide behind.
