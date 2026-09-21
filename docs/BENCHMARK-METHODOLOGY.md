# Benchmark Methodology

> *How we measure Nostos's throughput, what the numbers mean, and what they don't. Written so a skeptic can reproduce it and a CTO can trust it.*

---

## 1. The claim

**Nostos's Rust server sustains high-throughput aggregate fan-out — Postgres-style replication events delivered to thousands of concurrent WebSocket clients — with zero drops.** The measured figure and its scope live in [benches/results/RESULTS.md](../benches/results/RESULTS.md).

**No PowerSync ratio is claimed.** PowerSync publishes no comparable aggregate fan-out figure anywhere in its docs, blog, or benchmark repos. Its published rates ([docs](https://docs.powersync.com/resources/performance-and-limits), verified 2026-08-06) belong to different pipeline stages:

| Metric | PowerSync rate | Pipeline stage |
|---|---|---|
| Replication ingest (small rows) | ~2,000–4,000 ops/sec | Postgres → PowerSync Service — a different stage from fan-out |
| Replication ingest (large rows) | ~5 MB/sec | same ingest stage, and a different *unit* — never set against ops/sec |
| Small-transaction rate | ~60 txn/sec | ingest-side transaction rate |
| Per-client sync | ~2,000–20,000 ops/sec | Service → *one* client — not an aggregate across clients |

> **Retired framing (Correction 2026-08-06):** an earlier revision of this section claimed a "≥5×" ratio by dividing Nostos's aggregate fan-out figure by PowerSync's replication-ingest rate — two different stages of two different pipelines under one "ops/sec" label. That framing is retired; the full record lives in RESULTS.md's "Correction (2026-08-06)". Same-stage, same-units comparisons only, ever. Nostos's server is Rust where PowerSync's is Node.js, but an architecture difference is not a benchmark — only a measured same-stage comparison would be, and none exists today.

---

## 2. What we measure (and don't)

### In scope
- Replication event ingestion → predicate evaluation → per-session delivery → WebSocket frame write.
- Sustained throughput over a fixed event count.
- Drop rate (events the server chose not to deliver because a client fell behind its bounded buffer).
- p99 per-client receive latency.

### Out of scope (deliberately, for Week 1)
- Real `pgoutput` parsing (the headline uses a synthetic `FakeReplicator`). **Now measured separately (2026-08-17):** the real-PG ingest leg — see RESULTS.md "Real-Postgres ingest leg"; still excluded from the headline itself.
- WAN latency (in-process loopback on 127.0.0.1).
- Client-side SQLite apply. **Now measured separately (2026-08-17):** the client-apply leg — see RESULTS.md "Client-apply leg"; still excluded from the headline itself.
- Cross-machine distribution (single server process).

These scope limits are **stated in every results artifact.** The claim is specifically about the *server's fan-out ceiling*, not end-to-end application latency.

---

## 3. Workload

The workload models a Postgres logical-replication stream of row changes to a single table (`tasks`). Each event is one of:

```rust
enum RowOp {
    Insert { table, pk, payload: Arc<[u8]> },
    Update { table, pk, payload: Arc<[u8]> },
    Delete { table, pk },
}
```

- **Distribution:** 80% Insert, 15% Update, 5% Delete (typical append-heavy app).
- **Payload profiles:**
  - `small` — 100-byte payload (the PowerSync "small row" regime).
  - `large` — 4 KB payload (exposes per-byte copy cliffs).
- **Predicate fan-in:** each event matches a configurable fraction of connected clients. Default: **all clients match** (worst-case fan-out — every event goes to every session). This is the hardest case for the router.

---

## 4. Harness

Single process (`nostos-bench`):

1. Starts a real `nostos-server` (axum + WebSocket) on `127.0.0.1:<ephemeral>`.
2. Spawns `N` tokio tasks. Each:
   - opens a WebSocket,
   - sends a `Subscribe { predicate }` frame,
   - enters a read loop, incrementing a per-client `AtomicU64` and pushing receive timestamps into a latency histogram.
3. Obtains an in-process handle to the server's `FanOutService` and constructs a `FakeReplicator` that emits `M` synthetic events as fast as the router will accept them (the router's backpressure is the rate limiter).
   - **`NOSTOS_FAKE_EPS` / `NOSTOS_FAKE_KEYS` do not apply here.** Those bound the `nostos-server` *binary's* dev default (A10, ADR-0027); `nostos-bench` builds its own `FakeReplicatorConfig` (`crates/nostos-bench/src/main.rs:226`), leaving both knobs at `0` = unpaced, monotonic keys. The measured ceiling is unaffected by them — and must stay that way, since pacing would cap the very number this document defines.
4. Waits until the sum of per-client counters ≥ `M` (with a timeout).
5. Computes: sustained ops/sec = `M / wall_clock`. Drop rate = `1 - (delivered / M)`. p99 latency from the histogram.

Run for `N ∈ {1000, 5000, 10000}` and both payload profiles.

---

## 5. Backpressure contract

Each client session has a **bounded** delivery channel of depth `B` (`NOSTOS_SESSION_BUFFER`, default 1024). The router's `deliver()` is **non-blocking with drop semantics**: if a client's channel is full, the event for that client is dropped and a `session.dropped` counter increments.

**Why drop-and-observe, not block:** a single stalled WebSocket must never stall the replication fan-out (head-of-line blocking). PowerSync's full-reprocessing model (their proposal #349) doesn't have this guarantee.

**Consequence for honesty:** the benchmark reports drop rate alongside throughput. A throughput number with a high drop rate is meaningless and is called out as such. The headline number is the **highest throughput at <1% drop rate.**

**Enforced (2026-09-21).** `benches/scripts/fanout-100k-diag.sh` stamps every
tier with `drop_pct=.. throughput_valid=yes|no`. Before that this rule was
honoured in prose only: both 2026-09-21 ack-coalescing runs produced twelve
tiers dropping 7.9–95.7%, and their ops/sec were compared as throughput anyway.

---

## 6. System & environment

Recorded in every results artifact:
- CPU model, core count, frequency.
- RAM.
- OS + version.
- Rust toolchain (`rustc --version`).
- `ulimit -n` (file descriptors — must be ≥ 2 × max clients).
- Cargo profile (`release` with `lto = "fat"`, `codegen-units = 1`).

**Reproducibility:** `make bench` from a clean clone reproduces the numbers (modulo hardware). The benchmark binary writes a JSON artifact with every input + output + environment field.

**Linux-container runs (added 2026-09-02).** macOS caps a single host at ~9.2k loopback sockets (`ENOBUFS`, mbuf-cluster exhaustion), so the 10k soak runs in Docker via `benches/scripts/linux-soak.sh` (`rust:1.95-bookworm`, `--ulimit nofile=1048576`, `ip_local_port_range=1024 65535`). A Docker Desktop VM is a *different environment*: its numbers are reported only against other runs in the same container (baseline vs candidate, back-to-back), never against a macOS-native figure, and every container log starts with an `env:` line (kernel, arch, nproc, nofile, port range, mem, rustc) that the results section must quote. Host load on the Mac running the VM is recorded alongside, because it leaks into the VM.

---

### 6.1 The headroom rule — when a measurement counts

Two halves. The second is the one that matters.

- **START gate.** A *build* may run under any host load. A *measurement* only
  starts when host load1 < 8 (0.8 × 10 cores, `sysctl vm.loadavg` — the macOS
  host's, not the container's `/proc/loadavg`), no other bench container is up,
  and `/tmp/nostos-bench.lock` is free.
- **MID-RUN validity.** Sampling every 10 s for the whole tier: **aggregate
  non-harness CPU ≤ 150%**, on at least **95% of samples**. The Docker Desktop
  VM doing the fan-out is *harness* and is expected above 500%.

Mechanically enforced by `benches/scripts/fanout-100k-diag.sh`. Every tier ends
with `MIDRUN samples=.. viol=.. viol_pct=.. other_mean=.. other_max=..
load1_max=.. valid=yes|no`; a tier with `valid=no` is fit for order-of-magnitude
bounds only, never for an A/B. Logs are kept either way (the `CONTENDED.md`
pattern), never silently re-rolled.

**load1 does not invalidate a run on its own.** The 10-vCPU harness VM alone
contributes ~4–5 to host load1 while fanning out, so "load1 < 8 for the whole
run" is unreachable by design — a tier that ends at load1 = 12 can be perfectly
valid. load1 is recorded for context; the aggregate-CPU clause decides.

#### What "150%" means and where it comes from (recalibrated 2026-09-21)

macOS `top` reports **100% = one core**, so this host has 1000% to give.

- The VM's observed *peak* demand during a 100k fan-out is ~740% (`cores_used`
  7.37/10 at the cliff, `docs/plans/fanout-100k-cliff-diagnosis.md`).
- Headroom above that peak is therefore ~260%.
- The bar is set at 150%, comfortably inside the headroom: at 150% of non-harness
  load the VM can still draw 850% > the 740% it has ever wanted. Above it, other
  work is cutting into CPU the harness has been measured to use.

**Aggregate, not per-process.** Ten processes at 15% starve the VM exactly as
much as one at 150%, and a per-process rule sees only the second.

`other_mean` is recorded per tier so analysis can regress throughput on a
continuous contention covariate. End-of-run load1 is a poor substitute: it is
partly *caused* by throughput (Spearman ρ ≈ 0.6 over six tiers — suggestive at
the extremes, not an identification).

#### The rule this replaces, and why it was wrong

The rule agreed 2026-09-02 read: *no non-harness process above 20% CPU on any
10 s sample*. It was unenforceable and mismeasured.

- **20% is 2% of this machine.** One fifth of one core out of ten, while the
  harness itself legitimately runs at 574%. No Mac with a display attached ever
  satisfies it — WindowServer alone idles at 30%.
- **Per-process misses the actual failure mode**, as above.
- **"Any sample" is not proportional.** One 10 s blip in a 300 s tier is 3% of
  the run; discarding the tier for it optimises the wrong thing.

The recalibration was made while trying to get a run to pass, which is exactly
when a threshold is most likely to be bent, so it is pinned by a regression test
in the script's `--self-test`: the 2026-09-02 attempt-2 sample (WindowServer 40 +
VS Code 61 + Google 39 + ProtonVPN 29 + node 24 + secd 30 ≈ 223%) — the one run
with a human INVALID verdict — must still come out `valid=no`, and an idling
desktop (~75–81% aggregate) must still come out valid. A future change to
`OTHER_LIMIT` that lets the 2026-09-02 run through fails the test.

#### Why this is written down here

It was not. The rule lived in `docs/plans/fanout-100k-cliff-diagnosis.md` while
the harness cited *this* file for it, and the mid-run half went unenforced. On
2026-09-21 an ack-coalescing A/B passed the start gate and then ran tiers at
load1 34.21 and 22.64; the resulting 2.23× was noise and was retracted
(RESULTS.md). A rule that is written down and not enforced is worse than no
rule — it makes contaminated runs look gated.

---

## 7. Pure-router micro-benchmark

In addition to the end-to-end WebSocket harness, a `criterion` micro-benchmark measures **just** `SessionStore.matching` + `EventSink.deliver` with no network I/O — an in-memory `RecordingSink`. This isolates the router's own ceiling from WebSocket frame encoding. Reported alongside the end-to-end number so a skeptic can see where time goes.

---

## 8. How the comparison to PowerSync is framed

The `RESULTS.md` always states, verbatim:

> *PowerSync publishes a server-side ceiling of ~2,000–4,000 ops/sec for small rows. Nostos's measurement is of the same logical operation — fanning row-change events to connected clients. The comparison is scoped: Nostos's number is from a synthetic replicator on loopback; PowerSync's is from their docs. The ratio is the point, not the absolute.*

We do not claim end-to-end superiority — only that the **server fan-out path** is materially faster, which is the moat.

---

## 9. Failure modes we'll report honestly

- If the WebSocket accept loop is the bottleneck (not the router), we say so and report the router-only number.
- If we hit OS connection limits before the router saturates, we say so and report the highest achievable client count.
- If the drop rate is >1% at the target throughput, we report the throughput at <1% drops instead, and flag the gap.
- If we **don't** beat PowerSync ≥3×, we say so — that's a signal to pivot the architecture, not a number to spin.
