# 🪨 Nostos

> **The open, Rust-fast local-first sync engine.**
> *Postgres to every device, even offline. No static buckets. No write-back endpoints. Apache-2.0, end to end.*

[![CI](https://img.shields.io/badge/CI-passing-brightgreen)]() &nbsp;
![License](https://img.shields.io/badge/license-Apache--2.0-blue) &nbsp;
![Rust](https://img.shields.io/badge/rust-1.95-orange) &nbsp;
![Status](https://img.shields.io/badge/status-week--1%20spike-red)

Nostos is a from-scratch, **Rust-native** competitor to [PowerSync](https://powersync.com): a sync engine that keeps an on-device SQLite database in sync with a server-side Postgres, **even when the device is offline.** It targets the empty market cell that no incumbent occupies today — *Apache-2.0 + Postgres-logical-replication + 2-way offline + first-class Flutter/RN/Web SDKs + Rust-fast + free self-host.*

> **Status:** 🚧 Week-1 spike — proving the headline performance moat (≥5× PowerSync's 2–4k ops/sec server ceiling). Not production-ready. See [`docs/WEEK-01-PLAN.md`](docs/WEEK-01-PLAN.md).

---

## Why Nostos exists

PowerSync works — but it has three self-inflicted wounds Nostos exploits:

| Wound | PowerSync today | Nostos's answer |
|---|---|---|
| **Server bottleneck** | Server is **TypeScript/Node.js** — capped at ~2–4k ops/sec | **Pure-Rust server** (tokio + axum) — target ≥5–10× |
| **License** | Server is **FSL** (source-available, no-compete, 2-yr wait) | **Apache-2.0** end to end — clean for enterprise legal |
| **Buckets** | **1,000 buckets/user hard cap**; static-only sync rules | **Dynamic reactive sync** — live predicates, scroll forever, no ceiling |

Meanwhile **ElectricSQL abandoned 2-way offline sync**, **Zero is web-only**, and **Supabase Realtime has no offline layer**. Nostos fills the open cell.

Full strategic brief: [`docs/STRATEGY.md`](docs/STRATEGY.md).

---

## The architecture in one diagram

```
   Postgres / Supabase ──logical replication──▶ ┌────────────────────────────────────┐
                                                │        nostos-server  (Rust)         │
                                                │  replicator · predicate engine ·    │
                                                │  fan-out router · metrics           │
                                                └───────────┬─────────────────────────┘
                                                  WebSocket │  (SSE read-path option)
                                                            ▼
        ┌────────────────────────────────────────────────────────────────┐
        │                    nostos-core  (Rust crate)                     │
        │  sync state machine · LWW + CRDT-field merge · cursors (LSN)    │
        │            dynamic predicates · conflict resolution              │
        │                  ┌──────────────────────┐                       │
        │                  │   Storage trait       │                       │
        │                  └──────────────────────┘                       │
        └─────┬────────────────┬──────────────────┬──────────────────┬─────┘
              │ FRB            │ UniFFI           │ wasm-bindgen     │ napi-rs
          Flutter          iOS/Android/RN         Web/WASM          Node/Electron
        (sqlite3_         (op-sqlite on RN,      (sqlite-wasm      (better-sqlite3)
         flutter_libs)      native SQLite)        + OPFS)
```

**The repo you're looking at implements the server half + the Week-1 benchmark.** The multi-platform client SDKs (`nostos-core` + FFI bridges) ship in later weeks.

---

## Repository layout — Ports & Adapters (hexagonal) + DDD

```
nostos/
├── crates/
│   ├── nostos-domain/         # PURE: types + invariants. Zero I/O, zero async, zero framework.
│   ├── nostos-application/    # Use-cases + PORT TRAITS (interfaces). Depends only on domain.
│   ├── nostos-infra/          # ADAPTERS: pg logical replication, tokio router, ws transport.
│   ├── nostos-server/         # Composition root (binary). Wires adapters → ports.
│   └── nostos-bench/          # Week-1 throughput benchmark harness.
├── docs/                     # Architecture, ADRs, roadmap, strategy.
├── docker/                   # Postgres for the real replicator (Week 2+).
├── benches/results/          # Benchmark output (RESULTS.md + chart).
└── Makefile                  # Founder's control panel.
```

**Dependency rule (enforced by structure + clippy):**

```
   bootstrap ─► application ─► domain ◄─ infrastructure
                              (adapters implement ports)
```

The domain layer knows nothing about tokio, postgres, or axum. The application layer defines *ports* (`ReplicatorStream`, `EventSink`, `SessionStore`) — the infrastructure layer provides *adapters* that implement those ports. This is what lets the benchmark swap a `FakeReplicator` in for the real `PgReplicator` without touching a line of domain or use-case code.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design.

---

## Quick start

```bash
git clone <repo> nostos && cd nostos
cp .env.example .env

# 1. Verify the toolchain + targets (rustup picks up rust-toolchain.toml)
make setup

# 2. Run the test suite
make test

# 3. Run the Week-1 benchmark — the headline chart
make bench            # → benches/results/RESULTS.md

# 4. Or run the server standalone
make run              # → ws://localhost:8800/sync
```

> **The Week-1 benchmark needs no Postgres.** It drives a synthetic `FakeReplicator` through the *real* fan-out pipeline to isolate the server's throughput ceiling. The real `PgReplicator` arrives in Week 2 (`make pg-up`).

---

## The Week-1 deliverable

A benchmark that answers: ***"How fast can Nostos's server fan Postgres-style replication events out to thousands of concurrent WebSocket clients — and how does that compare to PowerSync's published 2–4k ops/sec ceiling?"***

The harness:
1. Spawns **N** in-process WebSocket client tasks (1k / 5k / 10k).
2. Each client subscribes with a `Predicate`.
3. A `FakeReplicator` generates synthetic `RowOp` events into the real router.
4. The router evaluates each event against live predicates and pushes to matching sessions through **bounded per-client channels with explicit backpressure** (slow clients are dropped, never silently OOM the server).
5. We measure **sustained ops/sec, drop rate, p99 client latency.**

Output: `benches/results/RESULTS.md` + a JSON artifact + an SVG chart. See [`docs/BENCHMARK-METHODOLOGY.md`](docs/BENCHMARK-METHODOLOGY.md).

---

## License

**Apache-2.0**, end to end — server, core, and every SDK. No FSL, no BSL, no "source-available" asterisk. This is a deliberate wedge against PowerSync's licensing and a procurement advantage for enterprise buyers.

---

## Contributing

Pre-1.0. The architecture and strategy are pinned; the code is a Week-1 spike. If you want to follow along, watch [`docs/ROADMAP.md`](docs/ROADMAP.md). Once v0.1 ships, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

> *A cairn is a pile of stones that marks a trail. When you're offline and lost, it's how you find your way home. **Sync checkpoints (LSNs) are our cairns** — durable markers that mean your data always finds its way back to the source of truth, across devices, through outages, around the world.*
