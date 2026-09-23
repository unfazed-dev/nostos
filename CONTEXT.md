# Nostos — Project Glossary

Nostos is a Rust-native local-first sync engine (Postgres logical replication → fan-out
server → on-device SQLite) with Flutter/Dart, dotnet, and React-Native client SDKs.
This glossary is the canonical vocabulary for the client SDK surface — the words a
dev reads in the API and the docs. It is a glossary only; implementation details live
in ADRs and `docs/plans/`.

## Language

**NostosDatabase**:
The SQL-core sync handle — `connect`/`subscribe`/`watch(sql)`/`getAll`/`execute`/`write`.
The ratified low-level surface (2026-07-13 redesign). Raw SQL lives here.
_Avoid_: Client, Engine, Connection (those mean other things in this codebase).

**NostosStore** (facade):
The reactive layer over `NostosDatabase`. Produces typed `Collection<T>` handles and the
hot `SyncStatus` value. The beautiful DEFAULT surface; `NostosDatabase` is the escape hatch.
_Avoid_: Repository, Store-Engine.

**Collection\<T\>**:
A typed handle to one synced table, obtained from `db.collection<T>(table:, fromRow:, toRow:)`.
Exposes `watch()`/`count()`/`upsert()`/`delete()` in the row type `T`. One per table per `T`.
_Avoid_: Table-gateway, DAO, Repository (those imply ownership/query-language `Collection<T>` does not).

**watch() (facade)**:
Returns a `Stream<List<T>>` backed by the existing per-table hot-replay-shared pump
(`Nostos._watchCache` + `_replayLatest` — N callers share ONE upstream). `.asValueListenable()`
adapts to a `ValueListenable<List<T>>` for `ValueListenableBuilder` users (the ng-elf lesson,
opt-in, not forced).
_Avoid_: query-observable (rxdart/ng-elf vocabulary). The facade's primary type is a `Stream`.

**Collapsed write**:
nostos's write model — the client writes locally + enqueues; nostos-server's `PgWriteBack`
applies the write upstream (server-gated by `NOSTOS_WRITE_TABLES`). The dev does NOT write an
`uploadData` callback. This is the DX moat vs a split upload model.
_Avoid_: upload, sync-write, push-write.

**SyncStatus**:
The sync-state value exposed as a hot `ValueListenable` via `db.status` (and synchronously as
`db.currentStatus`). Carries `conn`, `syncing`, `reconciling`, `lastSyncedAt`, and upload/download
errors. Honest about in-flight work; does NOT yet carry `DataTrust` (gated behind P0 fixes).
_Avoid_: ConnectionState (that is the WebSocket-level enum inside `ConnState`).

**ConnState**:
The connection-level enum: `connecting | connected | reconnecting | disconnected`. One field
inside `SyncStatus`, not the whole status.
_Avoid_: Status, State.

**DataTrust** (gated, not yet shipped):
A trust grade `fresh | stale | reconciling` indicating whether the local image can be rendered
as ground truth. GATED behind the P0 fixes (client WAL backfill across offline gaps; offline
hard-delete orphan reconciliation). Ships only when it can be true; a permanent `stale` badge
on every app would poison the launch demo.
_Avoid_: Confidence, Freshness-score.

## Relationships

- A **NostosDatabase** owns one **NostosStore** (the facade); the store produces **Collection\<T\>** handles and the **SyncStatus** value.
- A **Collection\<T\>** wraps one synced table; its `watch()` emits the row type `T`; `upsert()`/`delete()` issue **collapsed writes**.
- **SyncStatus** contains a **ConnState**; it does NOT yet contain a **DataTrust** (gated).
- **Collapsed writes** are server-gated by `NOSTOS_WRITE_TABLES` (empty default); they reconcile to the server-authoritative image via per-field LWW (ADR-0014 tier a).

## Example dialogue

> **Dev:** "If I `todos.watch()` in three widgets, do I get three upstream subscriptions?"
> **Nostos:** "No — `watch()` returns a hot `ValueListenable` ref-counted per query; one re-execution fans out to all three. That's why we don't use a cold-stream-per-call — at our throughput that would storm."
>
> **Dev:** "Can I show a 'data is fresh' badge?"
> **Nostos:** "Not yet — `DataTrust` is gated behind the backfill and orphan-reconcile P0s. Until those land, `SyncStatus` is honest about `syncing`/`reconciling` but doesn't grade trust, because a permanent `stale` would be worse than no grade."

## Flagged ambiguities

- "watch" is overloaded — `NostosDatabase.watch(sql)` (ratified SQL-core, raw-SQL `Stream` of `Map` rows) vs `Collection<T>.watch()` (facade, typed `Stream<List<T>>` over the same hot-replay pump). Resolution: the facade's `watch()` is the typed default; the SQL-core one is the escape hatch. Both return `Stream`s; the facade's is typed + has `where`/`parameters`/`throttle` knobs.
- "stream" vs "valuelistenable" — the facade's primary reactive primitive is a `Stream` (builds on the existing hot-replay `_watchCache`). A `ValueListenable` is available via `.asValueListenable()` for `ValueListenableBuilder` users. Don't say "the watch ValueListenable" when you mean the `Stream`.
