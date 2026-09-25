# ADR-0013: Direct write-back (Front 2 — deferred)

- **Status:** **Accepted (shipped)** — corrected 2026-07-30. This read "Deferred (Phase 4 —
  design sketch)" long after it shipped, which was the most misleading line in the repo: the
  landing page and `docs/STRATEGY.md` both sell direct write-back as *the* differentiator against
  sync engines that require the developer to implement and host their own upload endpoint, so a
  reader who checked here was told the headline feature did not exist. As built: `PgWriteBack` in `nostos-infra`, gated by `NOSTOS_WRITE_TABLES`
  (`nostos-server/src/main.rs:112`, empty by default), covered by `e2e_pg_writeback.rs` +
  `e2e_pg_writeback_timestamp.rs` against real Postgres, and exercised as the ECHO half of all 9
  `sdk-e2e` slices. Still deferred: nothing in this ADR's core path.
- **Date:** 2026-06-27

## Context

Front 2 ("Direct Write-Back — no endpoints") removes the biggest DX tax common
to bucket-based sync engines: the client queues mutations and *you* implement
+ host your own upload endpoint. Nostos
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
3. **Function mode:** for full control, the developer provides a function (the
   common escape-hatch pattern for custom upload hooks). Power users keep
   total control.

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
clients must host their own write endpoint (exactly the DX tax Front 2 removes). The
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

### Typed parameter binding (deviation from plan, ratified 2026-07)

The plan specified text-cast binding for v1 ("bind everything as text and let
PG coerce, `ponytail:` typed binding when a schema registry exists"). This is
**incorrect for typed columns** and was deviated from: the adapter infers the
Rust type from the JSON value shape and binds via a `SqlValue` enum
(`Uuid`/`Bool`/`I64`/`F64`/`Jsonb`/`Text`/`Null`). The hard-won finding:

> Postgres does **not** implicitly coerce a `text`-bound parameter to `uuid`
> for `INSERT`/`UPDATE`. `PREPARE t(text) AS INSERT INTO tasks (id) VALUES ($1)`
> fails with `column "id" is of type uuid but parameter is of type text`.
> Binding by inferred type is **required**, not an optimization.

Rationale: (1) the schema-registry ponytail is not load-bearing — JSON value
shape + a `uuid::Uuid::parse_str` attempt covers every column type nostos v1
writes, with a `Text` fallback; (2) parameterization (the actual safety
property) is unchanged — every value still binds via `$1…$n`, never
interpolated. The upgrade path is binding by *column* type (read from
`pg_attribute`/`pg_constraint`) rather than by *value* shape, which becomes
necessary only when a column type has no JSON-shape signal (e.g. `timestamptz`
stored as a numeric epoch). Code doc: `crates/nostos-infra/src/write_back.rs`
(`SqlValue` enum + `json_value_to_sql`).

## Addendum (2026-07): v2 — outbox dead-letter policy

The v1 outbox contract above let a permanently-failing write block the queue
head forever (flagged `ponytail:` in `nostos-client/src/client.rs`). v2 bounds
it (parity workstream P2):

- `nostos_outbox` gains `attempts` + `dlq` columns (legacy DBs migrated on open
  by probing `PRAGMA table_info`). `pending()` filters `WHERE dlq = 0`.
- The flush loop bumps `attempts` on every `WriteResult{ok:false}`; at
  `dead_letter_max_attempts` (default 50, `SyncClientConfig`) the write is
  **quarantined** via `Outbox::mark_dead_letter` — removed from the pending
  queue but **NOT deleted**, so the head advances past a permanent rejection
  without losing the write. `dead_letter_entries()` inspects the DLQ.
- The `Outbox` trait adds `bump_attempts`/`mark_dead_letter` with no-op
  defaults so the `InMemoryStorage` test double stays non-breaking.

Decision: quarantine-not-delete. The contract is that the backend
returns 2xx for validation conflicts and the queue must not silently drop;
deleting would lose user intent irrecoverably. Replay/inspection is the
operator surface; auto-retry-from-DLQ is deferred.

### On-device SQL read surface (read-side, same client)

`SqliteStorage::query(sql)` (parity P1) runs arbitrary `SELECT` against
`nostos_data` — the dev projects the opaque payload via `json_extract` (JSON1
ships in the bundled SQLite). It is deliberately on the **concrete
`SqliteStorage`**, NOT the `Storage` trait: the trait stays WASM-clean
(`checkpoint` + `apply_batch` only), so `nostos-ffi-wasm` is unaffected. This
is the foundation for the Flutter `watchQuery(sql)` reactive query (parity
feature #1).
