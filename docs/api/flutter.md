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
  sqlitePath: '$dir/cairn.db',
);

// 2. Supabase — takes NO Supabase arguments. It reads Supabase.instance's
//    current session itself, and throws StateError if you have not signed in.
final db = await NostosDatabase.supabase(
  nostosUrl: 'ws://127.0.0.1:8800/sync',
  sqlitePath: '$dir/cairn.db',
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
for typed `upsert`. Reads run over **SQLite views on `cairn_data`** (ADR-0028) —
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
| `writeBatch` | `Future<List<int>> writeBatch(List<NostosWrite> writes)` — single-table convenience; stamps this table |

> **`patch`/`upsert` vs `orSetAdd`/`orSetRemove`:** `patch` and `upsert` are
> per-field **last-writer-wins** (ADR-0014) — a concurrent write clobbers the
> prior value. `orSetAdd`/`orSetRemove` target a column the server tags as an
> OR-set and **merge**: concurrent adds of different elements both survive, and
> a remove is a tombstone a concurrent or later re-add revives (add-wins). Use
> OR-set handles for multi-value fields (tags, collaborators, reactions) where
> LWW would silently drop concurrent additions. Counters are server-authoritative
> (`WriteOp::Increment`, ADR-0030 D1) — there is no offline counter handle; patch
> the absolute value once the server echoes it.

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
> surface is a VIEW), but `DELETE FROM cairn_outbox` would silently destroy queued writes. Route
> every mutation through `write` or a `Collection`.

Reads run against **one SQLite VIEW per synced table**, projected from the server schema
(ADR-0028). The view is named after the table (a `public.` prefix is stripped), the replication
key is exposed as `_pk`, and columns come from `json_extract` over the stored payload. A slow
`WHERE col = ?` is fixed with a partial expression index on `cairn_data` — **not** by
materializing tables (ADR-0028 has the measurement). Columns have no SQLite *affinity* (a
timestamp arriving as a JSON string sorts lexicographically — fine for ISO-8601); a
non-`public` Postgres schema is **untested** against the view naming.

## Proven by

`sdk-e2e` `flutter` slice: a real `cargo run -p nostos-server` spine driven through
connect/subscribe/watch inside a genuine app bundle (`-d macos`, since the server binds
loopback).
