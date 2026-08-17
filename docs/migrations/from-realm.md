# Migrating from Realm (Atlas Device Sync)

> *A concept map, an honest data-export strategy, and server/client steps for moving an Atlas Device Sync app to Nostos. This is a re-platform, not a drop-in — the object store becomes Postgres + SQLite.*

**Last reviewed: August 2026.** The deprecation context below is stated conservatively and dated; verify it against MongoDB's own notice before planning around it.

---

## Why teams are moving off Atlas Device Sync

In **September 2024**, MongoDB announced the deprecation of Atlas Device Sync and the Atlas Device SDKs (the Realm-based SDKs), with published end-of-support dates in **2025** — see [MongoDB's deprecation notice](https://www.mongodb.com/docs/atlas/device-sdks/deprecation/) for the authoritative dates and scope; this guide deliberately does not restate specifics beyond that. As of August 2026, treat the Device Sync product line as deprecated. The status of the standalone Realm embedded database has continued to evolve since the announcement — check MongoDB's notice and the project's own channels rather than this page before making claims about it.

Nostos is one possible landing spot, with a clear-eyed caveat up front: **this is the biggest model change of any migration path into Nostos.** Realm is an embedded *object* database with a sync server attached; Nostos is *row-level* Postgres→SQLite sync. You are re-platforming the data model (objects → tables) and the sync engine at once.

Also read the maturity note in [`from-powersync.md`](from-powersync.md): Nostos is a Phase-3 alpha, the client SDKs are not yet on registries, and the shipped `/sync` verifiers are `none` (dev) and `supabase-jwt` (ADR-0010) — if your Realm app authenticated through something other than a Supabase-compatible JWT issuer, verify that path first.

---

## Concept mapping

| Realm / Atlas concept | Nostos equivalent | Documented in |
|---|---|---|
| Atlas Device Sync (the sync server) | `nostos-server` reading your Postgres via logical replication (`pgoutput`), fanning out to per-device SQLite | [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md) |
| MongoDB Atlas (source of truth) | **Your Postgres** — any deployment; Supabase works (JWKS auth + direct-PG connection, see `.env.example`) | [`docs/OPERATING.md`](../OPERATING.md) |
| Flexible Sync subscriptions (RQL queries, parameterized) | **`where_sql` predicate subscriptions** — a safe-SQL subset, one predicate per subscription, compiled and enforced server-side; layered underneath `nostos_rules.toml` (ADR-0031) and tenant scoping (ADR-0011) | [ADR-0012](../adr/0012-dynamic-predicate-expression-engine.md) |
| Sync rules: queryable fields, roles, permissions (App Services config) | `nostos_rules.toml` modes `all`/`toggles`/`hand` + `NOSTOS_TENANT_COLUMN` enforced on every predicate. v1 rules grammar is a deliberate AND-only subset — no OR/NOT/joins (ADR-0031 non-goals) | [ADR-0031](../adr/0031-sync-rules-modes-and-checksum-resync.md), [ADR-0011](../adr/0011-server-enforced-predicates.md) |
| Realm local database (the `.realm` object store) | On-device **SQLite**: SQL reads over views on the SQLite-backed SDKs (Flutter, Node, RN, Tauri, Kotlin, Swift, .NET); in-memory KV on the WASM-backed web client today (ADR-0017) | [`docs/api/README.md`](../api/README.md) |
| Realm SDKs (Kotlin, Swift, .NET, JS/React Native) | `sdk/` per-platform SDKs over `nostos-core`/`nostos-client` — path dependencies, not yet on registries | [`docs/api/README.md`](../api/README.md) |
| Realm Auth / App Services authentication | **Supabase auth**: `NOSTOS_SYNC_AUTH=supabase-jwt`, verified via HS256 secret or JWKS (RS256/ES256/EdDSA); `sub` becomes account and tenant id | [ADR-0010](../adr/0010-sync-authentication-and-principal.md) |
| Client writes → Device Sync → Atlas | Durable SQLite outbox → flush over the authenticated `/sync` socket → **direct write-back** to Postgres behind `NOSTOS_WRITE_TABLES` | [ADR-0013](../adr/0013-direct-write-back-design.md) |
| Conflict handling under Device Sync | Server-authoritative **last-writer-wins by WAL order** (ADR-0004/0014 tier (a)); richer tiers are specced later work | [ADR-0014](../adr/0014-tiered-conflict-resolution.md), [ADR-0030](../adr/0030-crdt-merge-tier.md) |

---

## The data move (export strategy)

**Nostos ships no Realm→Postgres import tool.** As of August 2026 nothing in this repo reads `.realm` files, and no export command exists — the data move is yours to own. What follows is the shape of the move, not a tool.

1. **One table per Realm object type.** Map each Realm class to a flat Postgres table: `_id` (ObjectId/UUID) → a `UUID PRIMARY KEY`; links/to-many relationships → foreign-key columns or child tables; embedded objects → a `JSONB` column or a child table (your call per access pattern); lists of primitives → a Postgres array or a child table; timestamps → `timestamptz`.
2. **Add a tenant column.** Nostos's server-enforced scoping needs one (`NOSTOS_TENANT_COLUMN`, default `org_id`) if any per-user/per-org isolation matters — the moral equivalent of your Device Sync permissions, but column-based.
3. **Design for the write-back binder.** From [`docker/pg-init/01-sources.sql`](../../docker/pg-init/01-sources.sql): client-written integer columns should be `BIGINT` (the Rust adapter binds `i64` → `INT8`; `INT4` rejects `i64`, and there is no `NUMERIC` binding variant), and money as integer cents.
4. **Set `REPLICA IDENTITY FULL` on tenant-scoped tables** so replicated deletes carry the tenant column — otherwise tenant-filtered replay drops deletes and reconnects grow ghost rows (ADR-0025 follow-up, documented in that SQL file).
5. **Load Postgres with standard tooling** — `psql \copy`, `COPY`, or whatever ETL your platform offers. The export mechanics from a Realm SDK differ per language (serializing objects through your own app code is the normal path); the invariant is simply: the rows land in Postgres, because…
6. **Postgres becomes the source of truth.** Every writer — server-side services and clients — must target Postgres at cutover. Devices then take a **fresh snapshot** from nostos on first connect; the local `.realm` files do not carry over, so finish any device-local-only work before you retire the old build.

---

## Server steps

Same spine as the PowerSync guide — see [`from-powersync.md`](from-powersync.md) Steps 1–2 for the long form:

```bash
# Local reference stack (Postgres 16, wal_level=logical, port 5433):
make pg-up            # or: docker compose -f docker/docker-compose.yml up -d postgres

# Create the publication over your tables, write nostos.toml + .env:
cargo run -p nostos-cli -- init \
  --db-url postgresql://user:pass@host:5432/db \
  --tables items,folders \
  --write-tables items \
  --tenant-column owner_id

# Run + verify:
cargo run -p nostos-cli -- dev
cargo run -p nostos-cli -- doctor
```

The environment you are configuring (full table in [`from-powersync.md`](from-powersync.md) Step 2; canonical list in `.env.example`): `NOSTOS_REPLICATOR=pg`, `NOSTOS_PG_URL`, `NOSTOS_PG_SLOT`/`NOSTOS_PG_PUBLICATION`, `NOSTOS_SYNC_AUTH=supabase-jwt` with `NOSTOS_SUPABASE_JWT_SECRET` or `NOSTOS_SUPABASE_URL`/`NOSTOS_SUPABASE_JWKS_URL`, `NOSTOS_TENANT_COLUMN`, `NOSTOS_WRITE_TABLES`, and the WAL-retention guards (`NOSTOS_SLOT_MAX_LAG`, `NOSTOS_PG_SLOT_WAL_KEEP_SIZE`) a production deploy must set.

Read rules replace your App Services sync-rule config:

```bash
cargo run -p nostos-cli -- rules init    # nostos_rules.toml, toggles mode, every table off
cargo run -p nostos-cli -- rules edit    # toggle tables on, 'w' saves
cargo run -p nostos-cli -- rules check   # validate + print mode and checksum
```

No file on disk = `sync_mode = "all"` (zero-config dev default; tenant scoping still applies underneath — ADR-0031).

## Client steps

1. **Pick the SDK** for each platform you are leaving (Kotlin, Swift, .NET, JS/React Native all have pages) in [`docs/api/README.md`](../api/README.md). None are on registries yet — consume from this repo as path/git dependencies.
2. **Subscriptions: RQL → `where_sql`.** Where Device Sync took an RQL query per subscription, nostos takes one table plus an optional `where_sql` predicate, compiled server-side against a safe SQL subset (ADR-0012) and AND-ed under the rules scope and tenant scope (ADR-0011) — the wire frame:

   ```json
   { "type": "subscribe", "table": "items", "where_sql": "owner_id = '00000000-0000-0000-0000-000000000001' AND done = false" }
   ```

   There is no parameterized-query object, no JOIN/CTE subscription, and no lazy stream in v1 — if your Realm app leaned on queryable-relationship traversal, plan for per-table subscriptions plus client-side joins (parity tracking: [`docs/plans/powersync-sdk-parity-plan.md`](../plans/powersync-sdk-parity-plan.md)).
3. **Local reads: objects → SQL.** The SQLite-backed SDKs expose synced tables as SQLite views you query with SQL (Flutter also has reactive `watch` APIs — see [`docs/api/flutter.md`](../api/flutter.md)); the object-graph access pattern (`realm.objects(…)`, traversing links, live results) has no direct equivalent. This is the bulk of your client rewrite.
4. **Writes: outbox, not session writes.** A write is durable on disk before any network I/O and flushes on connect/reconnect through the server's allowlisted write-back (ADR-0013). Embedded-object and graph mutations must decompose into row upserts/deletes.
5. **Auth: Supabase.** Mint the same JWT your app already uses against Supabase; the server resolves it to a principal and scopes every predicate and write (ADR-0010/0011/0018).

---

## What changes (the honest costs)

- **Object model → relational rows.** Links become FKs, embedded objects become JSONB/child tables, and every object-graph traversal in your UI layer becomes a query you write. Budget for this; it dominates the migration.
- **Schema migrations change homes.** Realm's schema-version migrations become Postgres migrations; the server sees tables, not object schemas.
- **Tooling.** Realm Studio is replaced by ordinary Postgres and SQLite tooling.
- **Alpha.** Phase 3, launch gated; SDKs from the repo; no managed cloud; Postgres is the only source — there is no MongoDB path.
- **Conflict model.** Server-authoritative LWW by WAL order today; if you depended on Device Sync's conflict behavior, test your worst concurrent-edit cases during dual-run.
- **Web (if you had one).** The browser client's durability is best-effort until the OPFS work lands (ADR-0017).

For the cutover sequence itself, follow [`from-powersync.md`](from-powersync.md) Step 4 — dual-run against the same Postgres, drain old pending writes per cohort, flip, then retire the old stack. The one Realm-specific addition: run the data export (above) to completion and cut all writers over to Postgres *before* the first cohort flips, since Postgres is now the source of truth, not Atlas.
