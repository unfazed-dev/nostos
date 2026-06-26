# ADR-0002: Rust + tokio + axum for the server

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

The server's job: consume a Postgres logical-replication stream, evaluate each row change against thousands of live authenticated predicates, and fan matching events out to thousands of concurrent WebSocket clients — with bounded backpressure and low tail latency. PowerSync's published ceiling (~2–4k ops/sec) is a Node.js-process ceiling; we need to materially beat it.

## Decision

**Language: Rust.** Runtime: **tokio.** HTTP/WS server: **axum.** PG replication: **`tokio-postgres` + `pgoutput` crate.**

## Rationale

- **Rust** gives predictable latency (no GC pauses), low memory footprint (Cloud margin advantage), and a credible ≥5–10× throughput claim vs Node.js — the core moat.
- **tokio** is the de facto async runtime; work-stealing scheduler handles 10k+ concurrent connections well; `tokio::sync::mpsc` gives us bounded channels for backpressure.
- **axum** (built on `hyper` + `tokio`) has first-class WebSocket support, middleware (tower) for tracing/CORS/metrics, and idiomatic `State` extraction — and the ecosystem momentum is here (vs. actix).
- **`tokio-postgres`** is the maintained async PG client; the `pgoutput` crate parses the logical-replication `pgoutput` stream so we don't hand-roll the binary protocol.

## Consequences

**Positive:** the moat (Rust throughput) is real and benchmarkable; low memory → cheaper Cloud; mature ecosystem for every need (replication, WS, metrics, tracing).

**Negative:** Rust's compile times and the async-stuff-is-contagious property make the dev loop slower than Node; mitigated by keeping `async` out of `nostos-domain` (ADR-0001).

**Risk:** `axum` 0.7's WebSocket API has some ergonomic rough edges around backpressure-aware sinks — we wrap it in our own `TokioEventSink` adapter rather than passing axum types through the port boundary.

## Alternatives considered

- **Go.** Fast enough and simpler, but GC pauses hurt tail latency at 10k connections; weaker ecosystem for PG logical replication; doesn't give the "Rust-fast" marketing wedge.
- **Node.js / TypeScript** (what PowerSync uses). Explicitly rejected — we'd replicate their ceiling.
- **Elixir/OTP** (what ElectricSQL uses). Excellent for many connections, but no "Rust-fast" story and a smaller pool of contributors.
