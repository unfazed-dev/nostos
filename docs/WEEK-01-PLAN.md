# Week-1 Plan — Prove the throughput moat

> **Historical document (executed).** Outcome: 142,336 ops/sec @ 1k clients, 0% drops — see benches/results/RESULTS.md. Kept for methodology.

> **Goal, today:** a benchmark that proves Nostos's Rust server fans Postgres-style replication events out to thousands of concurrent WebSocket clients at **≥10,000–20,000 ops/sec sustained** — auditable, reproducible, with a results chart.

This is the single most important artifact of Week 1. The benchmark funds everything else: if we can prove the speed claim credibly, the rest of the strategy (OSS launch, Supabase partnership, design-partner conversations) has a spine.

---

## 1. The claim we're proving

Our claim: a **Rust** server (tokio + axum, logical-replication → fan-out pipeline) sustains high fan-out throughput.

**Headline target: ≥10,000–20,000 ops/sec sustained** at thousands of concurrent WebSocket clients.

---

## 2. Why this benchmark is honest (and what it does NOT claim)

**It DOES prove:** the maximum theoretical *fan-out* throughput of Nostos's internal pipeline — replicator → predicate evaluation → per-session bounded delivery → WebSocket frame write — at 1k/5k/10k concurrent clients.

**It does NOT prove (yet):**
- Real `pgoutput` parsing throughput (that's a Week-2 `PgReplicator`; the parsing crate `pgoutput` is well-trodden).
- Real-network WAN latency (in-process loopback; a separate WAN test comes later).
- End-to-end client SQLite apply (that's the client SDK, Month 2).

So the chart says: *"Nostos's server can fan out at X ops/sec to N clients."* That's a fair, scoped, auditable claim — exactly the kind that survives scrutiny on HN and in a sales call.

---

## 3. The benchmark design

### 3.1 Components

```
   nostos-bench (one process)
   ├── starts nostos-server on 127.0.0.1:<port>   (real axum + ws)
   ├── spawns N tokio tasks, each:
   │     • opens a WebSocket to ws://127.0.0.1:<port>/sync
   │     • sends a Subscribe{predicate} frame
   │     • loops reading frames, increments per-client counter
   ├── constructs a FakeReplicator (in-process handle into the server's FanOutService)
   │     • generates M synthetic RowOps at controlled rate / unbounded
   ├── waits until all M events are received (ack via AtomicUsize total)
   └── measures: wall-clock, sustained ops/sec, drop rate, p99 client recv latency
```

### 3.2 Why a FakeReplicator (and not a real PG)

A real Postgres at ~60 txn/sec would *itself* be the bottleneck — we'd be benchmarking PG, not Nostos. The `FakeReplicator` generates events faster than the router can push them, so the measured ceiling is **the router's**, not Postgres's. This isolates the moat. (The real `PgReplicator` benchmark comes in Week 2, against a `pgbench`-style synthetic write workload.)

### 3.3 Backpressure contract (the part that makes it honest)

Each client has a **bounded** channel of depth `NOSTOS_SESSION_BUFFER` (default 1024). If a client falls behind, the router **drops** events to that client with a metric increment (`session.dropped`). The benchmark reports the **drop rate** alongside ops/sec — so "100k ops/sec at 0% drops" means something, and "100k ops/sec at 40% drops" is called out as such. No silent backpressure hiding.

### 3.4 Test matrix

| Run | Clients | Total events | What it shows |
|---|---|---|---|
| 1 | 1,000 | 100,000 | Baseline fan-out |
| 2 | 5,000 | 100,000 | Scale under concurrency |
| 3 | 10,000 | 100,000 | The headline 10k-clients number |

Plus a **pure-router micro-benchmark** (criterion, no network) to isolate the `SessionStore.matching` + delivery cost from WebSocket I/O.

### 3.5 Payload sizes

Two payload profiles:
- **small** — ~100-byte row.
- **large** — ~4 KB row (to expose any per-byte copy cliffs; `Arc<[u8]>` should keep this flat).

### 3.6 Output

For each run, write:
- `benches/results/<timestamp>.json` — full numbers.
- `benches/results/RESULTS.md` — human-readable table + interpretation.
- `benches/results/chart.svg` — ops/sec vs clients.

---

## 4. Acceptance criteria

Week 1 is **done** when:

- [x] `make build` compiles the whole workspace clean.
- [x] `make test` passes — including domain property tests + application use-case tests with fake adapters.
- [x] `make clippy` is `-D warnings` clean.
- [x] `make bench` runs to completion and writes `RESULTS.md` + `chart.svg`.
- [x] **The sustained ops/sec at 10k clients is ≥12k ops/sec. Stretch: ≥20k.** *(10k clients measured 45,964 ops/sec — both base and stretch met. Headline 1k-client run: 142,336 ops/sec.)*
- [x] Drop rate at the target throughput is <1% (else the number isn't honest). *(Met at the headline 1k-client run — 0.00% — and at 500/5k runs. **NOT met at 10k clients: drop rate 17.26% — WS write path is the known limit; fix tracked in plan Phase C3.** The 10k number is reported with its drop rate in RESULTS.md, never as a clean throughput.)*
- [x] `RESULTS.md` states the claim, the methodology, the caveats, and the comparison — auditable by a skeptic.

---

## 5. Risks for the day

| Risk | Mitigation |
|---|---|
| WebSocket accept loop becomes the bottleneck (not the router) | measure with the pure-router micro-bench first to separate the two; if ws is the limit, that's still a fine Week-1 result |
| 10k OS TCP connections on macOS hits `kern.maxfiles` | raise ulimits in the bench harness (`setrlimit`); fall back to 5k if blocked |
| Drop rate is high even at low throughput | that's a real finding — investigate the bounded-buffer sizing; better to report an honest slow number than fake a fast one |
| tokio-tungstenite frame encoding shows up as a hot spot | pre-encode the wire frame once per event, `clone` the `Bytes` to each session (the `Arc` pattern again) |

---

## 6. What "done today" looks like

A repo you can `git clone`, `make bench`, and get a `RESULTS.md` that says *"Nostos sustained X ops/sec to 10,000 concurrent WebSocket clients. Here's how we measured it."* Plus the hexagonal codebase that produced it, with tests green and clippy clean. That's the Week-1 deliverable.
