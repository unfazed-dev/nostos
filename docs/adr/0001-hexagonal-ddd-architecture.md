# ADR-0001: Hexagonal (Ports & Adapters) + DDD layering

- **Status:** Accepted
- **Date:** 2026-06-26
- **Decision owner:** Founder

## Context

Nostos is a sync engine with many swappable moving parts: the source of replication events (real Postgres vs. a synthetic generator for benchmarking), the per-session delivery mechanism (tokio channel vs. in-memory recording for tests), the transport (WebSocket today, WebTransport tomorrow), and the client storage (rusqlite / sqlite-wasm / op-sqlite / sqlite3_flutter_libs).

We need to:
1. Benchmark the router with a **fake replicator** without touching real Postgres.
2. Unit-test use-cases with **no async runtime and no network**.
3. Swap the transport (WS → WebTransport) without rewriting business logic.
4. Eventually ship **four FFI bridges** (Flutter/RN/Web/Node) over one core, none of which can leak platform types into the business logic.

## Decision

Adopt **Ports & Adapters (hexagonal) + DDD layering** with four crates:

| Crate | Ring | Rule |
|---|---|---|
| `nostos-domain` | enterprise / business | **Pure.** No `tokio`, no `async`, no `serde` I/O. Types + invariants only. |
| `nostos-application` | application / use-cases | Defines **port traits** (driven-side interfaces) + use-cases. May use `async_trait`, `tracing`. |
| `nostos-infra` | infrastructure / adapters | Implements ports. **The only place `tokio`/`axum`/`postgres` may appear.** |
| `nostos-server` | bootstrap / composition root | Wires concrete adapters into ports. The only place that knows the wiring. |

**Dependency direction always points inward:** `bootstrap → infra → application → domain`. Infra implements application ports (dependency inversion). Domain never depends on anything above it.

**Enforcement:** by crate structure (the compiler enforces visibility) + workspace clippy lints. A PR that adds `tokio` to `nostos-domain` will not compile.

## Consequences

**Positive:**
- The benchmark swaps `FakeReplicator` for `PgReplicator` with zero changes to domain or use-case code.
- Use-cases are unit-testable with hand-rolled fakes — no tokio, no network, deterministic.
- Transport/storage/runtime swaps are localized to one adapter each.
- The future FFI bridges get a clean, platform-agnostic core to bind against.

**Negative:**
- More crates → slightly more boilerplate (re-exports, port trait definitions).
- Async trait objects (`Arc<dyn EventSink>`) have a small dynamic-dispatch cost in the hot path — mitigated by keeping the trait coarse-grained (one `deliver` call per event, not per byte).

**Mitigation on the hot path:** the `SessionStore::matching` returns concrete `(SessionId, Arc<dyn EventSink>)` tuples; the dynamic dispatch is one vtable call per session per event — negligible against the WebSocket write cost.

## Alternatives considered

- **Single crate, module-based layering.** Rejected: the compiler can't enforce the dependency rule across modules; `tokio` would creep into the domain within a week.
- **Clean Architecture (Use-case → Entity → Interface, 5+ layers).** Rejected: too many layers for a sync engine's hot path; hexagonal's three rings are the right granularity.
- **Actor framework (Actix).** Rejected: couples the domain to a specific async paradigm; we want runtime-agnostic core.
