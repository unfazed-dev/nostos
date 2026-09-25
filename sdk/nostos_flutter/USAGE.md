# Using Nostos in a Flutter app

End-to-end: from the Nostos landing page, through Nostos Cloud auth, to a
Flutter app doing CRUD (and more) against the Nostos API.

> **API surface here is verified against `lib/` as of 2026-07-20.** Signatures,
> op names, and the config shape are read from the shipped code — not paraphrased
> from memory. The working app at `fixtures/flutter/todo` is the canonical
> consumer example (referenced below).

---

## 1. What Nostos gives you

Nostos is a Rust-native, local-first sync engine. In a Flutter app you get:

- **Offline-first SQLite** — every read hits a local SQLite file (Rust-owned,
  via `flutter_rust_bridge`). The UI never blocks on the network.
- **A WebSocket sync loop** — Postgres logical replication → `nostos-server` →
  your device. Changes stream in; you subscribe to tables and react.
- **A durable write outbox** — writes land locally first, then replay to the
  server on reconnect. No `uploadData` toll-booth (ADR-0013).
- **A typed reactive facade** — `Collection<T>` + `NostosDatabase` (ADR-0024):
  declare a schema, connect, and read/write typed records.

There is **no connector class** and **no client-side schema artifact** to
maintain by hand. `subscribe` sets the server-side predicate; `watch` gives you
a reactive `Stream` of rows; `write` is a durable local outbox.

---

## 2. From the landing page (not yet live)

<!-- NOSTOS-IDENTITY-PENDING: no domain is registered — docs/IDENTITY.md. This
     section read "the marketing site at nostos.run … is the front door", present
     tense, for a site that does not exist at an unregistered domain. -->

The marketing site (the `web/` SvelteKit app — "The Nostos Field" identity,
ADR-0008) is **planned as** the front door; **no domain is registered yet**, so
this section describes the intended flow, not something you can visit today.
From there you would choose one of two paths:

### Path A — Nostos Cloud (managed)

Sign up → create a project. Nostos Cloud (the `nostos-cloud` control plane:
axum + rusqlite, ADR-0006) provisions, per project:

- a **`nostos-server` `/sync` endpoint** (`wss://sync.<your-project>.nostos.app/sync`),
- a linked **Supabase project** (auth + Postgres), and
- an **API key + HMAC-signed license** (`<payload>.<sig>`) carrying your tier
  and device cap (`nostos-domain::Tier`: Hobby / Pro / Scale / Enterprise).

You will leave the landing page with: your sync URL, your Supabase project URL
+ anon/publishable key, and your license token. Those four values go into the
app's `nostos.json` (Section 5).

### Path B — Self-host (OSS, Apache-2.0)

Run `nostos-server` yourself. For local dev the zero-setup default is fine
(`NOSTOS_REPLICATOR=fake`, `NOSTOS_SYNC_AUTH=none`); for real data point it at
your own Postgres. You keep the landing page open only for docs. See
`docs/OPERATING.md` for the server env vars and failure modes.

Either path lands you at the same Flutter API below.

---

## 3. Auth: getting a token the SDK can present

Nostos's `/sync` WebSocket authenticates with a **bearer token** passed as
`?token=` on the WS handshake. What that token *is* depends on the server's
`NOSTOS_SYNC_AUTH` mode (ADR-0010):

| `NOSTOS_SYNC_AUTH` | Token | Tenant isolation | Use it when |
|---|---|---|---|
| `none` | ignored (anonymous) | **none — single-tenant only** | local dev / OSS single-user |
| `supabase-jwt` | a Supabase **access JWT** (HS256-verified with `NOSTOS_SUPABASE_JWT_SECRET`) | **yes** — the JWT `sub` claim is the user/tenant id (ADR-0011) | managed Cloud, any multi-user app |

### The managed/Cloud path (recommended for real apps)

Your users authenticate with **Supabase** (email/password, magic link, OAuth —
whatever GoTrue gives you). The Supabase session's **access token** is then
handed to Nostos as the bearer token. Nostos verifies it with the same secret
Supabase signed it with, reads `sub` as the tenant, and isolates that user's
rows.

```dart
// 1. The user signs in via supabase_flutter (your login UI).
await Supabase.instance.client.auth
    .signInWithPassword(email: 'ada@example.com', password: '••••');

// 2. Grab the access token — this is what Nostos will present at /sync.
final session = Supabase.instance.client.auth.currentSession!;
final nostosToken = session.accessToken;
```

You usually do **not** pass that token by hand — `NostosDatabase.supabase(...)`
and `NostosDatabase.open(config: ...)` read the current Supabase session for you
(Section 6).

### The dev / self-host path

With `NOSTOS_SYNC_AUTH=none`, pass any non-empty token (or none). To exercise
the `supabase-jwt` path locally without Supabase, mint an HS256 JWT signed
with your dev `NOSTOS_SUPABASE_JWT_SECRET` and a `sub` claim of your choosing:

```
header: {"alg":"HS256","typ":"JWT"}
payload: {"sub":"user-a"}        # ← becomes the tenant id
```

Sign with the same secret the server verifies with. (Do **not** ship a real
signing secret in a client; this is for local testing only.)

> **v1 fast-follow (honest):** token auto-refresh on rotation is not yet
> transparent. Long-lived sessions whose Supabase token rotates mid-flight will
> eventually hit 401s. Until the fast-follow lands, subscribe to
> `onAuthStateChange` and re-`connect`/re-`subscribe` on `tokenRefreshed`.

---

## 4. Install

**Published (once W6 ships to pub.dev):**
```yaml
dependencies:
  nostos_flutter: ^0.1.0
```

**Pre-publish (path or git, today):**
```yaml
dependencies:
  nostos_flutter:
    path: ../../sdk/nostos_flutter        # adjust to your checkout
    # git:
    #   url: https://github.com/unfazed-dev/nostos
    #   path: sdk/nostos_flutter
```

The package ships its Rust core as a prebuilt native library via
`flutter_rust_bridge`'s native-assets hook (`hook/`) — no toolchain setup
required for consumers.

---

## 5. Configure: `nostos.json` + your schema

### `assets/nostos.json`

Bundled with the app and loaded by `NostosConfig.load()`. Keys (verified in
`NostosConfig.fromJson`):

```json
{
  "url": "wss://sync.<your-project>.nostos.app/sync",
  "supabase": {
    "url": "https://<project-ref>.supabase.co",
    "anon_key": "YOUR_SUPABASE_ANON_OR_PUBLISHABLE_KEY"
  },
  "sqlite_filename": "nostos.sqlite"
}
```

- `url` **(required)** — the `nostos-server` `/sync` URL: `ws://`/`wss://` by
  default, or an `iroh://…?ticket=…` dial URL (the server prints it at boot)
  to sync over the iroh/QUIC transport — see the transports note below.
- `supabase` *(optional)* — object with `url` + `anon_key` (or its successor
  name `publishable_key`). Present this block and `NostosDatabase.open` will
  initialize Supabase and use the signed-in session's access token as the sync
  bearer token.
- `sqlite_filename` *(optional, default `nostos.sqlite`)* — joined onto the
  `sqliteDir` you pass at connect time.

**Transports (ADR-0041).** `ws`/`wss` is the default everywhere. `iroh://`
is a preview: the native library must be built from source with the `iroh`
cargo feature — prebuilt binaries never carry it, and shipped artifacts stay
off-default until ADR-0041's field-leg condition clears. To opt in on a
source build, set `NOSTOS_FLUTTER_CARGO_FEATURES=iroh` in the build
environment (the build hook forwards it to `cargo build --features`);
without the feature an `iroh://` URL fails loudly with a named error. The
server side is `NOSTOS_TRANSPORT=iroh` (plus `NOSTOS_IROH_RELAY_URL` if you
self-host the relay) — see `docs/OPERATING.md` §9.

Register it under `flutter/assets` in your `pubspec.yaml`:
```yaml
flutter:
  assets:
    - assets/nostos.json
```

### Declare your schema

A declared `NostosSchema` **is** the migration story: every connect re-applies
it (read-views are dropped + recreated server-side, `SqliteStorage::apply_schema`).
Adding a column = adding a `NostosColumn`; no migration files, no version
counters. The row payloads under `nostos_data` are schema-less JSON, so only the
view shape changes (ADR-0019).

```dart
import 'package:nostos_flutter/nostos_flutter.dart';

const appSchema = NostosSchema(tables: [
  NostosTable(name: 'tasks', primaryKey: ['id'], columns: [
    NostosColumn.text('id'),
    NostosColumn.text('title'),
    NostosColumn.integer('completed'),   // 0 / 1
    NostosColumn.text('user_id'),
    NostosColumn.text('created_at'),
  ]),
]);
```

> `NostosTable` / `NostosColumn` are the package's collision-free aliases — `Table`
> and `Column` would shadow `material.dart` widgets, so they are intentionally
> not re-exported under those names.

If you omit `schema` at connect time, the SDK fetches it via
`GET {http-base}/schema` and parses it for you (`NostosSchema.fromSchemaDescriptor`).

---

## 6. Connect

Four factories, one underlying engine. Pick by ergonomics:

### Recommended — `NostosDatabase.open` (config-driven)

Loads `assets/nostos.json`, resolves the schema, applies it, and (if the config
has a `supabase` block) initializes Supabase + forwards the session token.

```dart
import 'package:path_provider/path_provider.dart';

final config = await NostosConfig.load();
final dir = await getApplicationSupportDirectory();

final db = await NostosDatabase.open(
  config: config,
  schema: appSchema,
  sqliteDir: dir.path,
);

await db.subscribe('tasks');          // start the sync session for a table
```

### Supabase one-liner — `NostosDatabase.supabase`

Throws `StateError` if no Supabase session is live — sign in first (Section 3).

```dart
// Supabase.initialize(...) must already have run — this factory reads
// Supabase.instance's current session itself, so it takes no Supabase args.
final db = await NostosDatabase.supabase(
  nostosUrl: 'wss://sync.<your-project>.nostos.app/sync',
  schema: appSchema,                  // omit to fetch via GET /schema
  sqlitePath: '${dir.path}/nostos.sqlite',
);
```

> Corrected 2026-07-30: this sample previously passed `supabaseUrl:` and
> `supabaseAnonKey:` to `NostosDatabase.supabase`. **Neither parameter exists** —
> the real signature is `{nostosUrl, schema, sqlitePath}`, and it would not have
> compiled. Pass Supabase's own URL/key to `Supabase.initialize`, or use
> `NostosDatabase.open(config: …)`, whose `NostosConfig` *does* carry a
> `supabaseUrl` / `supabaseAnonKey` block — that is where the confusion came from.

### Lowest-level — `NostosDatabase.connect`

You own the URL, the token, and the SQLite path. Useful for tests, non-Supabase
auth, or a pinned schema.

```dart
final db = await NostosDatabase.connect(
  url: 'wss://sync.<your-project>.nostos.app/sync',
  token: nostosToken,                  // bearer JWT (Section 3); omit for NOSTOS_SYNC_AUTH=none
  schema: appSchema,                  // omit to fetch via GET /schema
  sqlitePath: '${dir.path}/nostos.sqlite',
);
await db.subscribe('tasks');
```

### No server — `NostosDatabase.direct` (Supabase projects)

The device talks to your Supabase project itself: PostgREST for the pull and
the push, Realtime as the doorbell, RLS decides who reads what. Nothing to
operate. Set the project up once with `nostos link --mode direct` and check it
with `nostos doctor --mode direct`; the walkthrough is
[`docs/nostos-explained.html`](../../docs/nostos-explained.html).

```dart
final user = Supabase.instance.client.auth.currentUser!;
final db = await NostosDatabase.direct(
  supabaseUrl: 'https://<ref>.supabase.co',
  anonKey: supabaseAnonKey,            // the publishable key; RLS does the gating
  scope: 'sub:${user.id}',             // what your change-log trigger stamps
  token: session.accessToken,          // rotate with db.setToken(...)
  schema: appSchema,                   // REQUIRED: there is no /schema to fetch
  sqlitePath: '${dir.path}/nostos_direct.sqlite',
  keepLocalOnSignOut: true,            // optional, see below
);
```

Everything after this line is the same API as `connect`: same outbox, same
`watch`, same offline behaviour.

**Sign-out and what stays on the device.** `db.signOut()` drops the token and,
by default, wipes the local store (ADR-0029): the next sign-in downloads a
fresh snapshot. `keepLocalOnSignOut: true` (ADR-0049) keeps the rows instead.
The store remembers the JWT `sub` it holds rows for; the same user's next
sign-in resumes from where it left off (one snapshot per install), and a
different user's token wipes before its first pull. Keep it `false` for
shared-device apps. Call `db.signOut()` **before** `supabase.auth.signOut()` so
the push-token deregistration hook still has a session.

Multi-table? Subscribe to several at once:
```dart
await db.subscribeTables(['tasks', 'projects', 'comments']);
```

---

## 7. CRUD with `Collection<T>` (the typed facade)

Construct a `Collection<T>` per table, then read/write typed records. This is
the primary API (ADR-0024) and matches what `fixtures/flutter/todo` does.

```dart
class Task {
  Task(this.id, this.title, this.completed);
  final String id;
  final String title;
  final bool completed;

  factory Task.fromRow(Map<String, dynamic> r) => Task(
    r['id'] as String,
    r['title'] as String,
    (r['completed'] as int) == 1,
  );

  Map<String, dynamic> toRow() => {
    'id': id,
    'title': title,
    'completed': completed ? 1 : 0,
  };
}

final tasks = db.collection<Task>(
  table: 'tasks',
  fromRow: Task.fromRow,
  toRow: Task.toRow,
  pkColumn: 'id',           // default; shown for clarity
);
```

### Create / full-row update — `upsert` / `upsertRow`

```dart
// typed (uses toRow)
await tasks.upsert(Task('1', 'Ship Nostos', false));

// form / map-driven (no toRow needed on the call site)
await tasks.upsertRow({'id': '1', 'title': 'Ship Nostos', 'completed': 0});
```

### Partial update — `patch` (column-level, last-write-wins, ADR-0014)

```dart
await tasks.patch('1', {'completed': 1});        // flip done
await tasks.patch('1', {'title': 'Ship Nostos ✅'});  // rename
```

### Delete — `delete`

```dart
await tasks.delete('1');
```

> **All writes return `Future<int>` = the local outbox id, NOT a server ack.**
> The applied row round-trips back through `watch()` once the server replicates
> it (ADR-0013 outbox contract). This is what makes writes work offline.

### Raw write primitive — `db.write`

`Collection`'s write methods are thin wrappers over the universal primitive:

```dart
Future<int> write({
  required String table,
  required String op,        // "upsert" | "delete" | "patch"
  required Object pk,
  Map<String, dynamic>? payload,
});
```

Reach for it directly when you don't want a `Collection<T>` (dynamic tables,
generated code, etc.). `op` must be one of those three strings; `table` must
match an active subscription (v1).

---

## 8. Reactive reads

### Typed stream — `Collection<T>.watch`

```dart
final Stream<List<Task>> active = tasks.watch(
  where: 'completed = 0',
  orderBy: 'title',
);
```

- `where` is a literal SQL fragment (e.g. `'completed = 0'`, `"user_id = 'user-a'"`).
- `orderBy` is a literal `ORDER BY` fragment (e.g. `'created_at DESC'`).
- `throttle` coalesces a burst of change ticks into one re-query per window.

The stream re-emits whenever the table's synced data changes (full re-snapshot
per tick — self-healing on lag, not a fragile diff).

### Derived count — `Collection<T>.count`

```dart
final Stream<int> openCount = tasks.count(where: 'completed = 0');
```

Use this for count badges so they don't rebuild on unrelated column writes.

### Wire it into the widget tree

```dart
StreamBuilder<List<Task>>(
  stream: tasks.watch(where: 'completed = 0', orderBy: 'title'),
  builder: (context, snap) {
    final items = snap.data ?? const <Task>[];
    return ListView(children: items.map(TaskTile.new).toList());
  },
);
```

### Raw SQL escape hatch — `db.watch` / `db.getAll`

```dart
final Stream<List<Map<String, dynamic>>> s =
    db.watch('SELECT * FROM tasks ORDER BY created_at DESC');
final List<Map<String, dynamic>> rows =
    await db.getAll('SELECT * FROM tasks LIMIT 10');
```

> **First emission waits for real data.** Until the first snapshot has
> landed, a table is empty because nothing has been read, not because there is
> nothing. `watch` withholds that read (direct mode, since 2026-09-25): the
> stream stays silent for the bootstrap (~2 s for 1k rows on an iPhone) and
> its first value is the real one. Render a spinner on `!snapshot.hasData`,
> never an empty state — see `apps/atlet/flutter/lib/ui/home.dart`.

> **v1 boundary (honest):** `db.watch(sql)` does **not** yet take a
> `parameters: [...]` list (it's P1). Until it lands, interpolate carefully or
> prefer the typed `Collection<T>.watch(where:)`. `execute(sql)` is SELECT-only
> in v1 — writes go through `write()` / the Collection methods so they enter the
> outbox rather than desyncing the local view.

---

## 9. Sync status

```dart
final ValueListenable<SyncStatus> status = db.status;
final SyncStatus now = db.currentStatus;
```

`SyncStatus` carries:

| Field | Type | Meaning |
|---|---|---|
| `conn` | `NostosConnectionState` | `connecting / connected / reconnecting / disconnected` |
| `connected` | `bool` | convenience for `conn == connected` |
| `lastSyncedAt` | `DateTime?` | best-effort: stamped on each `connected` transition |
| `hasSynced` | `bool` | has synced at least once — tells "nothing synced yet" from "no data" |
| `pendingWrites` | `int` | writes captured locally, not yet ack'd by the server |
| `hasPendingWrites` | `bool` | `pendingWrites > 0` |
| `uploading` | `bool` | connected with writes still draining |
| `deadLetteredWrites` | `int` | writes that **permanently failed** this session |
| `lastWriteError` | `String?` | the server's message for the last permanent failure |
| `hasWriteError` | `bool` | `lastWriteError != null` |

### Pending is not an error

`pendingWrites > 0` while offline is the offline-first promise working. Show it
as "N unsynced changes".

`lastWriteError` is different: it is set **only** when a write has permanently
failed and left the send queue. Ordinary server rejections are frequently
transient and retry on their own, so they deliberately do not set it — surfacing
those would train users to dismiss write errors. When `hasWriteError` is true, a
write is genuinely lost and a human should be told. The message is the server's
verbatim reason and is usually actionable (a write-allowlist rejection, for
example, names the exact env var to set).

Banner widget:
```dart
ListenableBuilder(
  listenable: db.status,
  builder: (context, _) {
    final s = db.currentStatus;
    if (s.hasWriteError) {
      return Text('Change not saved: ${s.lastWriteError}');
    }
    if (s.hasPendingWrites) {
      return Text('${s.pendingWrites} unsynced change'
          '${s.pendingWrites == 1 ? '' : 's'}');
    }
    return Text(s.connected
        ? 'Synced${s.lastSyncedAt == null ? '' : ' · ${s.lastSyncedAt}'}'
        : 'Offline — changes queued');
  },
);
```

This is what makes Flutter's own optimistic-state pattern expressible on Nostos:
`db.write` returns as soon as the write is durable *locally*, so there is no
`catch` to revert in — `hasWriteError` is the signal that a previously-accepted
write did not survive the server.

For raw streams (e.g. non-Flutter logic), use `db.connectionState` →
`Stream<NostosConnectionState>`, or `nostos.writeStatus` →
`Stream<({int pending, int deadLettered, String? lastError})>`.

---

## 10. More

### Multi-table

Subscribe to many tables, hold one `Collection<T>` per table:
```dart
await db.subscribeTables(['tasks', 'projects', 'comments']);
final tasks = db.collection<Task>(table: 'tasks', fromRow: ..., toRow: ...);
final projects = db.collection<Project>(table: 'projects', fromRow: ..., toRow: ...);
```

### Schema migrations

There are none to write. Edit `appSchema`, restart, done — the next connect
re-applies it (views are dropped + recreated). Adding a column surfaces it from
already-synced payloads on next launch; removing one drops it from the view.
See `NostosSchema`'s class doc.

### Offline + the outbox

- **Reads** always hit local SQLite — the UI is fully functional offline.
- **Writes** are captured in a durable local outbox first, then replayed to the
  server when the connection returns. The applied row arrives back through
  `watch()` like any other replicated change (ADR-0013).
- Reconnect/replay after a dropped session is handled by the engine (ADR-0025:
  resume-info epoch + snapshot reconcile).

### Per-field conflict tier

`patch` is last-write-wins by default (ADR-0014). For fields that need richer
merge semantics, the per-field conflict-tier seam is the extension point — see
ADR-0004 / ADR-0014.

### Direct mode: stalls, retries, two engines on one file

- Every PostgREST request is bounded: 10 s to connect, 30 s between bytes
  (reqwest ships with no timeout at all; a VPN'd iPhone hung the first sync
  forever, measured 2026-09-25). A long snapshot that is still flowing never
  trips it.
- A failed sync retries on a 0.5 s → 30 s backoff, so a bad first sync comes
  back in seconds, not at the 60 s poll floor. `status` shows `reconnecting`
  meanwhile; nothing for you to do.
- The pull is gzip'd (7.4× fewer bytes on a 1k-row snapshot).
- The SQLite file opens in WAL mode with a 5 s busy timeout, so a push wake
  isolate and the foreground app can share one `sqlitePath` without
  `SQLITE_BUSY`.

### Server-side write allowlist

Writes are **server-gated** by `NOSTOS_WRITE_TABLES` (empty default = all writes
no-op, ADR-0013). On your `nostos-server`, set it to your writable tables:
```
NOSTOS_WRITE_TABLES=tasks,projects,comments
```
If your writes silently do nothing, this is why — see `docs/OPERATING.md`.

### Attachments (files, images) — T6 / ADR-0034

Two planes: a normal synced `attachments` table (metadata:
`id, filename, size, media_type, state, timestamp`) and a `BlobStore` +
`AttachmentStorageAdapter` pair for the bytes. Nostos never sees the bytes;
the object key is the attachment `id`.

```dart
final attachments = db.attachments(
  adapter: SupabaseStorageAdapter(bucket: 'product-images'),   // your bucket
  blobStore: LocalFileBlobStore(Directory('$dir/blobs')),      // dart:io cache
);

// Per-user files: durable transfers through the outbox.
final id = await attachments.queueUpload(filename: 'a.jpg', bytes: b, mediaType: 'image/jpeg');
await attachments.queueDownload(id);      // another device fetches it
attachments.start();                      // 2 s driver while online

// Shared catalog (every device reads, nobody writes): read-through, no
// metadata write, no `start()` needed.
final bytes = await attachments.bytes('p1-protein.jpg');   // null = uncached + download failed
```

Checklist:
- Declare `attachments` in `NostosSchema` and subscribe it.
- Server mode: add it to `NOSTOS_WRITE_TABLES`. Direct mode: give it a
  change-log trigger (`nostos link --mode direct --public attachments` for a
  shared catalog, or a `<column> = claims.<field>` scope) and RLS.
- The bucket needs `storage.objects` RLS for the device (`select` on
  `bucket_id = '<bucket>'`; `insert`/`delete` too for uploads).
- `signOut()` wipes the `BlobStore` unless `keepLocalOnSignOut: true`
  (ADR-0049), same policy as the rows.
- `LocalFileBlobStore` is `dart:io` — on web bring your own `BlobStore`
  (IndexedDB / OPFS); the driver is platform-agnostic.

Worked example: atlet's product images (`apps/atlet/supabase/migrations/0012_product_images_attachments.sql`,
`apps/atlet/flutter/lib/adapters/nostos_adapter.dart`).

---

## 11. Production checklist

- [ ] **Server auth:** `NOSTOS_SYNC_AUTH=supabase-jwt` +
      `NOSTOS_SUPABASE_JWT_SECRET` (your Supabase project's GoTrue signing key).
      `none` is single-tenant only.
- [ ] **Tenant isolation:** the JWT `sub` becomes the tenant id (ADR-0011). Confirm
      your Postgres publication + server predicate filter on it.
- [ ] **Write allowlist:** `NOSTOS_WRITE_TABLES=<your writable tables>` — without
      it, every write silently no-ops.
- [ ] **License (Cloud only):** `NOSTOS_LICENSE=<payload>.<sig>` from Nostos Cloud
      sets tier + device cap. Empty (default) = OSS self-host, unlimited
      (ADR-0006).
- [ ] **Token refresh:** wire `Supabase.instance.client.auth.onAuthStateChange`
      → reconnect on `tokenRefreshed` (v1 fast-follow until transparent refresh
      lands).
- [ ] **SQLite path:** pass the same `sqlitePath` / `sqliteDir` across launches
      so the durable store + its read-views persist.
- [ ] **Sign-out policy (direct mode):** decide `keepLocalOnSignOut`. Default
      wipes on `signOut()`; `true` keeps this user's rows for their next
      sign-in and only wipes when another user signs in (ADR-0049). Shared
      devices: leave it `false`.

---

## 12. API quick reference (verified signatures)

```dart
// Connect (pick one)
static Future<NostosDatabase> open({required NostosConfig config, NostosSchema? schema, required String sqliteDir});
static Future<NostosDatabase> connect({required String url, String? token, NostosSchema? schema, required String sqlitePath});
static Future<NostosDatabase> supabase({required String nostosUrl, NostosSchema? schema, required String sqlitePath, Set<String>? orSetTables, Set<String>? counterTables});
static Future<NostosDatabase> direct({required String supabaseUrl, required String anonKey, required String scope, String? token, required NostosSchema schema, required String sqlitePath, Map<String, String> counterFields = const {}, bool keepLocalOnSignOut = false});

// Session
Future<void> subscribe(String table, {String? where});
Future<void> subscribeTables(List<String> tables);
ValueListenable<SyncStatus> get status;
SyncStatus get currentStatus;
Stream<NostosConnectionState> get connectionState;
Future<void> signOut();            // drops the token; wipes unless keepLocalOnSignOut
bool get keepLocalOnSignOut;       // ADR-0049; the blob-store sign-out hook honours it

// extension AttachmentDatabase (T6)
Attachments attachments({required AttachmentStorageAdapter adapter, required BlobStore blobStore, int maxAttempts = 5});
Future<Uint8List?> Attachments.bytes(String id);   // read-through, no metadata write
Future<void> close();

// SyncStatus
NostosConnectionState get conn;   bool get connected;
DateTime? get lastSyncedAt;      bool get hasSynced;
int get pendingWrites;           bool get hasPendingWrites;   bool get uploading;
int get deadLetteredWrites;      String? get lastWriteError;  bool get hasWriteError;

// Reads
Stream<List<Map<String, dynamic>>> watch(String sql, {Duration? throttle});
Future<List<Map<String, dynamic>>> getAll(String sql);
Future<List<Map<String, dynamic>>> execute(String sql);   // SELECT-only in v1
Stream<List<T>> watchMapped<T>(String sql, T Function(Map<String, dynamic>) fromRow);
Future<List<T>> getAllMapped<T>(String sql, T Function(Map<String, dynamic>) fromRow);

// Writes (op ∈ {"upsert","delete","patch"})
Future<int> write({required String table, required String op, required Object pk, Map<String, dynamic>? payload});

// Typed facade
Collection<T> collection<T>({required String table, required T Function(Map<String, dynamic>) fromRow, Map<String, dynamic>? Function(T)? toRow, String pkColumn = 'id'});

// Collection<T>
Stream<List<T>> watch({String? where, Duration? throttle, String? orderBy});
Stream<int> count({String? where});
Future<int> upsert(T value);                              // needs toRow
Future<int> upsertRow(Map<String, dynamic> row);
Future<int> patch(Object pk, Map<String, dynamic> columns);   // LWW, ADR-0014
Future<int> delete(Object pk);
```

### Exports (`package:nostos_flutter/nostos_flutter.dart`)
`Nostos`, `NostosSupabase`, `NostosConnectionState`, `NostosTableSub`, `NostosConfig`,
`NostosSchema`, `NostosTable`, `NostosColumn`, `NostosDatabase`, `Collection`,
`SyncStatus`.

---

## 13. Where to go next

- **Working consumer app:** `fixtures/flutter/todo` — a real Flutter app using
  `Collection<T>` with add / edit / toggle / swipe-to-delete + a sync-status
  banner, against a live `nostos-server`. Read its
  `lib/infra/nostos_todo_repository.dart` for the exact CRUD pattern in production.
- **Server ops:** `docs/OPERATING.md` — every env var, startup-failure modes,
  slot lifecycle, "connected but lists empty" triage.
- **Architecture & decisions:** `docs/ARCHITECTURE.md`, then ADRs — 0004
  (conflict tiers), 0010/0011 (auth + tenant isolation), 0013 (write outbox +
  allowlist), 0014 (patch LWW), 0019 (schema), 0024 (`Collection<T>`), 0025
  (resume/replay), 0026 (shutdown durability).
- **Packaging path proof:** `integration_test/nostos_server_test.dart` in this
  package — the W4 acceptance test that spins up a real `nostos-server` and
  drives the full connect/subscribe/watch loop inside a genuine app bundle.
