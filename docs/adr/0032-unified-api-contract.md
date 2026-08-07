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
`delete` — all unchanged. **New:** `writeBatch([...])` for all-or-nothing
*delivery*: the group enters the outbox together and uploads in one round.

> **`writeBatch` is NOT a server transaction.** The server applies each row
> individually with per-field LWW; there is no cross-row rollback and no
> all-or-nothing *apply*. Two ops touching the same row/field collapse to the
> last one's value (outbox pk-dedup + server idempotent upsert). Docs must say
> this verbatim — a batch that merely *looks* transactional is the high-risk
> advisor-flagged failure mode.

### T4 — CRDT typed surface (engine shipped WS3 @317b4d1; unexposed)

`counter(pk,col).increment(n)` / `orSet(pk,col).add(v)` — deferred past Wave 1
(no wave assigns it). See "Coverage gaps" below.

### T5 — Outbox observability (ADR-0027)

Counts live in `SyncStatus` (`pendingWrites`, `deadLetteredWrites`,
`lastWriteError`); **new** read-only `deadLetters()` list (id, table, op, pk,
attempts, payload) so permanently-failed writes are diagnosable. `retryDeadLetter(id)`
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
- Wave-1 `writeBatch` entry atomicity is best-effort at the Dart layer: each
  op is a separate durable enqueue (the engine `write` FFI commits one outbox
  row per call). A mid-batch failure surfaces a `WriteBatchPartialError`
  carrying the completed ids; true transactional entry needs a Rust
  `write_batch` FFI (one SQLite transaction), deferred past Wave 1's
  no-codegen constraint. The common (all-valid) case is indistinguishable from
  a transactional entry.
- `deadLetters()` `error`/`timestamp` come back null in v1 — `cairn_outbox`
  has no per-row error-text or dead-lettered-at column. Persisting them is a
  schema migration queued for v1.1.

## Coverage gaps (reported, not self-resolved)

1. **CRDT typed surface (T4)** — engine exists, no wave assigns the typed
   surface. Flagged for the tech lead; not implemented in Wave 1.
2. **Expression `ORDER BY`** — structured `Order` is field+direction only; the
   `(col IS NULL) DESC` sort atlet needs is routed through the escape hatch.
   If real apps need it often, extend `Order` (contract change), don't widen
   the escape hatch.
3. **`writeBatch` entry atomicity** — needs a Rust `write_batch` FFI for true
   transactional entry; currently best-effort (see Consequences).
4. **Per-dead-letter `error`/`timestamp`** — needs a `cairn_outbox` schema
   migration (Rust) to persist; currently null.

## Signature check

`scripts/check-doc-signatures.py` is referenced by the contract/plan as the
doc-signature gate but **does not exist** in the tree (only `render-playbook.py`,
`sdk-e2e.sh`, `warp-ipv6-egress.sh` live in `scripts/`). Reported as a plan
defect; `make ci` / `make lint` (`fmt-check` + `clippy`) are the authority for
this wave.
