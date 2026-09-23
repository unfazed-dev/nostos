# ADR-0039: Sync streams — server-defined, client-parameterized subscriptions

- **Status:** Accepted (implemented 2026-08-18; slices 1–9 on `main`).
- **Date:** 2026-08-18
- **References:** ADR-0009 (one checkpoint per socket), ADR-0011 (tenancy), ADR-0012 (safe-SQL-subset predicates), ADR-0013 (echo/idempotent apply), ADR-0022 (multi-table-per-socket), ADR-0031 (rules file + reload), design draft `docs/plans/p5-sync-streams-design.md` (the binding text).

## Context

Sync Streams — named, per-client-parameterized subscriptions with lazy
`syncStream(name, params).subscribe()` — are the biggest remaining
SDK-parity feature gap. Nostos's `where_sql` (ADR-0012) is
per-table-subscription and client-authored SQL text — fine for static shapes,
but it means client bytes reach the server as a query string, and there is no
way to add/drop a shaped subscription mid-session without reconnecting.

The hard requirement: parameterize WITHOUT weakening ADR-0011 tenancy or
opening a SQL-injection surface.

## Decision

### 1. Streams are server-defined, client-parameterized

A stream is a `[streams.<name>]` entry in `nostos_rules.toml` (ADR-0031):
`{ table, where = "owner_id = :owner AND priority >= :min" }`. The template is
the ADR-0012 grammar extended with `:param` placeholders in literal position
ONLY (`crates/nostos-domain/src/predicate_compile.rs`). Clients send a name +
a JSON params object — never SQL. Templates are validated at boot under EVERY
sync mode (JOIN/CTE/subquery/misplaced-placeholder shapes fail the grammar);
config errors are loud at boot, never at subscribe. The section participates
in the rules checksum in every mode, so a streams edit = a rules edit →
resnapshot via the existing ADR-0031 D3 close-and-reconnect (extended to
live streams: a template or table-decision change drops the socket).

### 2. Parameter binding is value-level, never textual (the injection answer)

The template parses ONCE at ruleset compile into a `PredicateExpr` with
`ColumnValue::Param(name)` marker leaves (`nostos-application/src/rules.rs`,
`ActiveRuleset::stream`). At subscribe, `bind_params` substitutes each param
as a typed leaf — strict: missing/extra/nested-placeholder params are loud
errors (`BindError`). An unbound `Param` that somehow survives to match time
NEVER matches — including under `Ne`, which has an explicit guard so a
non-match can't invert into match-everything (`nostos-domain/src/predicate.rs`).
On the snapshot path (real SQL), the bound tree compiles to
`WHERE … $1..$n` with tokio_postgres positional binds — every value bound,
nothing interpolated (`snapshot_source.rs::compile_stream_where`, a fourth
defense on top of the three at `snapshot_source.rs:15-30`). The port takes the
bound `PredicateExpr`, not SQL text, so non-pg adapters evaluate in-memory
with the exact live-path semantics.

### 3. Tenancy is unchanged — the fail-closed AND-wrap

The bound predicate folds into `build_stream_predicate` at exactly the
`where_sql` seam (`transport.rs`): rules scope FIRST and fail-closed (a stream
on a `NotSynced` table rejects), tenant clause LAST so it wraps everything.
The design's "param on the tenant column is dropped + overridden" was
implemented as the strictly-safer AND-wrap: `tenant = :rogue AND tenant =
<principal>` is the impossible predicate, so an escape attempt yields ZERO
rows — never the other tenant's data, and never the principal's rows under a
borrowed template. Pinned by `stream_predicate_tenant_wrap_is_fail_closed`
(unit) and `cross_tenant_param_abuse_never_leaks` (PG e2e).

### 4. Wire, routing, client

Additive serde-tagged frames on the existing JSON protocol
(`subscribe_stream`/`unsubscribe_stream`/`stream_error`; snapshot boundaries
gain an optional `stream` key — old clients ignore it, old servers hard-close
on unknown frames, a documented SDK error). Streams ride the socket's ONE
global checkpoint (no per-stream resume); a lazy add takes a targeted
per-stream snapshot (`SnapshotSource::snapshot_stream`) bracketed by
stream-tagged boundaries on the same FIFO channel. Rejects are non-fatal
`stream_error` frames; the socket stays up. Streams count against
`MAX_TABLES_PER_SOCKET = 32`; id reuse = idempotent replace. Client:
`sync_stream(name, params).subscribe()` → `StreamHandle` (drop unsubscribes),
a mid-session command queue on the write-path notify pattern, active-set
re-send on every reconnect; Flutter: frb `subscribe_stream(name, params_json)`
(params as a JSON string, the P3 no-codegen trick) + Dart
`syncStream(name, params).subscribe()`. Rows surface through the EXISTING
reactive layer — streams control which rows land in SQLite.

**Client-side safety fix recorded here because it is load-bearing:** a
stream-tagged snapshot boundary brackets a SUBSET of the table's rows, so the
client must NEVER drive the table-scoped orphan-reap (ADR-0014/0025) from it —
doing so would delete every local row outside the stream's predicate. Only
untagged, table-level boundaries reconcile (`nostos-client/src/client.rs`).

## Scope ceiling (ponytails)

- **JOIN/CTE membership is OUT of v1** and rejected at startup with a clear
  error. Why honest: replication events arrive per-table, one row image at a
  time — a JOIN predicate can't be evaluated against one row, and a row
  entering/leaving a JOIN result without itself changing emits no event on the
  streamed table. Upgrade paths: (a) dependency-table re-evaluation (reverse
  index table→streams, bounded EXISTS re-query — measure first); (b) PG-side
  materialized view per stream.
- **No per-stream resume in v1:** reconnect re-snapshots every active stream.
  The socket checkpoint + idempotent apply prevent duplicate rows. Upgrade
  path: per-stream LSN cursors if re-snapshot cost proves out (measure first).
- **v1 unsubscribe leaves local rows in place** — eviction is a separate
  concern; that's consistent with how comparable sync engines behave.
- **stream_error is log-only on the client in v1** (no app surface). The
  active set keeps the entry, so a server-side definition fix + reconnect
  self-heals.
- **Web/WASM client:** no mid-session subscribe channel in v1 —
  `unsubscribeStream`/`subscribeStream` throw `UnimplementedError` there.
- **Fan-out cost** is linear-in-streams-per-table (two streams on one table =
  two sessions in that table's shard; `store.rs`) — fine under the 32 cap;
  revisit only with a measurement (the ADR-0012 digest-index revert
  precedent). A P5 bench row (streams × params) is REQUIRED before marketing
  reuses the 833k ops/sec figure.
- **String params against uuid/int columns** render as `"col"::text <op> $n`
  (the tenant clause's trade): index bypass accepted; stream snapshots are
  per-subscribe, not hot.

## Consequences

- Sync-Streams parity for the v1 shape (named, parameterized, lazy)
  with a strictly stronger injection/tenancy story than client-authored SQL.
- The `where_sql` path is untouched; streams coexist with predicate
  subscriptions on one socket.
- PG-gated e2e (`tests/e2e_pg_sync_streams.rs`, §6 items 1–6) ran live against
  real Postgres 2026-08-18: **all 5 tests green**, including the cross-tenant
  abuse gate — alongside the full serialized pg suite (replication, snapshot,
  tenant-scope, write-back, op-log replay, write-amp, nostos-push PgStore) with
  zero failures. The suite self-skips without `NOSTOS_E2E_PG=1`, so PG-less CI
  stays green.
