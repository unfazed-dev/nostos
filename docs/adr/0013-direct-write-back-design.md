# ADR-0013: Direct write-back (Front 2 — deferred)

- **Status:** Deferred (Phase 4 — design sketch)
- **Date:** 2026-06-27

## Context

Front 2 ("Direct Write-Back — no endpoints") removes PowerSync's biggest DX tax:
the client queues mutations and *you* implement + host `uploadData()`. Nostos
would apply queued client mutations to Postgres directly. There is **no
write-back code anywhere** in the current repo — no mutation queue, no write-rule
engine, no apply path. This is the single largest missing feature after the
client SDK.

## Decision

**Defer to Phase 4.** The read-path (replication → fan-out → client) is the
foundation; write-back layers on top of it and on top of conflict resolution
(ADR-0014). Building it before the read-path is correct (ADR-0009) and the
client has a durable apply (ADR-0016) would compound risk on an unproven base.

**Design sketch (Phase 4):**
1. **Direct mode (default):** declarative write rules per table — `columns`
   (allowed set), `auth_scope` (the tenant column, enforced like ADR-0011),
   `merge: upsert | insert_only`, an `etag`/`version` column for optimistic
   concurrency.
2. The client queues mutations; the server applies each to Postgres **inside a
   transaction** that re-checks the version/etag and applies the merge strategy.
   Conflict → ADR-0014's resolution tier.
3. **Function mode:** for full control, the developer provides a function (like
   PowerSync's `uploadData`). Power users keep total control.

## Rationale

- Write-back depends on: a correct LSN/resume model (so the client knows what it
  applied), a conflict strategy (so concurrent writes reconcile), and an
  authenticated principal (so writes are scoped). All three are Tier 0/1
  foundations; this ADR waits for them.
- Postgres remains the single source of truth — write-back *writes* to it; the
  read-path then fans the resulting WAL change back to all clients. The loop
  closes through the existing pipeline.

## Consequences

**Positive:** when it ships, Nostos can honestly say "point us at your Postgres;
we handle offline reads AND writes" — the demo that wins.

**Negative:** until Phase 4, Nostos is read-only from the client's perspective;
clients must host their own write endpoint (exactly PowerSync's tax). The
strategy doc must not market write-back as shipped until this ADR is implemented.

## Alternatives considered

- **Ship a write endpoint stub:** rejected — a stub that accepts writes but
  doesn't transactionally check versions is a data-corruption footgun, not a
  feature.

## References

- STRATEGY §6.2 (the write-back moat in depth).
- Depends on: ADR-0009 (resume), ADR-0010 (auth), ADR-0014 (conflict).

## Addendum (2026-07): v1 ships over the sync WebSocket

v1 scope shipped ahead of Phase 4 (plan: docs/plans/complete-nostos-fully-wired-operational.md):
- Transport: `ClientMessage::Write` on the existing authenticated /sync socket —
  zero new deps, one auth path, ordered with ACKs. The HTTP POST path described
  above remains the Phase-4 design for gateway/enterprise deployments.
- Rules: per-table allowlist (`NOSTOS_WRITE_TABLES`), pk upsert/delete only,
  server-authoritative LWW by WAL order (ADR-0004/0014 tier (a)).
- Conflict checks (version/etag), declarative write rules, and function mode
  remain Phase 4. Echo suppression is unnecessary: client apply is an
  idempotent upsert (nostos-core Storage contract), so the write's replication
  echo is a no-op.
