> **Read this first — this is an AS-BUILT RECORD, not a proposal (rewritten 2026-07-30).**
>
> Two earlier headers were both wrong. It first read "PLAN — no implementation without
> explicit operator go"; a 2026-07-30 revision downgraded that to "live as an API proposal,
> GATED-ON-GO". **Both understated reality: six of the seven decisions had already shipped**
> and were exported from `nostos_flutter`. A doc that says "not implemented" about implemented
> code is as harmful as the reverse — the next agent either rebuilds what exists or treats the
> genuine gaps as done. Ratified as an as-built record by the operator on 2026-07-30.
>
> **The diagnosis remains dead.** "add does nothing" / "5 rows → 1 shows" was **falsified by
> real-Postgres repro on 2026-07-13**: a `PgWriteBack` TEXT-vs-`TIMESTAMPTZ` bind (chrono fix)
> and a config bug (`NOSTOS_REPLICATOR != pg`, so the snapshotter was `None`). Both since
> fixed, neither by anything here. Do not cite the "Why" section as motivation for future work.
>
> **One decision was reversed, not shipped:** D1's *materialized typed tables* are now
> **rejected** — [ADR-0028](../adr/0028-client-read-views-over-opaque-payload.md).
> Index: [`README.md`](README.md).

# Nostos Flutter — PowerSync-Style Connection Redesign

**Started:** 2026-07-13. **Owner:** Claude (tech lead).
**Status:** AS-BUILT — D2–D6 shipped; D1 shipped in a different form (views, not typed
tables, ADR-0028); D7 settled differently (both classes exported, one taught).

## As-built ledger (2026-07-30)

Verified against `sdk/nostos_flutter/lib/` and `crates/` on 2026-07-30, not from memory.

| # | Decision | State | Evidence |
|---|---|---|---|
| D1 | PowerSync-shaped API, collapsed writes | ✅ **surface** / ❌ **storage** | `NostosDatabase.watch/getAll/write/collection`; storage is VIEWs over `cairn_data`, **not** typed tables — ADR-0028 |
| D2 | Hybrid schema (auto-fetch + override) | ✅ shipped | `GET /schema` (ADR-0021) + `NostosSchema`; `NostosDatabase._fetchSchema` |
| D3 | Instant-local writes + reconcile | ✅ shipped | `client.rs:407` `apply_local`; tests `apply_local_renders_instantly_and_echo_reconciles`, `apply_local_patch_merges_fields_and_renders_offline` |
| D4 | Per-field last-write-wins | ✅ shipped | PATCH targeted `UPDATE SET`, ordered by WAL arrival |
| D5 | DX edges (auto-schema, no `uploadData`, one-liner, codegen) | ✅ shipped | `NostosDatabase.supabase(…)`; `nostos gen` → `example/lib/nostos.g.dart` |
| D6 | Supabase first-class | ⚠️ **partial** | `NostosSupabase` + `.supabase(…)` shipped; **no connector / no token refresh — open P1, below** |
| D7 | Rollout = replace `Nostos` | ⚠️ **settled differently** | Both exported; `NostosDatabase` is the only *taught* surface. No `@Deprecated` |

### D6's token-refresh gap — CLOSED 2026-07-30

**Fixed, and not the way it was ratified.** The agreed fix was a pure-Dart `onAuthStateChange`
auto-wire with no Rust change. Reading the engine falsified that: the token is baked in at
`NostosHandle::connect`, `SyncClientConfig` is immutable after construction, and there is **no**
token-swap primitive (the docstring that claimed `NostosSupabase` had one was wrong). So "pure Dart"
necessarily meant *rebuilding the handle* — and `_replayLatest` wires `onDone: controller.close`,
so every `watch` stream the UI holds would end. That trades silent sync-death for apparent
data-loss, which is worse.

Built instead (grilling option **b**, the named upgrade path):

- `SyncClient.token: RwLock<Option<String>>` seeded from config, read by `connect_url()`, with
  `set_token()` (`crates/nostos-client/src/client.rs`). Test:
  `set_token_changes_the_next_connect_url`.
- `NostosHandle::set_token` updates both the seed and the live client — the seed alone would not
  reach a running client, the client alone would be discarded by the next `subscribe()`.
- `Nostos.setToken` + `NostosEngine.setToken`; `NostosDatabase.supabase` subscribes to
  `onAuthStateChange` (`tokenRefreshed`, `signedIn`, `userUpdated`, and `signedOut` → clear), with
  the subscription cancelled in `close()`.
- Dart tests: delegation, null-clearing, and **that an active `watch` stream is not disturbed**.

No reconnect is forced; a refresh self-heals within one backoff window.

### Original write-up of the gap (kept for the record)

`NostosConnector` **does not exist** anywhere in `lib/` or `crates/`. The plan's
"implement `NostosConnector` (`fetchCredentials` → token, refresh)" was never built;
`NostosDatabase.connect` takes a static `String? token`. `ClientConfig.token` is immutable
after construction, `run_with_reconnect` → `run_once` → `connect_url()` re-sends it every
attempt, and the server enforces `exp` (`jwks.rs:90`). **A Supabase-backed app therefore
stops syncing roughly an hour after login and never recovers**, unless the developer
manually re-connects on `onAuthStateChange` — which today is disclosed only in a dartdoc
at `nostos.dart:458`.

**Fix to build (operator-ratified 2026-07-30):** a pure-Dart auto-wire inside
`NostosDatabase.supabase(…)` — listen to `onAuthStateChange`, reconnect with the fresh
token on `tokenRefreshed`. No Rust/FFI change. `ponytail:` ceiling = Supabase only.
**Upgrade path when a non-Supabase user asks:** thread a
`Future<String> Function()? tokenProvider` through FRB into `ClientConfig` so
`connect_url()` re-resolves per attempt. Not the plan's `NostosConnector` class — its
`uploadData` half is precisely the boilerplate nostos's write-back exists to delete.

### Corrections to the spec below

The "Target DX" and "Architecture changes" sections are kept for the record but are
**not** buildable as written:

1. `await db.execute('INSERT INTO tasks …')` / `'DELETE …'` — shipped `execute` is a
   **read-only alias of `getAll`**. Those samples cannot work. Writes go through
   `write` / `Collection.upsert` / `patch` / `delete`. (A raw `INSERT` against a synced
   table name fails loudly — it's a view — which is the point; ADR-0028.)
2. `Schema([Table('tasks', [Column.text('title')])])` — `Table` and `Column` collide with
   `material.dart`'s widgets and are deliberately **not** re-exported. Canonical names are
   `NostosSchema` / `NostosTable` / `NostosColumn`.
3. "Replace the opaque `cairn_data(table, pk, BLOB)` model with **real typed tables**" —
   rejected, ADR-0028. A slow query gets a partial expression index, not a rewrite.

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
  the new API. Adds per-row delete/edit and makes Disconnect / Stop / Airplane
  **distinct** (see below). (The `created_at` bug is now fixed independently of
  WS5 — see "What this fixes" below.)
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

- `created_at` write bug → **fixed by a surgical `chrono` typed bind in
  `PgWriteBack`** (ADR-0019 follow-on, 2026-07-13) — NOT by WS2/WS5. Verified root
  cause: `json_value_to_sql` mapped the ISO8601 string to `Text`; tokio-postgres
  extended-query resolves the column type server-side and rejects a `String`
  against `TIMESTAMPTZ` client-side → `WriteResult{ok:false}` → dead-letter →
  "add does nothing." The "5 in Postgres, only 1 shows" READ symptom is a
  SEPARATE config bug: the fixture ran without `NOSTOS_REPLICATOR=pg` so the
  ADR-0014 `PgSnapshotter` stayed `None` (proven: with it wired, late subscribers
  get 5/5). Fixed by a startup guard in `main.rs` + the fixture `.env`.
  WS2 typed materialization remains valuable for DX, but fixes neither symptom.
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
