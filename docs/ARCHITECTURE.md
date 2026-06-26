# Nostos Architecture

> *Ports & Adapters (hexagonal) + DDD. The domain never knows about tokio, postgres, or axum — and that is the whole point.*

This document describes the **as-built** Week-1 architecture of the *server* half of Nostos (replicator → predicate engine → fan-out router → WebSocket transport). The multi-platform *client* SDKs (`nostos-core` + FFI bridges) ship in later weeks and are described in [`docs/decisions/`](decisions/) once built.

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

---

## 2. The layers

### 2.1 `nostos-domain` — the pure core

**Rules:** no `tokio`, no `async`, no `serde` I/O, no `#[derive(Error)]` that references infra. Pure data + invariants. If you can't `cargo test` it without spinning up a runtime, it doesn't belong here.

**Key types:**
- `Lsn` — a Postgres Log Sequence Number (newtype over `u64`). The fundamental unit of replication progress.
- `RowOp { Insert, Update, Delete }` — one row change. Payload is `Arc<[u8]>` (cheap to clone across a 1-to-N fan-out).
- `ReplicationEvent { lsn, op, txn_id? }` — an `RowOp` tagged with its source LSN.
- `Predicate { table, filter }` — the *dynamic* subscription filter (Week-1: table + simple equality; later: full boolean expressions). **This is the moat — it replaces PowerSync's static buckets.**
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
| `PgReplicator` | `ReplicatorStream` | Real PG logical replication via `tokio-postgres` + `pgoutput`. **Stubbed in Week 1.** |
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
advance watermark LSN                ← durable checkpoint (Week 2)
```

**Three properties that make this fast:**
1. **O(changed rows × matching sessions)**, never O(all sessions). The `Predicate.table` index prunes the candidate set before evaluation.
2. **Cheap clone** — `RowOp.payload` is `Arc<[u8]>`, so a 1-to-10,000 fan-out doesn't copy the payload 10,000 times.
3. **Bounded backpressure** — per-session channels with a hard cap. A stalled client is dropped with a metric increment; it can never stall the router. (PowerSync's proposal #349 admits their full-reprocessing approach doesn't have this property.)

---

## 4. What's deliberately NOT here (yet)

| Feature | Why deferred | When |
|---|---|---|
| Real `PgReplicator` (pgoutput parsing) | The Week-1 chart proves the *fan-out* moat, which is what's novel; pgoutput parsing is well-trodden (`pgwire-replication`) | Week 2 |
| Dynamic-predicate expression engine (boolean exprs) | Week 1 ships table + equality predicates only — enough to benchmark fan-out | Week 3 |
| Direct write-back (the DX moat) | Requires Postgres write-path + conflict resolution design | Week 4 |
| Client SDKs (Flutter/RN/Web) | Server must be right first | Month 2 |

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
