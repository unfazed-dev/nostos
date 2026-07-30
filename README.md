# 🪨 Nostos

> **The open, Rust-fast local-first sync engine.**
> *Postgres to every device, even offline. No write-back endpoints. Rust-fast. Apache-2.0, end to end.*

[![CI](https://img.shields.io/badge/CI-passing-brightgreen)]() &nbsp;
![License](https://img.shields.io/badge/license-Apache--2.0-blue) &nbsp;
![Rust](https://img.shields.io/badge/rust-1.95-orange) &nbsp;
![Status](https://img.shields.io/badge/status-alpha%20%E2%80%94%20Phase%203%2C%20v0.1%20prepared%2C%20launch%20gated-orange)

Nostos is a from-scratch, **Rust-native** competitor to [PowerSync](https://powersync.com): a sync engine that keeps an on-device SQLite database in sync with a server-side Postgres, **even when the device is offline.** It targets the empty market cell that no incumbent occupies today — *Apache-2.0 + Postgres-logical-replication + 2-way offline + first-class Flutter/RN/Web SDKs + Rust-fast + free self-host.*

> **Status:** alpha — Phase 3 🚧, v0.1 prepared, launch gated on the operator (see [`docs/ROADMAP.md`](docs/ROADMAP.md)). Not production-ready. The server fan-out moat is proven (833,307 ops/sec @ 1k clients, 0.00% drops = 208× PowerSync's published **high** ceiling of 4k ops/sec, 417× its 2k low — see [`benches/results/RESULTS.md`](benches/results/RESULTS.md)), the real Postgres replicator, native client, and write-back v1 are shipped. Public launch is now gated on the Flutter+Supabase plug-and-play bar — see [`docs/plans/flutter-supabase-plug-and-play-launch.md`](docs/plans/flutter-supabase-plug-and-play-launch.md).

---

## Why Nostos exists

PowerSync is the incumbent — and still carries real, current limits Nostos exploits. The defensible wedges (audited July 2026):

| Wedge | The incumbent's limit | Nostos's answer |
|---|---|---|
| **Server throughput** | PowerSync's server is **TypeScript/Node.js** — published ceiling ~2–4k ops/sec | **Pure-Rust server** (tokio + axum) — proven 833,307 ops/sec @ 1k clients, 0.00% drops (208× their 4k high, 417× their 2k low) |
| **License** | PowerSync's server is **FSL** (source-available, no-compete, 2-yr wait to Apache) | **Apache-2.0 today** — server, core, and every SDK. Clean for enterprise legal |
| **Write-back** | You build & host the `uploadData()` endpoint; ElectricSQL is read-only | **Direct write-back** — Nostos writes to your Postgres for you, no customer-built endpoints |
| **Self-host** | PowerSync Cloud is metered per-op; FSL "Open Edition" carries the license delay | **Free, full-featured, unlimited self-host** — no feature gates |

Meanwhile **ElectricSQL abandoned 2-way offline sync (read-path only)**, **Zero is web-only**, **Zero disabled offline writes**, and **Supabase Realtime has no offline layer**. Nostos fills the open cell. (PowerSync shipped dynamic **Sync Streams** to GA in May 2026, so the old "static buckets only" framing no longer holds — see the honest comparison in [`docs/COMPARISON.md`](docs/COMPARISON.md).)

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

**The repo you're looking at implements the server, the native client, the WASM bridge, and the benchmark harness.**

---

## Repository layout — Ports & Adapters (hexagonal) + DDD

| Crate | Role | Depends on |
|---|---|---|
| `nostos-domain` | pure types + invariants (Predicate, Lsn, events). Zero I/O, zero async | — |
| `nostos-application` | use-cases + port traits (FanOutService, SessionStore, ReplicatorStream, SyncAuth) | domain |
| `nostos-infra` | adapters: PgReplicator (feature `pg`), FakeReplicator, WS transport, wire codec, auth | application, domain |
| `nostos-server` | composition root — the axum binary | all above |
| `nostos-core` | client apply engine + Storage trait. WASM-clean: no tokio, no SQLite | domain |
| `nostos-client` | native client: SqliteStorage (rusqlite) + tokio SyncClient | core, domain, infra |
| `nostos-ffi-wasm` | wasm-bindgen bridge over nostos-core | core |
| `nostos-bench` | throughput harness — honest numbers (drops reported, env recorded) | domain, application, infra |
| `nostos-cloud` | control plane: auth / Stripe / licensing (separate binary) | domain |

```
nostos/
├── crates/
│   ├── nostos-domain/         # PURE: types + invariants. Zero I/O, zero async, zero framework.
│   ├── nostos-application/    # Use-cases + PORT TRAITS (interfaces). Depends only on domain.
│   ├── nostos-infra/          # ADAPTERS: pg logical replication, tokio router, ws transport.
│   ├── nostos-server/         # Composition root (binary). Wires adapters → ports.
│   ├── nostos-core/           # Client apply engine + Storage trait (WASM-clean).
│   ├── nostos-client/         # Native client: rusqlite Storage + tokio SyncClient.
│   ├── nostos-ffi-wasm/       # wasm-bindgen bridge over nostos-core (web/Worker).
│   ├── nostos-bench/          # Throughput benchmark harness.
│   └── nostos-cloud/          # Control plane: auth / Stripe / licensing (separate binary).
├── docs/                     # Architecture, ADRs, roadmap, strategy.
├── docker/                   # Postgres for the real replicator.
├── benches/results/          # Benchmark output (RESULTS.md + chart).
└── Makefile                  # Founder's control panel.
```

**Dependency rule (enforced by structure + clippy):**

```
   bootstrap ─► application ─► domain ◄─ infrastructure
                              (adapters implement ports)
```

The domain layer knows nothing about tokio, postgres, or axum. The application layer defines *ports* (`ReplicatorStream`, `EventSink`, `SessionStore`) — the infrastructure layer provides *adapters* that implement those ports. This is what lets the benchmark swap a `FakeReplicator` in for the real `PgReplicator` without touching a line of domain or use-case code.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design, and
**[`docs/api/`](docs/api/README.md) for the API reference** — one page per SDK, every signature
extracted from source and cited to the file it came from.

---

## Quick start

```bash
git clone <repo> nostos && cd nostos
cp .env.example .env

# 1. Verify the toolchain + targets (rustup picks up rust-toolchain.toml)
make setup

# 2. Run the test suite
make test

# 3. Run the Week-1 benchmark — the headline chart (no Postgres needed)
make bench            # → benches/results/RESULTS.md
```

There are **three demo paths** — pick the one that matches what you want to see.

### A. Zero-setup demo (no Docker) — native client + reconnect/resume

```bash
cargo run -p nostos-client --example reactive_scroll
```

This spins an **in-process** axum sync server, a durable SQLite client, and a
mid-run server restart that proves the client reconnects and **resumes from its
durable checkpoint** (no loss, no duplication). It uses a `FakeReplicator` plus
synthetic events shaped like real `tasks` rows — so it exercises the *real*
client apply engine and storage layer without needing Postgres. Exits 0 when the
demo completes; look for `resumed from durable checkpoint` in the output.

### B. Real-Postgres dev stack — the actual `PgReplicator`

```bash
make dev-stack
```

This is the **real** path: `docker compose up` brings up Postgres 16 with
`wal_level=logical` (host port `5433`, db/user/pass `nostos`, publication
`cairn_pub` + `tasks` table from `docker/pg-init`), the target waits for the
publication to exist, then runs `nostos-server` with
`NOSTOS_REPLICATOR=pg NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn`.
Look for the `replicator: PgReplicator (real Postgres logical replication)` log
line. From another terminal you can insert a row and watch it flow:

```bash
docker compose -f docker/docker-compose.yml exec postgres \
  psql -U cairn -d cairn -c \
  "INSERT INTO tasks (org_id, title) VALUES ('00000000-0000-0000-0000-000000000001', 'hello nostos');"
```

Then connect your own client to `ws://localhost:8800/sync` (or `psql` directly)
to watch events stream. Ctrl-C stops the server; tear down Postgres with
`make pg-down`.

### C. Web demo — the WASM client + `/demo` page

```bash
make web-demo
```

Runs **alongside** `make dev-stack` (run dev-stack first, in another terminal):
`wasm-pack build`s the `nostos-ffi-wasm` bridge, installs web deps, and starts
the Vite dev server on http://localhost:5173/. Open the `/demo` page — it
connects cross-origin to the server's WS (`ws://localhost:8800/sync`), so no
Vite WS proxy is wired. Ctrl-C stops the dev server.

> **The first two paths are independent.** `reactive_scroll` brings its *own*
> in-process server and does **not** connect to the `dev-stack` server — pick
> one or the other, not both. `dev-stack` is the only path that exercises real
> Postgres logical replication; `reactive_scroll` is the fastest way to see the
> native client + reconnect/resume in action.

> **The Week-1 benchmark needs no Postgres.** `make bench` drives a synthetic
> `FakeReplicator` through the *real* fan-out pipeline to isolate the server's
> throughput ceiling.

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

## Managed deploys — beta waitlist

Self-hosting is free forever (see License). If you'd rather never operate the
sync server yourself, a managed `nostos deploy` beta is coming: we run your
Nostos instance, tier-stamped and metered, connected to your own Postgres or
Supabase database. Open a [GitHub discussion or issue](https://github.com/unfazed-dev/nostos/issues)
titled `waitlist` to get in line for the design-partner beta.

<!-- NOSTOS-IDENTITY-PENDING: contact mailbox undecided (docs/IDENTITY.md). This
     asked readers to email founders@nostos.run — an unregistered domain, so every
     waitlist mail would have bounced into nowhere. -->

---

## Security

See [`SECURITY.md`](SECURITY.md) for vulnerability reporting, and [`docs/SECURITY-MODEL.md`](docs/SECURITY-MODEL.md) for why Nostos's server-enforced predicates — not Postgres RLS — are the authorization layer for sync traffic.

---

## Contributing

Pre-1.0. The architecture and strategy are pinned; the code is alpha (Phase 3 🚧 — v0.1 prepared, launch gated on the operator). If you want to follow along, watch [`docs/ROADMAP.md`](docs/ROADMAP.md). Once v0.1 ships, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

> *A cairn is a pile of stones that marks a trail. When you're offline and lost, it's how you find your way home. **Sync checkpoints (LSNs) are our cairns** — durable markers that mean your data always finds its way back to the source of truth, across devices, through outages, around the world.*
