# Direct mode — the sync protocol, and how the device gets transactional consistency

**Date:** 2026-09-22. **Status:** design, grounded in fetched docs; no code yet.
**Companion to:** `nostos-on-device-no-always-on-server.md` (*why* direct mode).
**Supersedes:** this file's first draft (per-table `updated_at` watermarks),
which conceded that cross-table transactional consistency was impossible
without a sync server. **It isn't.** The concession was an artefact of the wrong
change-source design, and the operator was right to refuse it.

Direct mode = the device syncs from the client's own Postgres via PostgREST +
Realtime. No Nostos server, no second box, every sync decision made on the
device.

## The design: a change log in the client's own database

One append-only table in a `cairn` schema. A trigger on each synced table
appends to it **inside the writing transaction** — the transactional outbox
pattern, whose entire purpose is to make the data change and the change
notification atomic without two-phase commit.

```sql
create table cairn.changes (
  seq        bigserial primary key,
  xid        xid8        not null default pg_current_xact_id(),
  table_name text        not null,
  pk         text        not null,
  op         text        not null,          -- insert | update | delete
  row        jsonb,                          -- full row image; null on delete
  scope      text                            -- tenant/owner, stamped by trigger
);
create index on cairn.changes (xid, seq);
```

Everything below follows from two properties of that table: **one sequence
across every table**, and **a transaction ID on every row**.

## Why this gives cross-table transactional consistency

`pg_current_xact_id()` returns the writing transaction's ID, typed `xid8` — and
the PostgreSQL docs are explicit that `xid8` "does not wrap around during the
life of an installation", so it is a permanent monotonic identity, not a
recycled counter.

So every change row carries the transaction that produced it, across all
tables. The device groups a pull by `xid` and applies each group in **one SQLite
transaction**. A transaction that touched three tables lands as three tables'
worth of rows or none of them.

That is the same property PowerSync's server-side checkpoints provide — "only
fully committed transactions are part of the state… different tables and buckets
are all included in the same consistent checkpoint" — obtained here from
Postgres itself rather than from a service. Nostos's own `ReplicationEvent`
already carries `txn_id` (`crates/nostos-domain/src/events.rs`), and
`ApplyEngine` already applies at commit boundaries, so the client-side half of
this **already exists**; only the frame source changes.

## Why it cannot lose a row: the snapshot horizon

The hazard that kills naive change logs: `seq` is assigned when the row is
*inserted*, but the row becomes visible when its transaction *commits*. A
transaction that grabs `seq = 100` and commits five seconds after one that
grabbed `seq = 101` will appear *below* a watermark that has already advanced
past it. The row is then never read again. Silent, permanent loss.

Postgres exports the fix, and the docs name this exact use case: the transaction
ID and snapshot functions exist "to determine **which transactions were
committed between two snapshots**."

`pg_current_snapshot()` returns `xmin:xmax:xip_list`, and
`pg_snapshot_xmin(...)` is the lowest transaction ID **still in progress**.
Every `xid` below it is finished — committed (so its log rows are visible) or
aborted (so its log rows never existed). Nothing new can ever appear below it.

**That makes the horizon a safe, monotonic, gapless checkpoint.** The device
stores one `xid8`, not a timestamp per table:

```sql
create function cairn.pull(since xid8, max_rows int default 2000)
returns table (horizon xid8, seq bigint, xid xid8,
               table_name text, pk text, op text, row jsonb)
language sql stable security invoker as $$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon)
  select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c.row
  from cairn.changes c, h
  where c.xid >= since and c.xid < h.horizon
  order by c.xid, c.seq
  limit max_rows;
$$;
```

Three things fall out of this being **one function call**:

1. **The snapshot and the rows come from one transaction**, so the checkpoint is
   marked synchronously with the query. WatermelonDB's backend contract demands
   exactly this — "perform all queries synchronously or in a write lock… to
   ensure that no changes are made to the database while you're fetching
   changes (otherwise **some records would never be returned in a pull
   query**)" — and offers a lossy duplicate-tolerant fallback for backends that
   can't. A single Postgres function does not need the fallback.
2. **`security invoker`** means RLS applies as the calling user. Scoping stays
   in Postgres, i.e. still server-authoritative.
3. PostgREST exposes it at `/rest/v1/rpc/pull`. Supabase's custom-schema guide
   covers the setup: add `nostos` to "Exposed schemas" in API settings, then
   `GRANT USAGE ON SCHEMA` + `GRANT ALL ON ALL ROUTINES` to `authenticated`.

## What this dissolves from the first draft

| first draft's "non-negotiable" rule | status now |
|---|---|
| composite `(updated_at, pk)` checkpoint | **gone** — one `xid8` horizon |
| `updated_at` trigger on every synced table | **gone** — no clock involved anywhere |
| soft delete mandatory on user tables | **gone** — the log records `op = delete` |
| watermark-before-query, tolerate duplicates | **gone** — one RPC, one snapshot |
| no cross-table transactional consistency | **gone** — group by `xid` |

WatermelonDB's own tips anticipate this. After describing the timestamp approach
they warn it needs a stored procedure enforcing "uniqueness and monotonicity" to
protect "against weird edge cases — such as records being lost due to server
clock time changes (NTP time sync, leap seconds, etc.)", then name the
alternative: "an auto-incrementing counter sequence, but you must ensure that
this sequence is **consistent across all collections**." A single change log is
that sequence, consistent across all collections by construction.

## What survives

**The realtime stream is a doorbell, and every reconnect pulls.** A second
trigger on `cairn.changes` calls `realtime.broadcast_changes()` on a
scope-keyed private channel. A message means "call `pull`", nothing more. RxDB
states the rule: "when the client goes offline and online again, it might happen
that `pullStream$` has missed out some events. Therefore `pullStream$` should
also emit a RESYNC event each time the client reconnects." Nostos already holds
the same position — push is a "wake-up trigger, not a data channel"
(`docs/STRATEGY.md:214`, ADR-0037). This also makes the ~3-day
`realtime.messages` retention a non-issue: a device away for a month takes the
same code path as one that blinked.

**Private channels need the setting, not just the policy.** Supabase: access is
controlled "by adding Row Level Security policies to the `realtime.messages`
table", and "to enforce private channels you need to **disable the 'Allow public
access' setting** in Realtime Settings". `nostos doctor` checks the setting.

## The honest costs — five, and none of them is a correctness hole

1. **Write amplification.** Every change to a synced table writes a second row,
   in the same transaction, so writes get slower and the database grows. This is
   the price of the outbox pattern and it is the main reason to prefer logical
   replication when you *can* run a server.
2. **RLS has to be expressible on one table.** The log holds row images from
   many tables, so one policy on `cairn.changes` must say what N table policies
   say. The `scope` column stamped by the trigger covers the common
   tenant/owner case. A client whose RLS involves joins across tables is real
   work, and `nostos link` should refuse rather than guess.
3. **A long write transaction delays everyone.** The horizon cannot pass an
   in-flight write, so a 30-second transaction holds sync back 30 seconds.
   Logical replication has the identical property — nothing can be emitted
   before commit — so this is not a regression, but it is worth knowing.
4. **Retention and re-snapshot.** Prune the log on a window; a device that was
   away longer re-snapshots the tables through PostgREST and resets its horizon.
   Needs the same "read the horizon first, then the rows" discipline.
5. **`track_commit_timestamp` is deliberately not used.** It would give real
   commit times via `pg_xact_commit_timestamp`, but the docs say it only works
   "for transactions that were committed after it was enabled" and that "commit
   timestamp information is routinely removed during vacuum". The snapshot
   horizon needs no server setting and no vacuum-sensitive data.

## Implementation order

1. `ChangeSource` seam in `nostos-client` (`client.rs` speaks only `/sync` today;
   `iroh_dial.rs` is the precedent for a second path).
2. `xid8` horizon in `cairn_meta` beside the existing LSN checkpoint.
3. `PostgrestChangeSource`: `rpc/pull` → group by `xid` → `RowOp` batches
   handed to the existing `ApplyEngine` at transaction boundaries.
4. Realtime private-channel subscription as doorbell; pull on every reconnect.
5. Outbox drain → PostgREST write; the log trigger captures the echo, which the
   idempotent `(table_name, pk)` store absorbs.
6. `nostos link --mode direct` generates the schema, the per-table triggers, the
   `pull` function, the broadcast trigger, the RLS policies and the grants — and
   refuses any table whose RLS it cannot express as a `scope`.
7. `nostos doctor --mode direct`: exposed schema, grants, policies, the
   public-access setting, log growth, oldest-unpruned vs. horizon lag.
8. One conformance suite both modes pass, including **"transaction touching
   three tables is never seen half-applied"** and **"device offline past the
   retention window"** as first-class cases.

## Worth an ADR

Second sync topology on the public client surface; hard to reverse; a real
trade-off (write amplification and one-table RLS, against no server). That
clears the bar in `.claude/skills/grill-with-docs`. Not written — the decision
is the operator's.

## Sources (fetched 2026-09-22)

- PostgreSQL 18, [system information functions §9.27.8–9.27.9](https://www.postgresql.org/docs/current/functions-info.html)
  — `pg_current_xact_id`, `xid8` non-wrapping, `pg_current_snapshot`,
  `pg_snapshot_xmin`, "which transactions were committed between two snapshots",
  and the `track_commit_timestamp` caveats.
- PostgreSQL 18, [replication settings](https://www.postgresql.org/docs/current/runtime-config-replication.html)
  — `track_commit_timestamp`.
- [Transactional outbox pattern](https://microservices.io/patterns/data/transactional-outbox.html)
  — atomic data change + notification without 2PC.
- WatermelonDB, [sync backend](https://watermelondb.dev/docs/Sync/Backend)
  — consistent-view requirement; the clock-skew warning; the cross-collection
  sequence alternative.
- RxDB, [replication protocol](https://rxdb.info/replication.html) — RESYNC on
  every reconnect; deterministic ordering.
- PowerSync, [consistency](https://docs.powersync.com/architecture/consistency)
  — what a cross-table checkpoint buys, Jepsen-verified.
- Supabase, [custom schemas](https://supabase.com/docs/guides/api/using-custom-schemas) ·
  [Realtime authorization](https://supabase.com/docs/guides/realtime/authorization) ·
  [Broadcast](https://supabase.com/docs/guides/realtime/broadcast)
- Confluent, [JDBC source connector](https://docs.confluent.io/kafka-connectors/jdbc/current/source-connector/overview.html)
  — why timestamp-only incremental queries miss rows.
