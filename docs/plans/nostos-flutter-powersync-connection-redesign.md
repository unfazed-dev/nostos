# Nostos Flutter — PowerSync-Style Connection Redesign

**Started:** 2026-07-13. **Owner:** Claude (tech lead). **Status:** PLAN — no
implementation without explicit operator go (standing scope rule: plans only,
nostos tree only).

Supersedes the "keep the SDK light / defer P6 schema-materialization" stance in
`docs/plans/<powersync-sdk-parity-plan>`. Shipped parity work (P1 SQL `watchQuery`,
P2 dead-letter, P3 PATCH) **stands** and is reused below.

## Why

The current `nostos_flutter` connection surface — `Nostos.connect` →
`subscribe` / `watch(table)` / `write(table, op, pk, payload Map)` against an
opaque `cairn_data(table, pk, BLOB)` store — is a bespoke API that does not match
the PowerSync Flutter pattern the market expects. The demo app exposed the cost
of that gap:

- a **TEXT-bound payload-Map write bug** (`created_at` JSON-string → bound as
  TEXT → Postgres rejects `TEXT → timestamptz`; every add-task fails `ok:false`,
  stuck retrying in the outbox — verified 2026-07-13);
- **no per-row delete/edit**;
- **Disconnect / Stop / Airplane all identical** (each calls the same
  `close()` + null-handle path).

Operator directive: *"this is not how a flutter app should connect to nostos —
implement an improved version of how PowerSync connects."* PowerSync's pattern
(verified from
[the Flutter reference](https://docs.powersync.com/client-sdks/reference/flutter)):
define a `Schema([Table(…, [Column.…])])` → implement a `BackendConnector`
(`fetchCredentials` + `uploadData`) → `PowerSyncDatabase(schema, path).initialize().connect(connector)`
→ use `db.execute / watch / getAll` against **your own typed tables**, with
synced data projected through SQLite views.

## Decisions ratified (grill-with-docs, 2026-07-13)

1. **Scope = "Option C" — PowerSync-shaped API, collapsed writes.** Adopt
   PowerSync's API surface (`Schema` + `NostosConnector` + `NostosDatabase` +
   `db.execute/watch/getAll` over **materialized typed tables**). **Keep** nostos's
   collapsed write model: `connector.uploadData` is *optional*; nostos-server's
   `PgWriteBack` auto-applies writes. This is "improved PowerSync" — the DX
   surface parity plus nostos's zero-backend-write edge.
2. **Schema source = Hybrid.** Auto-fetch the typed schema from nostos-server by
   default (zero boilerplate — the headline DX win over PowerSync's hand-written
   `Schema`); an explicit `Schema(...)` can override/constrain.
3. **Write model = Instant-local + background sync.** `db.execute` writes the
   real local typed table *immediately* (renders even fully offline); an oplog
   syncs to nostos-server; reconcile to the server-authoritative value on echo.
   True offline-first feel.
4. **Conflict resolution = per-field last-write-wins.** Already shipped as P3
   PATCH (targeted `UPDATE SET` of only the changed columns, ordered by server
   WAL arrival). Matches [PowerSync's default](https://docs.powersync.com/handling-writes/handling-update-conflicts)
   ("last write wins per field; updates to different fields don't conflict").
   Richer merging (document-style) via opt-in `UploadConnector` (P4) + CRDTs is
   a fast-follow — the same tiering PowerSync offers.
5. **DX edges over PowerSync:** auto-schema (no manual `Schema`), zero-backend
   writes (no `uploadData`), one-line `NostosDatabase.supabase(…)`, optional
   typed-record codegen.
6. **Supabase adapter = first-class.** `SupabaseConnector` (auth from the
   `supabase_flutter` session; JWT verified server-side ADR-0010; tenant
   predicate enforced ADR-0011/0018) + `NostosDatabase.supabase(…)` factory. No
   `uploadData` required (the DX edge over PowerSync's `SupabaseBackendConnector`).
7. **Rollout = Replace.** `NostosDatabase` replaces the `Nostos` class; the demo
   migrates. nostos_flutter is pre-1.0 with one demo consumer, so a clean break
   beats dual-maintenance. The Rust core (`nostos-core`, `nostos-client`) stays;
   the change is the Dart API + the local storage model.

## Target DX (the spec the implementation must match)

```dart
// 1. Schema is OPTIONAL — default is auto-fetched from nostos-server (zero boilerplate).
//    Pass one only to constrain/reshape.
final schema = Schema([
  Table('tasks', [Column.text('title'), Column.boolean('completed')]),
]);

// 2. Connect — Supabase one-liner. Auth from the Supabase session; JWT verified
//    server-side; tenant enforced. No uploadData, ever.
final db = await NostosDatabase.supabase(
  supabaseUrl: 'https://<ref>.supabase.co',
  anonKey: '<anon-key>',
  nostosUrl: 'ws://<nostos-server>/sync',
  schema: schema,            // optional
  path: sqlitePath,          // optional
);

// 3. Read reactively — re-runs when `tasks` mutates.
final Stream<List<Task>> tasks = db.watch(
  'SELECT id, title, completed FROM tasks ORDER BY created_at DESC',
);

// 4. Write — instant local (offline-first), background-synced, per-field LWW reconcile.
await db.execute(
  'INSERT INTO tasks (id, title, completed, org_id) VALUES (uuid(), ?, false, ?)',
  ['Buy oranges', orgId],
);
await db.execute('DELETE FROM tasks WHERE id = ?', [id]);
await db.execute('UPDATE tasks SET completed = ? WHERE id = ?', [true, id]);
```

Non-Supabase / custom-auth: implement `NostosConnector` (`fetchCredentials` →
token, refresh; optional `uploadData` for custom resolution) and call
`NostosDatabase.connect(connector, …)`.

## Architecture changes

- **`nostos-core` Storage + `nostos-client` SqliteStorage — materialized typed
  tables.** Replace the opaque `cairn_data(table, pk, BLOB)` model with **real
  typed tables** (one per synced table, columns from the auto/explicit schema).
  The apply engine `UPSERT`s replication events into the typed tables (column
  types from the server schema; kills the TEXT→timestamptz class of bug). The
  `Storage` trait gains the query surface on the concrete type (P1 pattern).
- **Oplog + instant-local writes.** Intercept `db.execute` INSERT/UPDATE/DELETE
  against materialized tables → apply locally immediately (optimistic) → capture
  into an oplog → flush via the existing wire `Write` frame to nostos-server
  `PgWriteBack`. On echo, reconcile to the server-authoritative image (per-field
  LWW; idempotent).
- **`nostos-server` schema endpoint.** Expose the publication's typed schema
  (`GET /schema`, or a schema frame sent on connect). nostos-server already
  bootstraps relations + column types from the catalog (`bootstrapped relations
  from catalog`); typed payload mapping is already server-side (ADR-0019).
- **`nostos_flutter` Dart API.** `NostosDatabase` + `NostosConnector` +
  `Schema`/`Table`/`Column` + `SupabaseConnector` + `db.execute/watch/get`. The
  `Nostos` class is removed.

## Workstreams (sequenced)

- **WS1 — nostos-server schema endpoint.** Expose typed publication schema
  (tables/columns/SQLite-affinity) for the client's auto-schema default.
- **WS2 — materialized typed-table storage + oplog + instant-local writes +
  reconcile** (nostos-core + nostos-client). Largest; the architectural heart.
  Replaces the opaque-blob model. Must not regress the throughput moat
  (re-bench `make bench`).
- **WS3 — `NostosDatabase` Dart API** (replaces `Nostos`): `Schema`, `NostosConnector`,
  `db.execute/watch/getAll`, `connectionState`/status stream.
- **WS4 — `SupabaseConnector` + `NostosDatabase.supabase(…)` factory**
  (`supabase_flutter` session auth).
- **WS5 — migrate the demo app** (`sdk/nostos_flutter/example/lib/main.dart`) to
  the new API. **Dissolves** the created_at bug, adds per-row delete/edit, and
  makes Disconnect / Stop / Airplane **distinct** (see below).
- **WS6 — optional typed-record codegen** (opt-in; from the auto/explicit schema).

Deferred: `UploadConnector` custom-resolution hook (P4); CRDTs; nostos-server URL
discovery endpoint.

## Controls (disconnect / stop / airplane) — distinct, real

Resolved by the migration (WS5) on top of `NostosDatabase`:

- **Pause** — stop the sync loop, keep the local DB + handle. Writes still land
  locally (instant-local). Badge → disconnected.
- **Resume** — restart the sync loop on the same DB; oplog flushes. Badge →
  connecting → connected.
- **Stop** — close the DB/handle fully (teardown).
- **Airplane** — a **real** network cut, not a client close: stop nostos-server
  (or block the port) so the socket dies from the app's side → badge →
  reconnecting (auto-retry) → queued oplog writes flush on restore. This is the
  visible difference from Pause (disconnected, no retry) and the hero offline
  proof. (Mechanism: platform channel or the operator toggling nostos-server; the
  demo will document the simplest reliable cut for macOS.)

## Preserve (moat — do NOT regress)

- Rust fan-out throughput (142k–833k ops/sec vs PowerSync 2k–4k) — `benches`;
  re-measure after WS2.
- Apache-2.0 end-to-end.
- Server-enforced tenant isolation (ADR-0011/0018) — stronger than client-trusted.
- Zero-backend-write DX (no required `uploadData`).
- Typed payloads server-side (ADR-0019).
- Human-debuggable JSON wire (ADR-0009) — keep until a measurement says otherwise.

## What this fixes (demo), as a consequence

- `created_at` write bug → gone (typed SQL writes; no TEXT-bound payload-Map).
- Per-row delete/edit → `db.execute('DELETE|UPDATE tasks WHERE id = ?')`.
- Disconnect / Stop / Airplane → distinct real behaviors (above).

## Open sub-decisions (deferred)

- nostos-server URL discovery (explicit `nostosUrl` now; future `nostos dev`
  well-known discovery endpoint).
- CRDT support (fast-follow).
- `UploadConnector` custom-resolution hook (P4).

## Explicit-go gate

This is a plan. Implementation begins on operator "go" + sequencing approval.
Per standing scope: plans only; nostos tree only; commit only when asked.
