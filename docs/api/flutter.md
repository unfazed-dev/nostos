# Flutter / Dart — `nostos_flutter`

The richest of the SDKs: reactive `Stream`s, **typed collections** (the taught
surface), structured predicates, and a sync-status signal. Contract: ADR-0032.

## Entry point — `NostosDatabase`

`NostosDatabase` is the supported surface. `Nostos` (the low-level engine handle) is still exported
as an escape hatch and is the seam tests fake against, but everything you need is here.

Three factories (`lib/src/nostos_database.dart:62`, `:97`, `:167`):

```dart
// 1. Raw URL — the general case.
final db = await NostosDatabase.connect(
  url: 'ws://127.0.0.1:8800/sync',
  token: null,               // bearer JWT; omit for NOSTOS_SYNC_AUTH=none
  schema: null,              // null → fetched from GET {base}/schema
  sqlitePath: '$dir/nostos.db',
);

// 2. Supabase — takes NO Supabase arguments. It reads Supabase.instance's
//    current session itself, and throws StateError if you have not signed in.
final db = await NostosDatabase.supabase(
  nostosUrl: 'ws://127.0.0.1:8800/sync',
  sqlitePath: '$dir/nostos.db',
);

// 3. Config-driven, from `nostos pull && nostos gen`. What example/ uses.
final db = await NostosDatabase.open(
  config: config,            // NostosConfig; a supabase block here is honoured
  schema: nostosSchema,       // generated
  sqliteDir: dir.path,       // note: DIR, not a file path
);
```

`sqlitePath` is **required** on `connect`/`supabase`. `open` derives it from
`sqliteDir` + the config's filename.

## Reading — typed `Collection<T>` (the taught surface)

`lib/src/nostos_database.dart` (ADR-0024 / ADR-0032 T2). `toRow` is only needed
for typed `upsert`. Reads run over **SQLite views on `nostos_data`** (ADR-0028) —
never materialized typed tables.

```dart
final todos = db.collection<Todo>(
  table: 'todos', fromRow: Todo.fromRow, toRow: (t) => t.toRow());

// Structured where/orderBy — data, not SQL fragments (injection-safe).
final Stream<List<Todo>> active = todos.watch(
  where: Where.and([Where.eq('user_id', 'u1'), Where.eq('completed', 0)]),
  orderBy: [Order.desc('created_at')],
  limit: 50,
);
final Future<List<Todo>> page = todos.getAll(
  where: Where.eq('completed', 0), orderBy: [Order.asc('_pk')], limit: 20, offset: 40);

final Future<Todo?> one = todos.get('7');        // one-shot; fetchById is an alias
final Stream<Todo?> detail = todos.watchOne('7'); // detail screen — no list churn
final Stream<int> badge = todos.count(where: Where.eq('completed', 0));
final Stream<bool> any = todos.exists(where: Where.eq('completed', 0));
```

**Predicate operators v1** (`lib/src/predicate.dart`): `Where.eq/neq/lt/lte/gt/gte`,
`Where.inList(col, [...])`, `Where.isNull/notNull`, and the combinators
`Where.and([...])` / `Where.or([...])` / `Where.not(p)`. `Order.asc(field)` /
`Order.desc(field)`. Column names are identifier-validated and values are emitted
as safe SQLite literals — nothing the caller supplies is spliced raw.

| Member | Signature |
|---|---|
| `watch` | `Stream<List<T>> watch({Where? where, List<Order>? orderBy, int? limit, int? offset, Duration? throttle})` |
| `getAll` | `Future<List<T>> getAll({Where? where, List<Order>? orderBy, int? limit, int? offset})` |
| `get` / `fetchById` | `Future<T?> get(Object pk)` / `fetchById(Object pk)` (alias) |
| `watchOne` | `Stream<T?> watchOne(Object pk)` |
| `count` | `Stream<int> count({Where? where})` |
| `exists` | `Stream<bool> exists({Where? where})` |
| `upsert` | `Future<int> upsert(T value)` — needs `toRow` |
| `upsertRow` | `Future<int> upsertRow(Map<String, dynamic> row)` |
| `patch` | `Future<int> patch(Object pk, Map<String, dynamic> columns)` — canonical per-field LWW (ADR-0014) |
| `delete` | `Future<int> delete(Object pk)` |
| `orSetAdd` | `Future<int> orSetAdd({required Object pk, required String element})` — add-wins merge (ADR-0030/T4) |
| `orSetRemove` | `Future<int> orSetRemove({required Object pk, required String element})` — tombstone, add-wins |
| `counterIncrement` | `Future<int> counterIncrement({required Object pk, required int delta})` — PN-Counter merge (ADR-0030 addendum) |
| `counterDecrement` | `Future<int> counterDecrement({required Object pk, required int delta})` — PN-Counter, bumps negative counter |
| `writeBatch` | `Future<List<int>> writeBatch(List<NostosWrite> writes)` — single-table convenience; stamps this table |

> **Three write semantics — pick by conflict model:**
>
> | Method | Semantics | Use when |
> |--------|-----------|----------|
> | `patch`/`upsert` | **Last-writer-wins** (ADR-0014) | A concurrent write should replace the prior value (normal fields) |
> | `counterIncrement`/`counterDecrement` | **PN-Counter merge** (ADR-0030 addendum) | Concurrent increments from different replicas must all count (likes, scores, tallies) |
> | `orSetAdd`/`orSetRemove` | **OR-set add-wins merge** (ADR-0030/T4) | Concurrent adds of different elements must both survive (tags, collaborators, reactions) |
>
> `patch` and `upsert` are per-field **last-writer-wins** — a concurrent write
> clobbers the prior value. `counterIncrement`/`counterDecrement` target a table
> the server tags as a PN-Counter and **merge** per-replica elementwise max: an
> offline increment survives a server frame on the same row, and concurrent
> increments from different replicas all count. `orSetAdd`/`orSetRemove` target a
> column the server tags as an OR-set and **merge**: concurrent adds of different
> elements both survive, and a remove is a tombstone a concurrent or later re-add
> revives (add-wins). The server-authoritative `WriteOp::Increment` (ADR-0030 D1)
> remains for server-serialized counters that don't need offline convergence.
>
> **Declaring CRDT tables.** A table must be tagged before its first `counter*`/
> `orSet*` verb — without it the verb throws `*TableNotTagged` and writes clobber
> instead of merge. Declare it once at open: `NostosDatabase.connect` / `.open` /
> `.supabase` take `counterTables` and `orSetTables` sets
> (e.g. `counterTables: {'likes'}, orSetTables: {'tags'}`). These MUST match the
> server's `NOSTOS_COUNTER_COLUMNS` / `NOSTOS_OR_SET_COLUMNS` ("three-views-of-one-
> truth": the client verb-gate, the client apply-merge, and the server must
> agree), or client-merge and server-clobber will diverge.

## Writing — durable collapsed outbox (ADR-0013)

All writes go through the durable outbox and return the **local outbox id**, NOT
a server ack — the applied row round-trips back through `watch`. `op` is
`"upsert"`, `"delete"`, or `"patch"`.

```dart
await db.write(table: 'todos', op: 'patch', pk: '7', payload: {'completed': 1});
```

### `writeBatch` — all-or-nothing *entry* (ADR-0032 T3)

```dart
await db.writeBatch([
  NostosWrite(table: 'orders', op: 'upsert', pk: o.id, payload: orderWritePayload(o)),
  NostosWrite(table: 'cart_items', op: 'delete', pk: 'c1'),
  NostosWrite(table: 'cart_items', op: 'delete', pk: 'c2'),
]);
```

> **`writeBatch` is NOT a server transaction.** The server applies each row
> individually with per-field LWW; there is no cross-row rollback and no
> all-or-nothing *apply*. Two ops touching the same row/field collapse to the
> last value. **Entry atomicity IS real** — all ops land in one SQLite
> transaction or none do; a mid-batch failure rolls back the whole batch and
> leaves zero partial outbox rows.

## Lifecycle — pause/resume/auth

| Member | Signature | Notes |
|---|---|---|
| `subscribe` | `Future<void> subscribe(String table, {String? where})` | starts the socket + run loop. `where` is the server-compiled safe-SQL predicate (ADR-0012) |
| `subscribeTables` | `Future<void> subscribeTables(List<NostosTableSub>)` | multiplexes many tables over **one** socket (ADR-0022); replaces the active set |
| `pauseSync` / `resumeSync` | `Future<void>` / `void` | ADR-0032 canonical pause/resume — retain token, schema, and watches; watches re-emit on resume. (`disconnect`/`resume` are back-compat aliases) |
| `waitForFirstSync` | `Future<void>` | completes once the first sync has landed; resolves immediately if already synced (ADR-0032 T1) |
| `setToken` | `Future<void> setToken(String? token)` | live credential swap — **never reconnect to refresh** |
| `signOut` | `Future<void>` | disconnect + **wipe** local data (ADR-0029) |
| `close` | `Future<void>` | release resources (keeps local data) |
| `schema` | `NostosSchema` | the resolved schema the read-views were built from |

**Use `setToken`, never a re-connect.** It swaps the credential on the live
client so the next connection uses it — nothing is torn down and open `watch`
streams keep flowing. `NostosDatabase.supabase` wires `onAuthStateChange` →
`setToken` for you (since 2026-07-30), so rotated tokens self-heal within one
backoff window instead of dying an hour after sign-in.

## `SyncStatus` + write-outcome observability

`ValueListenable<SyncStatus> get status` (hot). Connection state folded with the
durable outbox (ADR-0027 / ADR-0032 T5).

| Member | Type / Notes |
|---|---|
| `conn` / `connected` / `hasSynced` / `lastSyncedAt` | `NostosConnectionState` / `bool` / `bool` / `DateTime?` |
| `pendingWrites` | `int` — drains as writes land. `> 0` is healthy offline |
| `deadLetteredWrites` | `int` — **never decreases**; permanently failed |
| `lastWriteError` | `String?` — server's reason for the most recent permanent failure |
| `hasWriteError` / `hasPendingWrites` / `uploading` | `bool` |
| `webStorageDegraded` | `bool` — web-only (ADR-0036): OPFS unavailable, fell back to memory. Always `false` on native |

`db.deadLetters()` → `Future<List<DeadLetter>>` lists the quarantined rows (id,
table, op, pk, attempts, payload, error, timestamp) so failures are diagnosable.
Each `DeadLetter` carries the server's per-row `error` and a `timestamp` of when
the flush loop quarantined it (persisted via the `last_error`/`dead_lettered_at`
outbox columns). v1 is read-only; `retryDeadLetter(id)` / `discardDeadLetter(id)`
are deferred to v1.1.

## Schema types

`lib/src/schema.dart`. `Table` and `Column` are **deliberately not re-exported** — they collide
with `material.dart`'s widgets. Use the aliases:

```dart
const schema = NostosSchema(tables: [
  NostosTable(name: 'tasks', columns: [NostosColumn.text('title'), NostosColumn.integer('done')],
             primaryKey: ['id']),
]);
```

`NostosColumn({required name, affinity, pgOid})` plus `.text(name)` / `.integer(name)` shorthands.

## Escape hatch — raw SQL (last resort)

For queries the typed surface cannot express yet (an `(col IS NULL) DESC` order,
a join, a projection), `NostosDatabase` exposes raw-SQL reads over the same
views. **Prefer `Collection<T>` + `Where`/`Order` for every "table, maybe filter,
maybe order" read** — it is injection-safe by construction.

| Member | Signature | Notes |
|---|---|---|
| `watchSql` | `Stream<List<Map<String, dynamic>>> watchSql(String sql, {Duration? throttle})` | reactive; re-emits after every applied change. Hot, replay-shared per query |
| `getAll` | `Future<List<Map<String, dynamic>>> getAll(String sql)` | one-shot raw read |
| `execute` | `Future<List<Map<String, dynamic>>> execute(String sql)` | **read-only alias of `getAll`** — see warning |
| `watch` (String) | `Stream<List<Map<String, dynamic>>> watch(String sql, …)` | back-compat alias of `watchSql` |

> **`execute` does not write.** It is an alias of `getAll`, by convention and **not** by
> enforcement — nothing parses your SQL. Statements aimed at a synced table fail loudly (the read
> surface is a VIEW), but `DELETE FROM nostos_outbox` would silently destroy queued writes. Route
> every mutation through `write` or a `Collection`.

Reads run against **one SQLite VIEW per synced table**, projected from the server schema
(ADR-0028). The view is named after the table (a `public.` prefix is stripped), the replication
key is exposed as `_pk`, and columns come from `json_extract` over the stored payload. A slow
`WHERE col = ?` is fixed with a partial expression index on `nostos_data` — **not** by
materializing tables (ADR-0028 has the measurement). Columns have no SQLite *affinity* (a
timestamp arriving as a JSON string sorts lexicographically — fine for ISO-8601); a
non-`public` Postgres schema is **untested** against the view naming.

## Attachments — two-plane blob sync (T6 / ADR-0034)

Blobs (images, files) **never transit the Nostos server** — that would pollute the fan-out
throughput that is Nostos's headline advantage and make the server stateful. Instead two planes:

- a **metadata plane** — an ordinary synced `attachments` table (`id, filename, size, media_type,
  state, timestamp`) — synced through replication + the collapsed outbox like any business table; and
- a **blob plane** — a developer-supplied `AttachmentStorageAdapter` (your Supabase Storage / S3 /
  … bucket) plus a local blob cache.

### Setup — `NOSTOS_WRITE_TABLES` (the #1 foot-gun)

The metadata table is writable through the same collapsed outbox as any business table, so the
server's empty-default allowlist **MUST include `attachments`** (ADR-0013). A forgotten entry
surfaces loudly at the transport:

```
table not writable: 'attachments' — add it to NOSTOS_WRITE_TABLES
```

```bash
export NOSTOS_WRITE_TABLES=attachments,tasks,…   # comma-separated; empty by default
```

The app also declares `attachments` in its `NostosSchema` (it is a normal table).

### API

```dart
import 'package:nostos_flutter/nostos_flutter.dart';
// SupabaseStorageAdapter pulls in supabase_flutter; LocalFileBlobStore uses path_provider.

final attachments = db.attachments(
  adapter: SupabaseStorageAdapter(bucket: 'uploads'),   // or your own AttachmentStorageAdapter
  blobStore: LocalFileBlobStore(Directory('${dir.path}/nostos_blobs')),
  maxAttempts: 5,                                        // default; → archived after
);

// Pick a blob OFFLINE → cached locally + a queued_upload metadata row enqueued.
final id = await attachments.queueUpload(
  filename: 'photo.png', bytes: bytes, mediaType: 'image/png',
);

// Reconnect → the driver uploads to the bucket and flips state → synced.
attachments.start();   // self-driving 2s tick; or call pump() yourself / wire to connectivity.

// A second client receives the synced metadata row via replication, then:
await attachments.queueDownload(id);   // fetches bytes into its local cache on the next pump.

await attachments.remove(id);          // queued_delete → archived (blob gone, metadata retained).
```

| Member | Signature | Notes |
|---|---|---|
| `db.attachments` | `Attachments attachments({required adapter, required blobStore, required isOnline, maxAttempts, clock})` | constructs the driver + registers `blobStore.wipe` as a sign-out hook (ADR-0029) |
| `queueUpload` | `Future<String> queueUpload({required filename, required bytes, required mediaType, id})` | caches bytes locally, upserts a `queued_upload` row; returns the attachment id |
| `queueDownload` | `Future<void> queueDownload(String id)` | flips an existing synced row to `queued_download` |
| `remove` | `Future<void> remove(String id)` | flips to `queued_delete`; the driver deletes from the bucket |
| `pump` | `Future<void> pump()` | one driver tick: when online, reads queued rows + dispatches blob ops |
| `start` / `stop` | `void start()` / `void stop()` | self-driving 2s timer that calls `pump()` (start also pumps on connect) |
| `lastErrorFor` | `String? lastErrorFor(String id)` | the last adapter error (dead-letter reason); local-only, not synced |

`AttachmentStorageAdapter` (`upload(path, bytes, mediaType)` / `download(path)` / `delete(path)`) is
abstract; methods MUST be idempotent under retry (`delete` on a missing path is success). A
first-class `SupabaseStorageAdapter` ships (`upsert: true` for upload idempotency; `not-found` on
delete is swallowed). `BlobStore` (`put`/`get`/`remove`/`wipe`) is the local cache;
`LocalFileBlobStore` is the filesystem-backed impl (the app supplies the `Directory` —
`nostos_flutter` deliberately does **not** depend on `path_provider`).

### State machine + dead-letter

```
queued_upload   ──ok──►  synced
queued_download ──ok──►  synced
queued_delete   ──ok──►  archived      (blob gone; metadata tombstone stays)
any queued_*    ──retries exhausted──►  archived   (dead-letter; ADR-0027 parity)
synced | archived  ──►  terminal
```

Failed adapter calls retry on exponential backoff (1s, 2s, 4s … capped 60s). After `maxAttempts`
the row flips to `archived` and the error is surfaced via `lastErrorFor`. The attempt count is
**driver-local, not a synced column** — a process restart resets it, which is fine because the
metadata row stays `queued_*` across a restart (its state IS synced) and the driver retries fresh.

### Ordering

Within a row, `state = synced` is reached **only after** the blob confirms — that is structural.
Cross-row ordering ("a `tasks` row referencing `attachment_id` must not land until the blob is
uploaded") is **not enforced**; the app gates the referencing UI by reading `state` reactively
(`watch`). Strong cross-row ordering would need an outbox dependency mechanism — see ADR-0034 §3
for the boundary + upgrade path.

## Flutter-web (ADR-0036)

`nostos_flutter` targets the web. `Nostos.connect` / `NostosDatabase.open` work
unchanged — engine selection is a **compile-time conditional import**
(`engine_selector.dart`): native builds drive `RustNostosEngine` (frb + the Rust
dylib + `path_provider`); web builds drive `WebNostosEngine`, which talks to the
**shared `nostos-ffi-wasm` backend** (`@nostos-sync/web`'s backend — ADR-0035) over a
durable-storage Worker. So Flutter-web inherits opfs-sahpool durability
(ADR-0033), NOT the rejected `frb_generated.web.dart` path (which would compile
rusqlite to wasm and strand the app on an in-memory backend).

**Bootstrap (web assets).** Ship these in your app's `web/nostos/` directory
(reference copies live in `sdk/nostos_flutter/web/nostos/`):

- `nostos_worker.js` — the Worker host (loads wasm + sqlite-wasm, owns the live
  `NostosSocket`, speaks `WebNostosEngine`'s protocol).
- `sqlite_wasm_glue.js` — the opfs-sahpool VFS wrapper (copied verbatim from
  `sdk/nostos_web/worker/`).
- `nostos_ffi_wasm.js` + `nostos_ffi_wasm_bg.wasm` — the wasm artifact; produce
  with `wasm-pack build crates/nostos-ffi-wasm --target web --out-dir pkg-web`
  and copy `pkg-web/nostos_ffi_wasm.{js,wasm}` into `web/nostos/`.

Override the Worker URL with `Nostos.connect(url: ..., workerUrl:
'assets/nostos/nostos_worker.js')` if your asset layout differs. The default is
`nostos/nostos_worker.js`.

**Safari Private Browsing.** When OPFS is unavailable the Worker degrades to
in-memory storage and `SyncStatus.webStorageDegraded` flips `true` — surface a
"session not persisted" banner (rows + outbox will not survive a reload).

**CRDT + writeBatch on web (ADR-0036).** The four CRDT verbs and atomic
`writeBatch` work on web: the Flutter-web Worker drives `NostosSocket` delegates
(added Wave 4c) that reuse the in-process `NostosEngine`'s CRDT/HLC logic and
`enqueue_batch` atomicity — no CRDT algebra is re-implemented in the wasm crate.
CRDT tables declared via `counterTables` / `orSetTables` at `connect` / `open` /
`supabase` are forwarded into the `connect` Worker message, which re-tags on
every (re)connect. (The earlier gap — CRDT verbs throwing `UnsupportedError` on
web — is closed.) Merge correctness is proven by the `nostos-ffi-wasm` host tests
+ the Flutter-web Playwright smoke; native and web share the same `nostos-domain`
CRDT invariants.

## Proven by

`sdk-e2e` `flutter` slice: a real `cargo run -p nostos-server` spine driven through
connect/subscribe/watch inside a genuine app bundle (`-d macos`, since the server binds
loopback).

`sdk/nostos_flutter/test/attachments_test.dart` (ADR-0034): the full
queue→offline→reconnect→upload→second-client-download→dead-letter→sign-out-wipe path against a
shared in-memory fake adapter. **The real Supabase-Storage round-trip is untested-environment
here** — no Supabase project is configured in the dev tree; the `SupabaseStorageAdapter` code path
compiles against `supabase_flutter` but no live bucket upload/download is exercised. Run that
round-trip against a configured project before shipping.
