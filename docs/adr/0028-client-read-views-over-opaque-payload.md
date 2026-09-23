---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "The on-device read model every consumer query targets. Views are cheap to DROP, but the *rejection* is the expensive half: reversing it means a real SQLite migration on every installed device (new per-table tables, backfill from cairn_data, dual-write during rollout), plus re-pointing the apply engine, `nostos gen` output, and every `watch`/`getAll` call site. Cheap to keep, expensive to undo."
  surprising_without_context: true
  surprise_reason: "A reader seeing `json_extract(payload, '$.col')` in every generated view asks 'why not real typed columns?' — and until this ADR the first answer they found said the opposite: ADR-0021 states the client will 'auto-build its typed tables', and `sqlite.rs`'s own ponytail comment named 'real typed tables + indexes' as the upgrade path. Both were written before the two facts that decide it: the affinity motivation was spent by a server-side fix, and a partial expression index removes the stated ceiling without materializing anything."
  result_of_real_tradeoff: true
  rejected_alternatives: "Materialized typed tables (one real table per synced table, columns + affinities from the /schema descriptor) — the shape the 2026-07-13 redesign plan specified. Rejected: its stated motivation (column affinity kills the TEXT->TIMESTAMPTZ bug class) was spent when that bug was fixed server-side by a chrono bind, and its claimed advantage (indexable non-PK columns) is obtainable on the existing table with a partial expression index, measured below. Costs a device migration per schema change and a second copy of every synced row for a benefit nothing currently needs."
  all_three_true: true
status: accepted
date: 2026-07-30
---

# ADR-0028: Client read model is SQLite VIEWs over the opaque payload; materialized typed tables are rejected

- **Status:** Accepted
- **Date:** 2026-07-30
- **Supersedes:** the earlier "materialized typed tables" architecture
  (D1 + Architecture changes). Corrects the "auto-build its typed tables" phrasing
  in [ADR-0021](0021-client-schema-discovery-rest.md).

## Context

`SqliteStorage` stores every synced row in one opaque table —
`cairn_data(table_name, pk, payload, applied_lsn)` — where `payload` is
column-named JSON. Typed reads happen through `apply_schema`
(`crates/nostos-client/src/sqlite.rs`), which materializes **one SQLite `VIEW` per
synced table**: `pk AS _pk` plus `json_extract(payload, '$.<col>') AS <col>` for
each column in the `/schema` descriptor (ADR-0021). `DROP VIEW` + `CREATE VIEW`
per apply, so re-declaring a changed schema refreshes the projection in place and
**bumping the schema IS the client migration** — no data is touched.

The 2026-07-13 redesign plan specified something else: replace `cairn_data` with
**real typed tables**, one per synced table, columns and affinities from the
server descriptor. WS2 shipped the view form instead, as the "lazy cousin", with a
`ponytail:` comment naming typed tables as the fast-follow. That left the
architecture of record and the code disagreeing for two and a half weeks, and left
the code's own comment pointing at a rewrite.

Two facts settle it, and neither was known when the plan was written.

**1. The affinity motivation is spent.** D1's justification was that real column
types "kill the TEXT→timestamptz class of bug" — an `add-task` failure where a
`created_at` JSON string was bound as TEXT and Postgres rejected
`TEXT → timestamptz`. Real-Postgres repro on 2026-07-13 located that bug in
`PgWriteBack`'s **server-side** bind, fixed with chrono. It was never a client
storage-model bug. Client-side affinity would not have prevented it and its
absence does not reintroduce it.

**2. The stated ceiling is removable without materializing anything.** The
`ponytail:` comment claimed "no non-PK column indexes (a view computes
`json_extract` per row → full scan on `WHERE col = ?`)". True of the view as
declared, false of the model: SQLite indexes **expressions**, and `json_extract`
is deterministic, so the index goes on the base table and the planner uses it
*through* the view. Measured on SQLite 3.51 with 5,000 rows:

```sql
-- The view is named after the table itself — `view_name()` is
-- `table.replace('.', "_")`, and pg.rs strips a `public.` prefix, so a
-- `public.tasks` relation projects to a view literally called `tasks`.
CREATE VIEW tasks AS
  SELECT pk AS _pk, json_extract(payload,'$.title') AS title
  FROM cairn_data WHERE table_name='tasks';

EXPLAIN QUERY PLAN SELECT * FROM tasks WHERE title='t42';
-- before: SCAN cairn_data

CREATE INDEX ix ON cairn_data(json_extract(payload,'$.title'))
  WHERE table_name='tasks';          -- partial + expression index

EXPLAIN QUERY PLAN SELECT * FROM tasks WHERE title='t42';
-- after:  SEARCH cairn_data USING INDEX ix (<expr>=?)
```

Correct rows either way; the partial predicate keeps the index scoped to one
logical table. (Measured on a standalone fixture with these exact statements, not
through the client.)

That naming is what makes `SELECT * FROM tasks` work with no prefix to learn, and
it is why raw DML against a synced table hits a **view** rather than missing
entirely — the loud-failure property below depends on it.

> **Known edge, non-public schemas — RESOLVED 2026-08-17.** The mismatch
> this note predicted was real: `view_name` collapses the dot (view
> `myschema_tasks`) while the Dart structured paths built
> `SELECT * FROM myschema.tasks`, which SQLite reads as schema `myschema`.
> Fixed on the Dart side: every structured path (`_composeQuery`, `count`,
> `exists`) now normalizes through `_viewName()`, the exact mirror of the
> Rust rule, so `Collection` over a schema-qualified table hits its view.
> Pinned by `sdk/nostos_flutter/test/view_name_test.dart`. Raw-SQL callers
> (`NostosDatabase.watch`/`getAll`) get NO normalization by design — they must
> write the collapsed view name themselves (documented on `_viewName`).

## Decision

**The client read model is SQLite VIEWs over `cairn_data`. Materialized typed
tables are rejected — not deferred.**

When a query is measurably slow, the sanctioned fix is a **partial expression
index on `cairn_data`**, as above. Not a storage rewrite.

Reopen this only with a measurement that an expression index cannot satisfy. A
plausible future trigger is a genuine need for SQLite **column affinity** —
`json_extract` returns the JSON value's own type, so a `TIMESTAMPTZ` arriving as
a JSON string stays TEXT, and `ORDER BY`/range predicates on it sort
lexicographically. That is a real limitation; it is simply not one anything in
the tree currently hits, and ISO-8601 sorts correctly as text.

## Consequences

**Good.** Zero extra storage — one copy of each row, not two. Zero device
migration on schema change: `DROP VIEW`/`CREATE VIEW` at connect, data untouched.
Reversible by construction. The write path stays single: replication events
`UPSERT` into `cairn_data` and every reader sees them through the view, so there
is no second copy to keep coherent.

**Bad.** No column affinity (above). Views are read-only — `INSERT`/`UPDATE`/
`DELETE` against a synced table name fails with `cannot modify <view> because it
is a view`. That is *load-bearing*, not incidental: it makes the raw-SQL write
path fail **loudly** instead of silently diverging local state from the
replication stream, which is exactly why `NostosDatabase.execute` is a read-only
alias of `getAll`. Note the boundary is conventional, not enforced —
`SqliteStorage::query` runs whatever SQL it is handed, and a statement aimed at
an internal table (`cairn_outbox`) *would* execute. Views protect the documented
names, not the internal ones.

**Neutral.** `FakeReplicator`'s non-JSON bytes project to `NULL` through
`json_extract` — a dev-fixture artifact, not a production path.

## Alternatives rejected

**Materialized typed tables** (the plan's D1). Motivation spent (fact 1);
advantage obtainable in place (fact 2); costs a per-device migration on every
schema change and a duplicate of every synced row. Rejected.

**Views now, typed tables on the roadmap.** Considered and rejected as worse than
either commitment: a standing "fast-follow" invites a future agent to spend a
sprint on a rewrite whose justification no longer exists. Deciding *against* it,
in writing, is the point of this ADR.

## Implementation notes

`crates/nostos-client/src/sqlite.rs::apply_schema` is the whole of it, with
`apply_schema_creates_queryable_view_over_opaque_payload` and
`apply_schema_migration_refreshes_view_columns` as the guards. The `ponytail:`
comment there cites this ADR instead of proposing typed tables.
