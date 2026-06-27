# ADR-0016: Client SDK + durable checkpoint + WAL-bloat protection (deferred)

- **Status:** Deferred (Phase 1–4 — the remaining foundations)
- **Date:** 2026-06-27

## Context

Three distinct gaps are grouped here because each is a foundation that the
deferred fronts (ADR-0012–0015) build on:

1. **The client SDK doesn't exist.** There is no client crate — no apply state
   machine, no `Storage` trait implementation, no reconnect/resume logic on the
   *client* side. The server sends frames; nothing receives and applies them
   except the benchmark's WS swarm.
2. **Durable checkpoints aren't persisted.** ADR-0009 made resume *correct*
   (ack-driven slot advance), but the client's last-applied LSN lives only in
   memory — a client restart loses it (the client re-subscribes from the slot's
   confirmed LSN, which may be far behind, triggering a large replay).
3. **No WAL-bloat protection.** ADR-0009's ack-driven model means a
   permanently-silent client keeps the slot from advancing → unbounded WAL
   retention on the customer's primary Postgres. That's a correctness-preserving
   but operationally dangerous tradeoff.

## Decision

**Defer all three, with explicit configs/tests as each ships.**

**Design sketches:**
1. **Client SDK (`nostos-core` + a Rust reference client):** a `Storage` trait
   over SQLite; an apply loop that consumes `WireFrame`s, upserts by pk,
   advances a durable LSN checkpoint, and sends `Ack` frames. The FFI bridges
   (ADR-0015) bind this.
2. **Durable checkpoint:** persist the client's last-applied LSN to its local
   SQLite (one row, written transactionally with the apply). On reconnect the
   client sends it as `resume_lsn` (the transport already seeds from it —
   ADR-0009). Server-side, the slot's `confirmed_flush_lsn` is already durable
   in the `pg_replication_slots` catalog.
3. **WAL-bloat protection (server):** `max_slot_wal_keep_size` set on the slot
   at creation (Postgres 13+); plus an age/size-based forced advance or
   slow-client eviction policy when `min_acked` lags too far behind `last_seen`.
   This trades a controlled data-loss window (the slowest client) for source-DB
   safety — a deliberate, documented, configurable tradeoff.

## Rationale

- The client SDK is the largest single missing piece — but it depends on the
  server contract (ADR-0009/0010/0011) being stable first, which Tier 0/1 just
  established.
- WAL-bloat protection is the *cost* of ADR-0009's correctness: the ack-driven
  model is correct (no data loss) but operationally dangerous without a bound.
  Surfacing it here, rather than hiding it, is the honest move.

## Consequences

**Positive:** each ships as an independent, testable increment; the server
contract they depend on is now real.

**Negative:** until the client SDK exists, Nostos is server-only — the benchmark
swarm is the only "client." The strategy doc's end-to-end demo claim is
aspirational until Phase 1's client core lands.

**Kill criterion (WAL-bloat):** a deploy MUST set `max_slot_wal_keep_size` or
ship the eviction policy before production; an unbounded slot on a customer's
primary is unacceptable.

## Alternatives considered

- **Stub the client SDK now:** rejected — a client that doesn't durably apply is
  worse than no client (it would lose data on restart).
- **Preemptive slot advance (ignore acks) to avoid bloat:** rejected — that's
  the original ADR-0009 bug (silent data loss). Correctness before disk.

## References

- Depends on: ADR-0009 (resume), ADR-0010 (auth), ADR-0011 (enforcement).
- Enables: ADR-0013 (write-back needs a client), ADR-0015 (bridges bind this).
- Code: the server-side `resume_lsn` seed (`transport.rs`); the missing client
  apply loop.
