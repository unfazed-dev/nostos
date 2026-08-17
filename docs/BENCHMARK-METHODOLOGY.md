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
- Real `pgoutput` parsing (uses a synthetic `FakeReplicator`; real PG comes Week 2).
- WAN latency (in-process loopback on 127.0.0.1).
- Client-side SQLite apply (no client SDK yet).
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
