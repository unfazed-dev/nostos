# Nostos Architecture

> *Ports & Adapters (hexagonal) + DDD. The domain never knows about tokio, postgres, or axum — and that is the whole point.*

This document describes the **as-built** architecture of Nostos (updated 2026-08) — server, native client, and WASM bridge. The repo currently spans twelve workspace crates; the multi-platform SDK surface ships progressively under `sdk/` (Flutter first-class; web, Kotlin, Swift, .NET, RN following) per [ADR-0015](adr/0015-ffi-bridge-strategy.md) and [ADR-0016](adr/0016-client-sdk-and-wal-bloat-protection.md).

---

## 1. The dependency rule

```
                    ┌──────────────────────────────────────────────┐
                    │                                              │
                    │   nostos-server (bootstrap / composition root) │
                    │                                              │
                    └───────────────┬──────────────────────────────┘
                                    │ depends on
                    ┌───────────────▼──────────────┐
                    │   nostos-infra (adapters)      │
                    │   pg · router · ws · codec    │
                    └───────────────┬──────────────┘
                                    │ implements
                    ┌───────────────▼──────────────┐
                    │   nostos-application (ports)   │  ◄── use-cases live here
                    └───────────────┬──────────────┘
                                    │ depends on
                    ┌───────────────▼──────────────┐
                    │   nostos-domain (pure core)    │  ◄── zero I/O, zero async
                    └──────────────────────────────┘
```

The arrow of compile-time dependency **always points inward.** Domain has no deps on the upper layers. Application defines *ports* (trait interfaces) that infra implements — *dependency inversion.* This is what lets the benchmark swap a `FakeReplicator` in for the real `PgReplicator` with zero changes to domain or use-case code.

### 1.1 The nine crates (as-built)

| Crate | Role | Depends on |
|---|---|---|
| `nostos-domain` | pure types + invariants (Predicate, Lsn, events). Zero I/O, zero async | — |
| `nostos-application` | use-cases + port traits (FanOutService, SessionStore, ReplicatorStream, SyncAuth) | domain |
| `nostos-infra` | adapters: PgReplicator (feature `pg`), FakeReplicator, WS transport, wire codec, auth | application, domain |
| `nostos-server` | composition root — the axum binary | domain, application, infra |
| `nostos-core` | client apply engine + Storage trait. WASM-clean: no tokio, no SQLite | domain |
| `nostos-client` | native client: SqliteStorage (rusqlite) + tokio SyncClient | core, domain, infra |
| `nostos-ffi-wasm` | wasm-bindgen bridge over nostos-core | core |
| `nostos-bench` | throughput harness — honest numbers (drops reported, env recorded) | domain, application, infra |
| `nostos-cloud` | control plane: auth / Stripe / licensing (separate binary) | domain |

---

## 2. The layers

### 2.1 `nostos-domain` — the pure core

**Rules:** no `tokio`, no `async`, no `serde` I/O, no `#[derive(Error)]` that references infra. Pure data + invariants. If you can't `cargo test` it without spinning up a runtime, it doesn't belong here.

**Key types:**
- `Lsn` — a Postgres Log Sequence Number (newtype over `u64`). The fundamental unit of replication progress.
- `RowOp { Insert, Update, Delete }` — one row change. Payload is `Arc<[u8]>` (cheap to clone across a 1-to-N fan-out).
- `ReplicationEvent { lsn, op, txn_id? }` — an `RowOp` tagged with its source LSN.
- `Predicate { table, filter }` — the *dynamic* subscription filter. **This is the moat** — a full boolean-tree expression engine (`And|Or|Not` + typed comparison `Lt|Gt|Le|Ge` over `Number/Float/Bool/Text`, proven against real PG rows via the JSON column extractor), shipped and documented in [ADR-0012](adr/0012-dynamic-predicate-expression-engine.md). Baseline: ~150–170 eval-only events/sec through 10k predicates (~1.5M predicate-evals/sec).
- `SyncSession { id, predicate }` — one connected client's subscription.

**Why pure:** deterministically unit-testable with no runtime; survives any future async-runtime or framework swap.

### 2.2 `nostos-application` — use-cases & ports

**Rules:** may use `async_trait`, `tracing`, `serde` for port-level DTOs. No `tokio` runtime types leaked into signatures (the port returns `BoxStream` / uses an abstract sink, not `tokio::sync::mpsc::Sender`).

**Port traits (the driven-side interfaces):**
```rust
#[async_trait]
pub trait ReplicatorStream: Send + Sync {
    async fn next_event(&mut self) -> Option<ReplicationEvent>;
}

#[async_trait]
pub trait EventSink: Send + Sync {
    /// Deliver one event to one session. Backpressure strategy is the adapter's call.
    async fn deliver(&self, session_id: SessionId, event: ReplicationEvent) -> SinkResult;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>);
    async fn remove(&self, id: SessionId);
    async fn matching(&self, event: &ReplicationEvent) -> Vec<(SessionId, Arc<dyn EventSink>)>;
}
```

**Use-cases (the driving-side entry points):**
- `FanOutService` — the hot loop: `ReplicatorStream → evaluate predicates via SessionStore → deliver to each matching EventSink`. The core of the throughput moat.
- `SessionManager` — `connect(session)` / `disconnect(id)`. Called by the transport adapter when a client opens/closes a WebSocket.

### 2.3 `nostos-infra` — adapters

Each adapter implements one application port. **All `tokio`/`axum`/`postgres` code lives here and only here.**

| Adapter | Implements port | Notes |
|---|---|---|
| `PgReplicator` | `ReplicatorStream` | Real PG logical replication: `pgoutput` parsing via `pgwire-replication` + `tokio-postgres`, behind feature `pg`. LSN checkpointing, slot management, reconnect/heartbeat (ADR-0009). `FakeReplicator` is the synthetic no-PG fallback used by the benchmark. |
| `FakeReplicator` | `ReplicatorStream` | Synthetic WAL generator — drives the benchmark with no PG. Configurable rate & payload size. |
| `InMemorySessionStore` | `SessionStore` | `DashMap` keyed by `Predicate.table` for O(1) predicate lookup. The index that makes dynamic sync fast. |
| `TokioEventSink` | `EventSink` | Wraps a per-session bounded `mpsc::Sender`. **Slow clients dropped** at the buffer cap (explicit, observable — never silent OOM). |
| `WebSocketTransport` | — | axum WebSocket upgrade → spawns a `TokioEventSink` per connection + reads drain loop. |
| `WireCodec` | — | `ReplicationEvent → JSON/binary frames` on the wire. |

### 2.4 `nostos-server` — composition root

The `main()` that reads config, constructs adapters, injects them into use-cases, and binds axum. **The only place that knows which concrete adapters are wired.** `NOSTOS_REPLICATOR=fake` swaps `PgReplicator` → `FakeReplicator` with no other change.

### 2.5 `nostos-bench` — benchmark harness

Spawns N in-process WebSocket clients against a running `nostos-server`, drives a `FakeReplicator`, measures. See [`BENCHMARK-METHODOLOGY.md`](BENCHMARK-METHODOLOGY.md).

---

## 3. The hot path — the throughput moat

```
ReplicatorStream.next_event()
        │
        ▼
SessionStore.matching(&event)        ← O(1) by Predicate.table index
        │   returns Vec<(SessionId, sink)>
        ▼
for (id, sink) in matches {
    sink.deliver(id, event).await     ← bounded mpsc; slow client → Drop (observed via metric)
}
        │
        ▼
advance watermark LSN                ← durable checkpoint (ADR-0009, shipped)
```

**Three properties that make this fast:**
1. **O(changed rows × matching sessions)**, never O(all sessions). The `Predicate.table` index prunes the candidate set before evaluation.
2. **Cheap clone** — `RowOp.payload` is `Arc<[u8]>`, so a 1-to-10,000 fan-out doesn't copy the payload 10,000 times.
3. **Bounded backpressure** — per-session channels with a hard cap. A stalled client is dropped with a metric increment; it can never stall the router. (PowerSync's proposal #349 admits their full-reprocessing approach doesn't have this property.)

---

## 4. What's deliberately NOT here (yet)

| Feature | Why deferred | When |
|---|---|---|
| ~~Real `PgReplicator` (pgoutput parsing)~~ | ✅ Shipped behind feature `pg` (`pgoutput` via `pgwire-replication`). | — |
| ~~Dynamic-predicate expression engine (boolean exprs)~~ | ✅ Shipped — boolean tree + typed comparison (ADR-0012). | — |
| ~~Native client + durable checkpoint~~ | ✅ Shipped — `nostos-client` (rusqlite + tokio SyncClient) + `nostos-ffi-wasm` (ADR-0016). | — |
| Direct write-back (the DX moat) | Postgres write-path + conflict resolution; design landed (ADR-0013), v1 in progress | Phase 4 (ROADMAP) |
| Flutter / RN / Node-native FFI bridges | FRB / UniFFI / napi-rs — the hardest threading seams | ADR-0015 |

Each deferral has an ADR in [`docs/adr/`](adr/) explaining the trade-off.

---

## 5. Testing strategy

- **Domain:** pure unit tests + `proptest` property tests (e.g. "LSN arithmetic is monotonic").
- **Application:** use-cases tested with **fake adapters** (a hand-rolled `MockReplicatorStream` / `RecordingSink`) — no tokio, no network. This is the payoff of hexagonal design.
- **Infra:** each adapter tested with a real (but minimal) counterpart — `InMemorySessionStore` against a real `FanOutService`; `WebSocketTransport` against an in-process client.
- **Bench:** the end-to-end throughput harness. Not a unit test — a measurement.

---

## 6. How to add a new port / adapter

1. Define the port trait in `nostos-application`.
2. Add the domain type(s) it operates on in `nostos-domain`.
3. Implement the adapter in `nostos-infra` behind the port.
4. Wire it in `nostos-server`'s composition root.
5. Write a fake/stub for the application-layer tests.

Never let an infra type leak into a port signature. If you find yourself wanting `tokio::sync::mpsc::Sender` in a port, that's a smell — define an abstract sink instead.

---

## 7. Platform assembly (managed Cloud + web)

Beyond the sync engine, Nostos ships a managed Cloud control plane and a web
surface. These are documented in detail in the ADRs; the summary:

- **`nostos-cloud`** — the control plane (axum + rusqlite). Accounts, projects,
  API keys, Stripe billing, HMAC-signed licenses, a dual-path auth (Supabase JWT
  OR session cookie), and a `PaymentProvider` trait seam (Stripe live, PayPal
  stubbed behind a feature flag). Runs on Fly.io alongside the engine.
- **`web/`** — the SvelteKit 2 + Svelte 5 app (landing + admin), static-exported
  to Cloudflare Pages. Visual identity is "The Nostos Field" (ADR-0008): one
  nostos primitive at two scales, themeable (system + dark + light).
- **`nostos-domain::Tier`** — the portable tier taxonomy (Hobby/Pro/Scale/
  Enterprise) with concurrent-device caps. Lives in the domain ring so both the
  engine (`nostos-server`) and the control plane (`nostos-cloud`) share it without
  either sibling depending on the other.

### Decisions

- [ADR-0007: Platform assembly — Supabase + Rust + Cloudflare + Fly.io](adr/0007-platform-assembly-supabase-rust-cloudflare-fly.md)
- [ADR-0008: Visual identity — The Nostos Field](adr/0008-visual-identity-the-nostos-field.md)

### Reactive-default

Nostos is **reactive-when-connected, queue-when-offline**: the data contract is
always reactive; instant push is not the default. The push cadence lives in
`FanOutService::push_interval` (server-side, single-source) so the FFI bridges
stay dumb. Default `Duration::ZERO` (instant, what the benchmark measures); a
managed deploy sets ~1-2s to coalesce bursts. See ADR-0007 §Reactive-default.
