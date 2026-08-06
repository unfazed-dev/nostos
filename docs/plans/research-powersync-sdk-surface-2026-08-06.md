# PowerSync SDK Surface + Supabase Integration — Research (2026-08-06)

Sources: official docs (docs.powersync.com), powersync.com blog/legal pages, GitHub. Web-search + fetched pages, not memory.

## 1. Client SDKs (current, per docs.powersync.com)

| Platform | Package / repo | License |
|---|---|---|
| Flutter/Dart | `powersync` (pub.dev) — [powersync-ja/powersync.dart](https://github.com/powersync-ja/powersync.dart) | Apache 2.0 |
| React Native | `@powersync/react-native` (v1.35.7 as of mid-2026) | Apache 2.0/MIT |
| Web (JS/TS) | `@powersync/web` | Apache 2.0/MIT |
| Node.js | `@powersync/node` | Apache 2.0/MIT |
| Capacitor | `@powersync/web` (Capacitor target) | Apache 2.0/MIT |
| Tauri (Rust) | Tauri client (via `powersync` Rust crate) | Apache 2.0/MIT |
| Kotlin | `com.powersync` | Apache 2.0 |
| Swift | PowerSync Swift package | Apache 2.0 |
| .NET | `PowerSync.Common` | Apache 2.0 |
| Rust (native) | `powersync` crate | Apache 2.0/MIT |

All the above JS variants (React Native, Web, Node, Capacitor, Tauri) live in the `powersync-js` monorepo, plus framework integrations (React, Vue, Nuxt, TanStack Query) and ORM integrations (Kysely, Drizzle; Flutter side has Drift). Minimum SDK versions carrying the new Rust-native sync client (v0.4.0 SQLite extension): RN 1.22.0, Web 1.23.0, Node 0.6.0, Flutter/Dart 1.14.0, Kotlin 1.2.0, Swift 1.2.0 (not yet on .NET at time of that release).

Client SDKs and supporting client-side packages are open-source (Apache-2.0/MIT) — only the **server-side PowerSync Service + CLI** are FSL.

## 2. Flutter + Supabase integration shape

**Schema** (client-side, applied on DB open, no migrations needed):
```dart
final todos = Table('todos', [
  Column.text('list_id'),
  Column.text('created_at'),
  Column.text('completed_at'),
  Column.text('description'),
  Column.text('created_by'),
  Column.text('completed_by'),
  Column.integer('completed'),
], indexes: [Index('list', ['list_id'])]);

final schema = Schema([todos]);
```

**Open database:**
```dart
final dir = await getApplicationSupportDirectory();
final path = join(dir.path, 'powersync-dart.db');
db = PowerSyncDatabase(schema: schema, path: path);
await db.initialize();
```

**Backend connector** (talks to Supabase + PowerSync auth):
```dart
class Connector extends PowerSyncBackendConnector {
  @override
  Future<PowerSyncCredentials> fetchCredentials() async =>
      PowerSyncCredentials(endpoint: '...', token: '...'); // dev token or JWT
  @override
  Future<void> uploadData(PowerSyncDatabase database) async {
    final tx = await database.getNextCrudTransaction();
    if (tx == null) return;
    for (final op in tx.crud) { /* op.opData + op.id -> Supabase client call */ }
    await tx.complete();
  }
}
await db.connect(connector: Connector());
```

**Read/watch (reactive):**
```dart
final todos = await db.getAll('SELECT * FROM todos');
db.watch('SELECT * FROM todos WHERE list_id = ?', [listId]).listen((rows) { /* update UI */ });
```

**Server side (Supabase Postgres):** create a `powersync_role` (REPLICATION, BYPASSRLS), grant SELECT, create publication named exactly `powersync` (`CREATE PUBLICATION powersync FOR ALL TABLES`). Supabase already runs with logical replication enabled, so only user+publication setup is needed — no `wal_level` change like a generic Postgres source.

**Sync rules / Sync Streams** (server-side YAML, PowerSync Cloud/Service):
```yaml
config:
  edition: 3
streams:
  user_data:
    auto_subscribe: true
    queries:
      - SELECT * FROM lists WHERE owner_id = auth.user_id()
      - SELECT todos.* FROM todos INNER JOIN lists ON todos.list_id = lists.id WHERE lists.owner_id = auth.user_id()
```
(legacy bucket_definitions syntax still supported: `parameters:` + `data:` per bucket). RLS and Sync Streams are separate, complementary layers — RLS on Supabase gates the Data API for uploads; Sync Streams gate what's downloaded to each client.

Official self-hosted Flutter+Supabase starter: **github.com/powersync-community/flutter-powersync-supabase**. Full walkthrough: docs.powersync.com "Supabase + PowerSync" integration guide (~10-15 min tutorial), builds a To-Do list demo.

## 3. Running PowerSync against Supabase Postgres — hosting options

- **PowerSync Cloud** (managed): free tier account, sign up via PowerSync Dashboard; simplest path, no infra to run.
- **PowerSync Open Edition** (self-hosted, free): server (`PowerSync Service`) + CLI are **FSL-1.1-ALv2** licensed (Functional Source License), NOT OSI open-source. Docker image: `journeyapps/powersync-service` on Docker Hub. Demo repo: [powersync-ja/self-host-demo](https://github.com/powersync-ja/self-host-demo) (Docker Compose). Easiest bring-up: `npm install -g powersync && powersync init self-hosted && powersync docker configure ...` (CLI scaffolds a Compose stack incl. Postgres for source+storage). **Dashboard is NOT available when self-hosting.**
- **Enterprise Self-Hosted Edition**: same code, custom support/pricing, contact sales.
- **License practical effect for benchmarking:** FSL permits running/modifying/redistributing for non-commercial and commercial internal use; the restriction is against offering the FSL'd software itself as a competing hosted service. Running self-hosted PowerSync Service to benchmark against Nostos is allowed under FSL — it converts to Apache 2.0 two years after each release. Client SDKs (what most app code touches) are unrestricted Apache-2.0/MIT regardless.

## 4. Published benchmarks / demo apps

- Primary benchmark post: **[How Fast Is PowerSync? Performance Benchmarks For Flutter](https://powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter)** — methodology: server stack run locally to minimize network/server variance; data volumes 10k–10M rows; metrics = mean of 10 runs; initial-sync-time = sync-open → all-data-local; incremental-sync-latency tested at 1/100/1000 updates (batched via one Postgres transaction + one batch API call), completion detected via a Postgres-populated default column. Reports **rows/sec**, not ops/sec — not directly comparable to Nostos's ops/sec without conversion. Key number: ~7.1k rows/sec on Android for a 10k-row sync. Web throughput depends heavily on storage backend (OPFS > IndexedDB). No official multi-client concurrent-load throughput ceiling number was found (Nostos's RESULTS.md cites a "4k ops/sec published high ceiling" — that figure did not surface in this pass; flag for the team member who sourced it to confirm origin).
- Service limits/perf reference: docs.powersync.com/resources/performance-and-limits (per-plan limits for PowerSync Cloud).
- Sync-client rewrite perf note: RN sync ~35% faster after the Rust-based SQLite-extension sync client (v0.4.0), per [releases.powersync.com announcement](https://releases.powersync.com/announcements/improved-sync-performance-in-our-client-sdks).
- Demo apps: Flutter+Supabase starter (powersync-community/flutter-powersync-supabase); the integration guide's own "To-Do List" app (framework-specific clone links inside the guide); self-host-demo (powersync-ja/self-host-demo) for the Docker Compose stack itself.

## Sources
- https://docs.powersync.com/integration-guides/supabase-+-powersync
- https://docs.powersync.com/usage/installation/client-side-setup
- https://docs.powersync.com/intro/self-hosting
- https://docs.powersync.com/client-sdks/reference/flutter
- https://powersync.com/open-source
- https://powersync.com/legal/fsl
- https://powersync.com/blog/new-open-era-for-powersync
- https://powersync.com/blog/powersync-open-edition-release
- https://powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter
- https://docs.powersync.com/resources/performance-and-limits
- https://releases.powersync.com/announcements/improved-sync-performance-in-our-client-sdks
- https://github.com/powersync-ja/self-host-demo
- https://github.com/powersync-ja/powersync.dart
- https://github.com/powersync-community/flutter-powersync-supabase
