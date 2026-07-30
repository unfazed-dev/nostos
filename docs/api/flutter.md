# Flutter / Dart — `nostos_flutter`

Extracted from `sdk/nostos_flutter/lib/` on 2026-07-30. Index: [`README.md`](README.md).

The richest of the SDKs: reactive `Stream`s, typed collections, and a sync-status signal.

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

`sqlitePath` is **required** on `connect`/`supabase` (unlike the old `Nostos.connect`, where it was
optional). `open` derives it from `sqliteDir` + the config's filename.

## Methods

| Member | Signature | Notes |
|---|---|---|
| `subscribe` | `Future<void> subscribe(String table, {String? where})` | starts the socket + run loop. `where` is the server-compiled safe-SQL predicate (ADR-0012) |
| `subscribeTables` | `Future<void> subscribeTables(List<NostosTableSub>)` | multiplexes many tables over **one** socket (ADR-0022). Calling either again *replaces* the active set |
| `watch` | `Stream<List<Map<String, dynamic>>> watch(String sql, {Duration? throttle})` | reactive; re-emits after every applied change. Hot, replay-shared per query |
| `getAll` | `Future<List<Map<String, dynamic>>> getAll(String sql)` | one-shot read |
| `execute` | `Future<List<Map<String, dynamic>>> execute(String sql)` | **read-only alias of `getAll`** — see the warning below |
| `write` | `Future<int> write({required String table, required String op, required String pk, Map<String, dynamic>? payload})` | returns the local outbox id |
| `collection<T>` | `Collection<T> collection<T>({required String table, required T Function(Map) fromRow, Map Function(T)? toRow, String pkColumn = 'id'})` | the typed surface, below |
| `status` | `ValueListenable<SyncStatus>` | hot sync status |
| `disconnect` / `resume` / `close` | `Future<void>` / `void` / `Future<void>` | pause, resume, tear down |
| `schema` | `NostosSchema` | the resolved schema the read-views were built from |

> **`execute` does not write.** It is an alias of `getAll`, by convention and **not** by
> enforcement — nothing parses your SQL. Statements aimed at a synced table fail loudly (the read
> surface is a VIEW), but `DELETE FROM cairn_outbox` would silently destroy queued writes. Route
> every mutation through `write` or a `Collection`.

## Reading — SQL over views

Reads run against **one SQLite VIEW per synced table**, projected from the server schema
(ADR-0028). The view is named after the table (a `public.` prefix is stripped), the replication
key is exposed as `_pk`, and columns come from `json_extract` over the stored payload:

```dart
db.watch('SELECT * FROM tasks WHERE completed = 0 ORDER BY _pk').listen(render);
```

A slow `WHERE col = ?` is fixed with a partial expression index on `cairn_data` — **not** by
materializing tables. ADR-0028 has the measurement.

Two consequences worth knowing: columns have no SQLite *affinity* (a timestamp arriving as a JSON
string sorts lexicographically — fine for ISO-8601), and a **non-`public` Postgres schema is
untested** against the view naming.

## Typed reads/writes — `Collection<T>`

`lib/src/nostos_database.dart:462` (ADR-0024). `toRow` is only needed if you call `upsert`.

```dart
final todos = db.collection<Todo>(
  table: 'todos', fromRow: Todo.fromRow, toRow: (t) => t.toRow());

final Stream<List<Todo>> active = todos.watch(where: 'completed = 0', orderBy: 'created_at DESC');
final Stream<int> open = todos.count(where: 'completed = 0');

await todos.upsert(Todo(id: '1', title: 'ship', completed: false));
await todos.patch('1', {'completed': true});   // per-field LWW
await todos.delete('1');
```

| Member | Signature |
|---|---|
| `watch` | `Stream<List<T>> watch({String? where, String? orderBy})` |
| `count` | `Stream<int> count({String? where})` |
| `upsert` | `Future<int> upsert(T value)` — needs `toRow` |
| `upsertRow` | `Future<int> upsertRow(Map<String, dynamic> row)` |
| `patch` | `Future<int> patch(Object pk, Map<String, dynamic> columns)` |
| `delete` | `Future<int> delete(Object pk)` |

## `SyncStatus`

`lib/src/nostos_database.dart:565`. Connection state folded together with the durable outbox
(ADR-0027).

| Member | Type |
|---|---|
| `conn` | `NostosConnectionState` |
| `connected` / `hasSynced` / `lastSyncedAt` | `bool` / `bool` / `DateTime?` |
| `pendingWrites` | `int` — drains as writes land |
| `deadLetteredWrites` | `int` — **never decreases**; these are permanently failed |
| `lastWriteError` | `String?` |
| `hasWriteError` / `hasPendingWrites` / `uploading` | `bool` |

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

## ⚠️ Token refresh — open P1

**Sync stops when the access token expires** — about an hour after login on Supabase's default
TTL — **and does not recover.** The token is fixed at connect: `ClientConfig.token` is immutable,
the reconnect loop re-sends it every attempt, and the server enforces `exp`. Listen to
`onAuthStateChange` and re-connect with the new token on `tokenRefreshed`. Tracked with its agreed
fix in [`../plans/nostos-flutter-powersync-connection-redesign.md`](../plans/nostos-flutter-powersync-connection-redesign.md).

## Proven by

`sdk-e2e` `flutter` slice: a real `cargo run -p nostos-server` spine driven through
connect/subscribe/watch inside a genuine app bundle (`-d macos`, since the server binds
loopback). Also runs the doc-signature check that guards this page.
