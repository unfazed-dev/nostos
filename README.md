# 🪨 Nostos

> **The open, Rust-fast local-first sync engine.**
> *Postgres to every device, even offline. No write-back endpoints. Rust-fast. Apache-2.0, end to end.*

[![CI](https://img.shields.io/badge/CI-passing-brightgreen)]() &nbsp;
![License](https://img.shields.io/badge/license-Apache--2.0-blue) &nbsp;
![Rust](https://img.shields.io/badge/rust-1.98-orange) &nbsp;
![Status](https://img.shields.io/badge/status-alpha%20%E2%80%94%20Phase%203%2C%20v0.2.0%20tagged%2C%20launch%20gated-orange)

Nostos is a from-scratch, **Rust-native** sync engine that keeps an on-device SQLite database in sync with a server-side Postgres, **even when the device is offline.** It targets the empty market cell that no incumbent occupies today — *Apache-2.0 + Postgres-logical-replication + 2-way offline + first-class Flutter/RN/Web SDKs + Rust-fast + free self-host.*

> **Status:** alpha — Phase 3 🚧, v0.2.0 tagged, public launch gated on the operator (see [`docs/ROADMAP.md`](docs/ROADMAP.md)). Not production-ready. The server fan-out moat is proven (2,618,601 ops/sec aggregate fan-out @ 1k clients, 0.00% drops — median of 3 passes, 2026-09-02, eval-only: FakeReplicator on loopback — see [`benches/results/RESULTS.md`](benches/results/RESULTS.md)), the real Postgres replicator, native client, write-back with tenant enforcement, and the Flutter / Swift / Kotlin / RN / .NET / Tauri / Capacitor / Node / web SDKs under [`sdk/`](sdk/) are shipped. Public launch is now gated on the Flutter+Supabase plug-and-play bar — see [`docs/plans/flutter-supabase-plug-and-play-launch.md`](docs/plans/flutter-supabase-plug-and-play-launch.md).

---

## Why Nostos exists

Nostos's defensible wedges (audited July 2026):

| Wedge | Nostos's answer |
|---|---|
| **Server throughput** | **Pure-Rust server** (tokio + axum) — 2,618,601 ops/sec aggregate fan-out @ 1k clients, 0.00% drops (median of 3, 2026-09-02; eval-only: FakeReplicator on loopback) |
| **License** | **Apache-2.0 today** — server, core, and every SDK. Clean for enterprise legal |
| **Write-back** | **Direct write-back** — Nostos writes to your Postgres for you, no customer-built endpoints |
| **Self-host** | **Free, full-featured, unlimited self-host** — no feature gates |

**Sync rules:** an operator-facing `nostos_rules.toml` declares what each client can read — `all` (zero-config dev default), `toggles` (per-table on/off + scope), or `hand` (raw predicate grammar) — with a checksum-gated resync so a rules edit is never silently missed by a connected client. See [ADR-0031](docs/adr/0031-sync-rules-modes-and-checksum-resync.md).

Meanwhile **ElectricSQL abandoned 2-way offline sync (read-path only)**, **Zero is web-only**, **Zero disabled offline writes**, and **Supabase Realtime has no offline layer**. Nostos fills the open cell. See the honest comparison in [`docs/COMPARISON.md`](docs/COMPARISON.md).

**Migrating from Realm?** See the guide in [`docs/migrations/`](docs/migrations/): [`from-realm.md`](docs/migrations/from-realm.md).

Full strategic brief: [`docs/STRATEGY.md`](docs/STRATEGY.md).

---

## The architecture in one diagram

```
   Postgres / Supabase ──logical replication──▶ ┌────────────────────────────────────┐
                                                │        nostos-server  (Rust)         │
                                                │  replicator · predicate engine ·    │
                                                │  fan-out router · write-back        │
                                                └───────────┬─────────────────────────┘
                                                  WebSocket │  (or iroh QUIC, ADR-0041)
                                                            ▼
        ┌────────────────────────────────────────────────────────────────┐
        │   nostos-core (apply engine · LSN checkpoint · outbox ·        │
        │   Storage trait)  ◄── nostos-client (rusqlite + tokio)         │
        └─────┬────────────────┬──────────────────┬──────────────────┬───┘
              │ FRB            │ UniFFI           │ wasm-bindgen     │ napi-rs
          Flutter        Swift / Kotlin /      Web / WASM         Node / Electron
                          RN / .NET         (nostos-ffi-wasm,
                                             sqlite-wasm + OPFS)
```

Every native SDK wraps `nostos-client`'s `SyncClient<SqliteStorage>`; the web
SDK wraps `nostos-ffi-wasm`. The Tauri plugin rides the native client; the
Capacitor plugin runs the web SDK in its webview.

**This repo holds the server, the native client, the WASM bridge, the SDKs, the push daemon, the `nostos` CLI, the Cloud control plane, and the benchmark harness.**

---

## Repository layout — Ports & Adapters (hexagonal) + DDD

| Crate | Role | May depend on |
|---|---|---|
| `nostos-domain` | pure types + invariants (Predicate, Lsn, events). Zero I/O, zero async | — |
| `nostos-application` | use-cases + port traits (FanOutService, SessionStore, ReplicatorStream, SyncAuth) | domain |
| `nostos-infra` | adapters: PgReplicator (feature `pg`), FakeReplicator, WS transport, wire codec, auth, write-back, push senders | application, domain |
| `nostos-server` | composition root — the axum binary | domain, application, infra, license |
| `nostos-core` | client apply engine + Storage trait. WASM-clean: no tokio, no SQLite | domain |
| `nostos-client` | native client: SqliteStorage (rusqlite) + tokio SyncClient | core, domain, infra |
| `nostos-ffi-wasm` | wasm-bindgen bridge over nostos-core | core, domain |
| `nostos-bench` | throughput harness — honest numbers (drops reported, env recorded) | domain, application, infra |
| `nostos-license` | HMAC-signed offline license claims | domain |
| `nostos-push` | standalone push daemon `nostos-pushd` (ADR-0038) | domain, infra |
| `nostos-cli` | the `nostos` CLI — init, dev, doctor, deploy, rules, push | domain, infra |
| `nostos-cloud` | control plane: accounts, API keys, Stripe billing, license minting (separate binary) | domain, infra, license |

```
nostos/
├── crates/                   # The twelve Rust crates above.
├── sdk/                      # Flutter, Swift, Kotlin, React Native, .NET, Tauri, Capacitor, Node, web.
├── web/                      # SvelteKit landing + admin (static export).
├── apps/atlet/               # Atlet — benchmark-first app exercising every SDK against Supabase.
├── supabase/                 # schema.sql + the `cairn-push` Edge Function (held name).
├── docs/                     # Architecture, ADRs, roadmap, strategy, API reference.
├── docker/                   # Postgres for the real replicator.
├── deploy/ · packaging/      # Deploy guide; Homebrew + release packaging.
├── scripts/                  # CI checks (scripts/check.sh) and helpers.
├── benches/results/          # Benchmark output (RESULTS.md + chart).
└── Makefile                  # Founder's control panel (`make help`).
```

**Dependency rule (enforced by structure + clippy):**

```
   composition roots ─► infrastructure ─► application ─► domain
                        (adapters implement the application's ports)
```

The domain layer knows nothing about tokio, postgres, or axum. The application layer defines *ports* (`ReplicatorStream`, `EventSink`, `SessionStore`) — the infrastructure layer provides *adapters* that implement those ports. This is what lets the benchmark swap a `FakeReplicator` in for the real `PgReplicator` without touching a line of domain or use-case code.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design, and
**[`docs/api/`](docs/api/README.md) for the API reference** — one page per SDK, every signature
extracted from source and cited to the file it came from.

New to all of this? [`docs/nostos-explained.html`](docs/nostos-explained.html) is a
self-contained, click-it-yourself explanation of direct mode — open it in a browser.

---

## Quick start

```bash
git clone <repo> nostos && cd nostos
cp .env.example .env

# 1. Verify the toolchain + targets (rustup picks up rust-toolchain.toml)
make setup

# 2. Run the test suite
make test

# 3. Run the fan-out benchmark — the headline chart (no Postgres needed)
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
`wal_level=logical` (host port `5433`, db/user/pass `cairn` — pre-rename names, held; publication
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

> **The fan-out benchmark needs no Postgres.** `make bench` drives a synthetic
> `FakeReplicator` through the *real* fan-out pipeline to isolate the server's
> throughput ceiling.

---

## The fan-out benchmark

A benchmark that answers: ***"How fast can Nostos's server fan Postgres-style replication events out to thousands of concurrent WebSocket clients?"*** (See [`benches/results/RESULTS.md`](benches/results/RESULTS.md).)

The harness:
1. Spawns **N** in-process WebSocket client tasks (1k / 5k / 10k).
2. Each client subscribes with a `Predicate`.
3. A `FakeReplicator` generates synthetic `RowOp` events into the real router.
4. The router evaluates each event against live predicates and pushes to matching sessions through **bounded per-client channels with explicit backpressure** (a slow client's events are conflated, then shed and counted — never a silent OOM).
5. We measure **sustained ops/sec, drop rate, p99 client latency.**

Output: `benches/results/RESULTS.md` + a JSON artifact + an SVG chart. See [`docs/BENCHMARK-METHODOLOGY.md`](docs/BENCHMARK-METHODOLOGY.md).

---

## License

**Apache-2.0**, end to end — server, core, and every SDK. No FSL, no BSL, no "source-available" asterisk. This is a deliberate wedge and a procurement advantage for enterprise buyers.

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

See [`SECURITY.md`](SECURITY.md) for vulnerability reporting and the security model: why Nostos's server-enforced predicates — not Postgres RLS — are the authorization layer for sync traffic.

---

## Contributing

Pre-1.0. The architecture and strategy are pinned; the code is alpha (Phase 3 🚧 — v0.2.0 tagged, launch gated on the operator). If you want to follow along, watch [`docs/ROADMAP.md`](docs/ROADMAP.md); to contribute, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

> *Nostos (formerly Cairn) is the Greek word for the homecoming. **Sync checkpoints (LSNs) are how your data gets home** — durable markers that mean it always finds its way back to the source of truth, across devices, through outages, around the world.*
