# Direct mode — the sync protocol, as the prior art says to build it

**Date:** 2026-09-22. **Status:** design, grounded in fetched docs; no code yet.
**Companion to:** `nostos-on-device-no-always-on-server.md`, which argues *why*
direct mode (Shape B2) is the shape. This document is *how*, and it exists
because the naive version of this protocol silently loses rows.

Direct mode = the device subscribes to the backend's own realtime surface and
writes through its own REST API. No Nostos server. Everything in `nostos-client`
above the frame source is unchanged.

The design below is not invented here. Four independent implementations of this
exact protocol — WatermelonDB, RxDB, Confluent's JDBC source, PowerSync —
publish the same rules, and two of them publish them as warnings.

## Rule 1 — the checkpoint is `(updated_at, pk)`, never a bare timestamp

A bare `WHERE updated_at > $watermark` loses rows whenever two rows share a
timestamp and the page boundary falls between them.

RxDB states the requirement as a data-layout precondition: documents must be
**"deterministically sortable by their last write time"**, where *deterministic*
means "even if two documents have the same last write time, they have a
predictable sort order", and the fix is "using the *primaryKey* as second sort
parameter **as part of the checkpoint**".

Confluent's JDBC source connector reaches the same conclusion from the CDC side:
`timestamp+incrementing` mode "is the most robust because it can combine the
unique, immutable row IDs with modification timestamps to **guarantee
modifications are not missed** even if the process dies in the middle of an
incremental update query."

**So:** `cairn_meta` stores `(updated_at, pk)` per table, and catch-up is
`WHERE (updated_at, pk) > ($ts, $pk) ORDER BY updated_at, pk`. Two agreeing
sources, one of them a decade of production CDC.

## Rule 2 — `updated_at` is written by the database, never by the client

A device's clock is not trustworthy and a device is the last thing that should
be deciding its own watermark position. WatermelonDB's pull-endpoint contract
says to "mark the current server time **synchronously** with the queries".

**So:** `nostos link --mode direct` generates a `BEFORE UPDATE` trigger that
stamps `updated_at = now()`, and refuses a table that lets the client write the
column. A client-writable watermark column is a data-loss bug with a plausible
appearance.

## Rule 3 — soft delete is mandatory, not a preference

A catch-up query cannot see a row that is gone. Every implementation of this
protocol solves it the same way and none of them make it optional.

RxDB: "documents are **never deleted**, instead the `_deleted` field is set to
`true`. This is needed so that the deletion state of a document exists in the
database and can be replicated to other instances."

WatermelonDB's pull response carries an explicit `deleted` array — "IDs of all
records that were deleted on the server since `lastPulledAt`" — which is the
same thing with a different carrier: the server must retain the fact of the
deletion for at least as long as the longest plausible client absence.

Supabase adds a specific reason not to rely on the realtime DELETE event for
this: "RLS policies are not applied to `DELETE` statements, because there is no
way for Postgres to verify that a user has access to a deleted record", and
filtering delete events at all requires `replica identity full`.

**So:** `deleted_at timestamptz` on every synced table, the read VIEW filters it,
and a documented purge window that must exceed the longest tolerated absence.
`nostos link` refuses a table without it.

## Rule 4 — the realtime stream is a doorbell, and reconnect always resyncs

This is the rule that retires the "`realtime.messages` is only retained ~3 days"
worry, and it is the one most likely to be skipped.

RxDB, explicitly: "When the client goes offline and online again, it might happen
that the `pullStream$` has missed out some events. Therefore the `pullStream$`
should also emit a **RESYNC** event each time the client reconnects, so that the
client can become in sync with the backend via checkpoint iteration." And if a
backend cannot provide a complete stream, RxDB's advice is to emit *only* RESYNC
— "anything unknown has changed on the server".

Nostos already holds this position in its own words: push is a **"wake-up
trigger, not a data channel"** (`docs/STRATEGY.md:214`, ADR-0037). Direct mode
does not need a new principle, it needs the existing one applied to a second
change source.

**So:** the Supabase Realtime subscription is never the source of truth. Every
reconnect runs checkpoint iteration before trusting a single streamed frame, and
a streamed frame is only a fast path that saves a round trip. Missed messages,
dropped sockets, 3-day partition drops and a fortnight in a drawer all collapse
into the same code path — the one that is exercised on every single reconnect
rather than only in the rare case.

## Rule 5 — take the watermark *before* the query, and let duplicates happen

WatermelonDB's contract is emphatic here: the pull "MUST provide a consistent
view of changes since `lastPulledAt`", achieved by performing "all queries
synchronously or in a write lock", because otherwise "**some records would never
be returned in a pull query**". And when that is impossible — which it is for a
device issuing N independent PostgREST requests — the instruction is to "return a
`lastPulledAt` timestamp marked BEFORE querying starts."

That trades duplicate delivery for zero loss, which is the right trade **and is
free for Nostos specifically**: `cairn_data` is keyed `PRIMARY KEY (table_name,
pk)` and a frame is a complete row image, so re-applying a row is a no-op by
construction. Nostos's storage model already pays for this.

**So:** advance the watermark to a value captured before the first request in a
catch-up pass, never to the max seen in the results.

## Rule 6 — private channels, and the setting that silently disables them

`realtime.broadcast_changes()` requires broadcast authorization. Supabase's
authorization guide: access is controlled "by adding Row Level Security policies
to the `realtime.messages` table", and — the footgun — **"to enforce private
channels you need to disable the 'Allow public access' setting in Realtime
Settings"**.

**So:** `nostos doctor` checks the setting, not just the policies. A project with
correct RLS and public access left on is an open channel, and nothing in the app
behaves differently.

## What direct mode cannot have, and this is the real cost

**Cross-table transactional consistency.** Per-table watermarks mean a device
can hold an order line whose order header has not arrived. No amount of care in
rules 1–6 fixes it, because the consistency unit is the table.

PowerSync built a service to solve exactly this and had it Jepsen-verified. Their
description of what the service buys: a checkpoint is "a single point-in-time on
the server (similar to an LSN in Postgres) with a consistent state: only fully
committed transactions are part of the state. The client only updates its local
state when it has all the data matching a checkpoint… There is no intermediate
state while downloading large sets of changes such as large server-side
transactions. **Different tables and buckets are all included in the same
consistent checkpoint.**"

That is a correctness argument for a sync server, and it is a much better one
than the throughput argument this project has been making. Two consequences:

1. **`nostos-server`'s pitch should lead with consistency, not ops/sec.** It
   applies at commit boundaries with `txn_id` and `lsn` on every event
   (`crates/nostos-domain/src/events.rs`); direct mode has neither. The
   benchmark numbers are a second-order claim next to "your client never sees a
   half-applied transaction".
2. **Direct mode must say this out loud.** It is correct for
   single-table-at-a-time reads and per-row invariants — the large majority of
   app screens. It is wrong for anything that reads two tables and requires them
   to agree. That sentence belongs in the mode's first paragraph.

A partial mitigation worth exploring later, not at first ship: have the trigger
broadcast a transaction marker so the device can buffer to commit boundaries for
*streamed* frames. It does nothing for the catch-up path, which is the primary
path per rule 4, so it is polish rather than a fix.

## Also true, and worth stating

**Electric does not do this at all.** Their writes guide pairs sync with ordinary
web-service calls for the write path — Electric is read-path only. Direct mode
keeping Nostos's durable outbox and writing through PostgREST is therefore a
*larger* surface than Electric offers, not a reduced one.

**Brick is the closest existing thing**, and Supabase's own blog post about it
calls the request-queue-around-Supabase approach "an admittedly brittle
solution". Rules 1–6 are the difference between that and a protocol, and they
are also the reason this is worth building rather than wrapping.

## Implementation order

Rules 1–6 are not optional and they are not phases. A direct mode shipped
without rule 4 loses data for any device offline longer than the broadcast
retention window, and the symptom is a row that is quietly missing forever.

1. `ChangeSource` seam in `nostos-client` (`client.rs` today speaks only `/sync`;
   `iroh_dial.rs` is the precedent for a second path).
2. Composite `(updated_at, pk)` checkpoint per table in `cairn_meta`.
3. PostgREST catch-up with keyset pagination; watermark captured pre-query.
4. Realtime private-channel subscription decoding `broadcast_changes` payloads
   into `RowOp`; RESYNC on every reconnect.
5. Outbox drain → PostgREST upsert / soft-delete.
6. `nostos link --mode direct`: trigger + RLS policy + column generation, and a
   hard refusal for any table missing `updated_at` or `deleted_at`.
7. `nostos doctor --mode direct`: policies, the public-access setting, watermark
   indexes, realtime enabled.
8. One conformance suite both modes pass, with a "device offline past the
   retention window" case as a first-class test rather than an edge case.

## Sources (fetched 2026-09-22)

- WatermelonDB, [implementing your sync backend](https://watermelondb.dev/docs/Sync/Backend)
  — pull-endpoint contract, consistent-view requirement, watermark-before-query.
- RxDB, [replication protocol](https://rxdb.info/replication.html) — deterministic
  sort requirement, `_deleted` requirement, RESYNC-on-reconnect.
- Confluent, [JDBC source connector](https://docs.confluent.io/kafka-connectors/jdbc/current/source-connector/overview.html)
  — `timestamp+incrementing` as the robust mode.
- PowerSync, [consistency](https://docs.powersync.com/architecture/consistency)
  — causal+ via cross-table checkpoints, Jepsen-verified.
- Supabase, [Realtime authorization](https://supabase.com/docs/guides/realtime/authorization) ·
  [Broadcast](https://supabase.com/docs/guides/realtime/broadcast) ·
  [Postgres Changes](https://supabase.com/docs/guides/realtime/postgres-changes)
  — RLS on `realtime.messages`, the public-access setting, DELETE/RLS caveat.
- Electric, [writes guide](https://electric-sql.com/docs/guides/writes) — read-path
  only; writes via ordinary web services.
- Supabase, [offline-first with Brick](https://supabase.com/blog/offline-first-flutter-apps)
  — "an admittedly brittle solution".
