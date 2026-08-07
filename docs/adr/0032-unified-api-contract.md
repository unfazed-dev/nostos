# ADR-0032: Unified API contract (typed reads, structured predicates, outbox batching, dead-letter observability)

**Status:** Accepted — Wave 1 (Flutter) implemented 2026-08-08 · **Date:** 2026-08-07

Ratified contract: [`docs/plans/nostos-unified-api-contract.md`](../plans/nostos-unified-api-contract.md).
Implementation plan: [`docs/plans/nostos-unified-api-implementation.md`](../plans/nostos-unified-api-implementation.md).

## Context

The pilot app (`apps/atlet/flutter`) hand-wrote SQL for every read even though
a typed `Collection<T>` already existed, because `docs/api/flutter.md` presented
SQL first and `Collection.watch(where:)` took a **raw SQL fragment** for
`where`/`orderBy` — an injection foot-gun with parameter binding an unshipped
P1. Eight of nine SDKs had no typed layer at all. The gap is *contract*, not
rot: every Nostos read in atlet is "table, maybe filter, maybe order" (zero
joins/subqueries), because local-first datasets are pre-scoped by sync rules
(ADR-0031). A structured-predicate surface matches that reality and closes the
injection hole by construction.

## Decision

Adopt a single ratified verb contract (the T1–T5 tiers) and a structured-
predicate query shape, ported SDK-by-SDK starting with Flutter (Wave 1).

### T1 — Lifecycle & auth

`open`, `connect`, `disconnect`, `close`, `setToken`, `signOut` (ADR-0029), the
existing reactive `status`, and a **new** `waitForFirstSync()` barrier.
`pauseSync()` / `resumeSync()` are the canonical pause/resume names (the
low-level `Nostos.disconnect()`/`resume()` stay as back-compat aliases): they
retain token, schema, and watch subscriptions, and watches re-emit on resume
without caller re-wiring. No wire-protocol change.

### T2 — Reads (all reactive verbs have one-shot twins)

`collection<T>(table, fromRow, toRow?, pk)` is the typed handle. New verbs:
`get(pk)` (one-shot single row; `fetchById` is the alias), `watchOne(pk)`
(reactive single row — detail screens shouldn't re-render on list churn),
`exists(where?)` (reactive boolean). Existing `getAll`/`watch`/`count` migrate
to structured `where`/`orderBy`/`limit`/`offset`.

**Structured predicates:** `where` is data, not strings. Operators v1:
`eq, neq, lt, lte, gt, gte, inList, isNull, notNull` + `and, or, not`;
`orderBy` is `[Order.asc(field)|Order.desc(field)]`. Column names are
identifier-validated and values are emitted as safe SQLite literals, so nothing
the caller supplies is spliced raw — killing the injection P1. Mirrors
`Predicate` in nostos-domain (ADR-0012) so UniFFI can carry one shape across all
nine SDKs.

### T3 — Writes (durable collapsed outbox; ADR-0013)

`upsert`/`upsertRow`, `patch` (the canonical per-field LWW update, ADR-0014),
`delete` — all unchanged. **New:** `writeBatch([...])` for all-or-nothing *entry*:
the group lands in the outbox in ONE SQLite transaction (or none do) and uploads
in one round. The `Outbox::enqueue_batch` trait method carries a default
(sequential, best-effort) so Wave 2's WASM backend inherits the seam;
`SqliteStorage` overrides it with a real transaction.

> **`writeBatch` is NOT a server transaction.** The server applies each row
> individually with per-field LWW; there is no cross-row rollback and no
> all-or-nothing *apply*. Two ops touching the same row/field collapse to the
> last one's value (outbox pk-dedup + server idempotent upsert). Docs must say
> this verbatim — a batch that merely *looks* transactional is the high-risk
> advisor-flagged failure mode.

### T4 — CRDT typed surface (engine shipped WS3 @317b4d1; OR-set exposed Wave 1)

`Collection.orSetAdd(pk, element)` / `orSetRemove(pk, element)` are exposed on
the Flutter typed surface, wired to the WS3-shipped OR-set engine via FFI
(`SyncClient::or_set_add`/`or_set_remove` mint a client HLC and enqueue a
merge-upsert). **Counters are NOT exposed:** `WriteOp::Increment` is
server-authoritative (ADR-0030 D1) — the client cannot compute (column, delta)
locally, so there is no offline-first counter handle. See "Coverage gaps" below.

### T5 — Outbox observability (ADR-0027)

Counts live in `SyncStatus` (`pendingWrites`, `deadLetteredWrites`,
`lastWriteError`); **new** read-only `deadLetters()` list (id, table, op, pk,
attempts, payload, error, timestamp) so permanently-failed writes are
diagnosable. Each `DeadLetter` carries the server's per-row `error` and a
quarantine `timestamp` (persisted via the additive `last_error`/
`dead_lettered_at` outbox columns + `mark_dead_letter_with_error`). `retryDeadLetter(id)`
/ `discardDeadLetter(id)` are **deferred to v1.1** (amends ADR-0027's
counts-only stance for the list, not yet for mutation).

### Escape hatch

`execute()` and `watchSql()` (raw SQL) are kept, demoted to last in docs, and
carry a greppable name. They exist for queries the structured surface cannot
express yet (e.g. an `(col IS NULL) DESC` order, joins, projections). The
atlet `sessions` sort uses `watchSql` for exactly this reason — the structured
`Order` is field+direction only.

## Consequences

- **Breaking change** to the pre-contract Flutter `Collection.watch`/
  `count`/`getAll` signatures: `where: String?` → `where: Where?`,
  `orderBy: String?` → `orderBy: List<Order>?`. The pilot app and the facade
  tests are the only callers; both migrate with this change. The other eight
  SDKs adopt the same shape when their wave lands.
- Reads remain over **SQLite views on `cairn_data`** (ADR-0028) — never
  materialized typed tables.
- `writeBatch` entry atomicity is REAL: `NostosHandle::write_batch` (FFI) +
  `Outbox::enqueue_batch` (trait, default sequential) + `SqliteStorage`
  override (one SQLite transaction) — a mid-batch failure rolls back the whole
  batch and leaves zero partial outbox rows. The WASM backend (Wave 2) inherits
  the default (best-effort) until it gains its own transactional override.
- `deadLetters()` `error`/`timestamp` are populated — the additive
  `last_error`/`dead_lettered_at` outbox columns + `mark_dead_letter_with_error`
  persist the server's per-row reason and a quarantine timestamp.

## Coverage gaps (reported, not self-resolved)

1. **CRDT counter typed surface (T4)** — OR-set handles shipped (orSetAdd/
   orSetRemove); counters remain server-authoritative (`WriteOp::Increment`,
   ADR-0030 D1) with no offline client handle. Exposing a counter needs an
   engine-side counter-merge primitive (not just an FFI wrapper).
2. **Expression `ORDER BY`** — structured `Order` is field+direction only; the
   `(col IS NULL) DESC` sort atlet needs is routed through the escape hatch.
   If real apps need it often, extend `Order` (contract change), don't widen
   the escape hatch.
3. ~~**`writeBatch` entry atomicity`**~~ — RESOLVED: `enqueue_batch` trait
   method + `SqliteStorage` transactional override + `write_batch` FFI shipped
   (see Consequences).
4. ~~**Per-dead-letter `error`/`timestamp`**~~ — RESOLVED: additive
   `last_error`/`dead_lettered_at` columns + `mark_dead_letter_with_error`
   shipped (see T5).

## Signature check

`scripts/check-doc-signatures.py` verifies every method signature documented in
`docs/api/flutter.md` resolves to a real method in the Dart source. Standalone
(not wired into `make ci`/`make lint` — those remain `fmt-check` + `clippy`).
Run manually: `python3 scripts/check-doc-signatures.py` (exits 0 when all
documented methods are found).
