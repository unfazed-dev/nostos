# Migrating from PowerSync

> *Concept mapping first, then a stepped server/client swap and a cutover strategy. Written against the Phase-3 alpha — read the maturity note before planning a date around it.*

**Last reviewed: August 2026.** External claims about PowerSync are stated as of that review and sourced to PowerSync's own docs or to this repo's audited comparison ([`docs/COMPARISON.md`](../COMPARISON.md)); re-verify anything time-sensitive before you act on it.

---

## Before you start: the maturity check

Nostos is **alpha** (see the status banner in [`README.md`](../../README.md)): the Rust server, the real-Postgres replicator, the native client, and write-back v1 are shipped, but launch is gated and the client SDKs are **not yet published to package registries** — you consume them from this repo today ([`docs/api/README.md`](../api/README.md)). Migrating production traffic now is early-adopter territory: use the dual-run cutover below, keep a rollback path, and budget time to build SDKs from source.

Two hard blockers to rule out first:

- **Source database.** Nostos reads **Postgres logical replication only** (`pgoutput`). PowerSync also lists MySQL and MongoDB as sources (per their docs, July 2026); if your source of truth is not Postgres, this guide is not for you yet.
- **Identity provider.** Nostos's shipped `/sync` verifiers are `none` (dev-only) and `supabase-jwt` — HS256 shared secret or JWKS RS256/ES256/EdDSA ([ADR-0010](../adr/0010-sync-authentication-and-principal.md)). If your PowerSync deployment mints client JWTs from a non-Supabase IdP, confirm your tokens are JWKS-verifiable with a `sub` claim before committing; a generic OIDC-issuer mode is not shipped today.

One piece of stale marketing to unlearn: **PowerSync shipped dynamic Sync Streams to GA in May 2026**, and the old "1,000-bucket hard cap" line was a soft default, not a ceiling. Nostos does not claim PowerSync cannot do dynamic sync ([`docs/COMPARISON.md`](../COMPARISON.md) §0). The migration reasons that remain are license, write-back, and self-host — covered honestly in [What you gain / what changes](#what-you-gain--what-changes).

---

## Concept mapping

| PowerSync concept | Nostos equivalent | Documented in |
|---|---|---|
| Sync Streams / legacy bucket sync rules (YAML) | **`where_sql` predicate subscriptions** — a safe-SQL predicate per subscription, compiled server-side, evaluated per event; layered *underneath* `nostos_rules.toml` (ADR-0031) and server-enforced tenant scoping (ADR-0011) | [ADR-0012](../adr/0012-dynamic-predicate-expression-engine.md), [ADR-0031](../adr/0031-sync-rules-modes-and-checksum-resync.md), [ADR-0011](../adr/0011-server-enforced-predicates.md) |
| `uploadData()` / write-path endpoints you build and host | **Direct write-back** (ADR-0013): client mutations queue in a durable SQLite outbox, flush over the same authenticated `/sync` socket, and are applied to Postgres behind the `NOSTOS_WRITE_TABLES` allowlist. Alternatively keep your own backend writing straight to Postgres — replication fans either path out to every client | [ADR-0013](../adr/0013-direct-write-back-design.md) |
| Client JWT verified by the PowerSync service | **Supabase JWT via JWKS** (ADR-0010): `NOSTOS_SYNC_AUTH=supabase-jwt` plus `NOSTOS_SUPABASE_JWT_SECRET` (legacy HS256) or `NOSTOS_SUPABASE_URL` / `NOSTOS_SUPABASE_JWKS_URL` (RS256/ES256/EdDSA). The token rides `Authorization: Bearer` or `?token=` on the WS handshake | [ADR-0010](../adr/0010-sync-authentication-and-principal.md) |
| PowerSync client SDKs (Flutter, RN, JS/Web, Swift, Kotlin) | `nostos-core` apply engine + `nostos-ffi-wasm` (web) behind per-platform SDKs: `sdk/nostos_flutter`, `sdk/nostos_node`, `sdk/nostos_react_native`, `sdk/nostos_swift`, `sdk/nostos_kotlin`, `sdk/nostos_dotnet`, `sdk/nostos_tauri`, `sdk/nostos_capacitor`, `sdk/nostos_web` | [`docs/api/README.md`](../api/README.md) |
| PowerSync Service (self-host or cloud) | `nostos-server` — the axum binary; free, full-featured, unlimited self-host | [`docs/OPERATING.md`](../OPERATING.md) |
| `powersync.yaml` / `sync-rules.yaml` | `nostos_rules.toml` with three modes — `all` (zero-config dev default; no file on disk means `all`), `toggles`, `hand` (ADR-0031). Managed live via `nostos rules` and `PUT /rules` (`NOSTOS_ADMIN_TOKEN`) | [ADR-0031](../adr/0031-sync-rules-modes-and-checksum-resync.md) |
| PowerSync's replication slot on your Postgres | Nostos's own slot `cairn_slot` (`NOSTOS_PG_SLOT`) reading publication `cairn_pub` (`NOSTOS_PG_PUBLICATION`) | [`docs/OPERATING.md`](../OPERATING.md) §2 |
| Push / notifications integrations | Push as a wake-up trigger, not a data channel: `nostos push init` / `nostos push check`, embedded router or the standalone `nostos-pushd` daemon | [`docs/push.md`](../push.md) |

---

## Step 1 — Schema and publication setup

Your tables stay yours: `nostos init` creates/updates the **publication**, not your tables. Postgres needs `wal_level = logical` and a publication covering every table you want to sync.

The working example in this repo is [`docker/pg-init/01-sources.sql`](../../docker/pg-init/01-sources.sql) — read it before designing your schema. Three of its habits matter to any migration:

1. **Tenant tables get `REPLICA IDENTITY FULL`** so a replicated `DELETE` carries the full old row image (including the tenant column); without it, tenant-filtered replay can drop deletes and leave ghost rows on reconnect (ADR-0025 follow-up, documented in that file).
2. **Client-written integer columns are `BIGINT`** (and money is integer cents). The Rust write-back binds JSON values by shape inference — `i64` → `INT8` — and `INT4` columns reject `i64`; there is no `NUMERIC` binding variant.
3. **Nostos-internal tables (`cairn_oplog`, `cairn_push_tokens`) stay out of the publication** — publishing them would feed nostos's own machinery back through replication as spurious client events.

Commands:

```bash
# Local reference stack (Postgres 16, wal_level=logical, host port 5433):
make pg-up    # or: docker compose -f docker/docker-compose.yml up -d postgres

# Point nostos at YOUR Postgres: creates/updates the publication,
# writes nostos.toml + .env. --write-tables is the write-back allowlist
# (ADR-0013) — a subset of --tables; omit it for read-only sync.
cargo run -p nostos-cli -- init \
  --db-url postgresql://user:pass@host:5432/db \
  --tables tasks,projects \
  --write-tables tasks \
  --tenant-column org_id
```

On Supabase: point `--db-url` at the **direct** connection (`db.<ref>.supabase.co:5432`), not the pooler — logical replication needs it. See the `NOSTOS_PG_URL_CLOUD` notes in [`.env.example`](../../.env.example) (including the IPv6-only caveat for `db.<ref>.supabase.co`).

## Step 2 — Server swap

For a first look, run the repo's own stack against real Postgres:

```bash
make dev-stack   # compose-up Postgres + run nostos-server with PgReplicator on :8800
```

For your own database, `nostos init` (above) wrote `nostos.toml` + `.env`; then:

```bash
cargo run -p nostos-cli -- dev       # runs nostos-server locally from nostos.toml + .env
cargo run -p nostos-cli -- doctor    # connectivity, replication health, JWKS reachability
```

For a self-host deploy, `nostos deploy` generates a fly/railway config from `nostos.toml`; the server itself is the `nostos-server` binary. The environment that replaces your PowerSync service config:

| Variable | Example | Role |
|---|---|---|
| `NOSTOS_REPLICATOR` | `pg` | real Postgres logical replication (`fake` is the no-PG dev default) |
| `NOSTOS_PG_URL` | `postgresql://…` | source database |
| `NOSTOS_PG_SLOT` / `NOSTOS_PG_PUBLICATION` | `cairn_slot` / `cairn_pub` | slot + publication names |
| `NOSTOS_SYNC_AUTH` | `supabase-jwt` | verify client JWTs (`none` is single-tenant dev only) |
| `NOSTOS_SUPABASE_JWT_SECRET` *or* `NOSTOS_SUPABASE_URL` / `NOSTOS_SUPABASE_JWKS_URL` | | HS256 secret or JWKS endpoint (ADR-0010 addendum) |
| `NOSTOS_TENANT_COLUMN` | `org_id` | column AND-ed into every predicate (ADR-0011) |
| `NOSTOS_WRITE_TABLES` | `tasks` | write-back allowlist; empty = read-only |
| `NOSTOS_BIND` / `NOSTOS_WS_PATH` | `0.0.0.0:8800` / `/sync` | where clients connect |
| `NOSTOS_SLOT_MAX_LAG` / `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` | | WAL-retention guards — **a production deploy must set these** (see `.env.example`) |

Read rules replace your sync-rules YAML conceptually. Generate and edit them without a restart:

```bash
cargo run -p nostos-cli -- rules init    # writes nostos_rules.toml in 'toggles' mode, every table sync=false
cargo run -p nostos-cli -- rules edit    # toggle tables on; 'w' saves
cargo run -p nostos-cli -- rules check   # validate, print active mode + checksum
```

No `nostos_rules.toml` on disk means `sync_mode = "all"` — everything replicated syncs to every authorized client (with a startup warning naming table counts). Tenant scoping still applies underneath; `all` disables rules, not isolation (ADR-0031).

## Step 3 — Client swap

1. **Pick your SDK** in [`docs/api/README.md`](../api/README.md) — the matrix there is honest about the one real split: SQLite-backed SDKs (Flutter, Node, RN, Tauri, Kotlin, Swift, .NET) give **SQL reads over SQLite views**; the WASM-backed ones (browser, Capacitor) give an in-memory KV today, with durable browser storage deliberately deferred ([ADR-0017](../adr/0017-web-persistence.md) — read it if you are migrating a web client). None are on registries yet: consume them as path/git dependencies from this repo. For Flutter, [`docs/QUICKSTART.md`](../QUICKSTART.md) is the end-to-end walkthrough.
2. **Replace sync rules with subscriptions.** Each subscription targets one table with an optional `where_sql` predicate — a safe SQL subset compiled server-side (ADR-0012); an invalid predicate is rejected at subscribe time, and a predicate can never widen past the rules scope or the tenant scope (ADR-0011). The wire frame (the protocol is human-debuggable JSON by design):

   ```json
   { "type": "subscribe", "table": "tasks", "where_sql": "assignee_id = '00000000-0000-0000-0000-000000000001'" }
   ```

   Exact per-SDK call signatures are on each [`docs/api/`](../api/README.md) page — check them there rather than from memory; the repo actively validates those pages against source for a reason.
3. **Delete your `uploadData()` path** (or keep it — writes straight to Postgres replicate fine). The nostos equivalent is an outbox write: the mutation is durable on disk before any network call, flushes over the authenticated `/sync` socket on connect/reconnect, and the server applies it inside the `NOSTOS_WRITE_TABLES` allowlist with typed parameterized binding and server-authoritative last-writer-wins by WAL order (ADR-0013). The replication echo needs no suppression: client apply is an idempotent upsert.
4. **Expect a fresh local database.** Nostos's client keeps its own SQLite store and builds it from the Postgres snapshot on first connect. PowerSync's on-device database does not carry over — local-only rows you still want must be written to Postgres before cutover.

## Step 4 — Cutover strategy

**Recommendation: dual-run, per-cohort — not big-bang.** Three reasons:

1. **The topology is free.** Both engines read the same Postgres simultaneously through their own replication slots (PowerSync's slot alongside `cairn_slot`). This repo proves that coexistence: `make ps-up` ([`docker/docker-compose.powersync.yml`](../../docker/docker-compose.powersync.yml)) runs the PowerSync Service against the same Postgres Nostos reads, and `crates/nostos-infra/tests/powersync_smoke.rs` validates the shared-source setup:

   ```bash
   make ps-up
   NOSTOS_POWERSYNC=1 NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn \
     cargo test -p nostos-infra --test powersync_smoke -- --nocapture
   ```

2. **Alpha discipline.** With the SDKs unpublished and launch gated, you want the PowerSync path intact as a rollback while a shadow cohort runs on nostos.
3. **Cohort flips are cheap.** A client that switches engines just takes a fresh snapshot; there is no client-side data migration to schedule.

A dual-run sequence that works:

1. Stand up nostos-server (Steps 1–2) alongside PowerSync against the same Postgres; leave writes flowing through your existing path.
2. Move a shadow cohort (internal builds, dogfood devices) to a nostos SDK; verify snapshot correctness, subscription predicates, and write-back against real usage.
3. Per cohort: **drain the PowerSync upload queue first** (pending client mutations must land in Postgres before the old client retires), then flip the cohort's build to the nostos SDK and let its writes go through nostos's outbox.
4. Monitor (`NOSTOS_METRICS_BIND`, [`docs/OPERATING.md`](../OPERATING.md) triage sections), then retire the PowerSync service and **drop its replication slot** — two live slots retain WAL for both.
5. Big-bang is acceptable only for dev environments and small fleets with no offline-write backlog: one flag-day, fresh snapshots everywhere, rollback = redeploy the old build.

Ops note while both run: WAL retention is the sum of both slots' lag. Set `NOSTOS_SLOT_MAX_LAG` / `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` (see `.env.example`) and mind the PowerSync slot's retention too.

---

## What you gain / what changes

**What you gain.**

- **License.** Apache-2.0 today, end to end — server, core, every SDK. PowerSync's server is FSL (source-available, no-compete clause, 2-year wait to Apache) as of this repo's July 2026 audit; if enterprise procurement is the trigger for your migration, this is the whole game. See [`README.md`](../../README.md) and [ADR-0005](../adr/0005-apache-2.0-license.md).
- **Write-back without endpoints.** No `uploadData()` implementation to build, host, or scope per tenant — the durable outbox and the allowlisted, fully parameterized apply path are the product (ADR-0013).
- **Free, full-featured, unlimited self-host.** No metered-per-op cloud tax and no license-delayed "open edition".
- **A Rust server, on its own measured terms.** Nostos's published figure: **833,307 ops/sec aggregate fan-out at 1,000 clients, 0.00% drops** (eval-only: FakeReplicator on loopback — see [`benches/results/RESULTS.md`](../../benches/results/RESULTS.md)). PowerSync publishes no comparable aggregate fan-out figure; its published rates are 2,000–4,000 ops/sec replication ingest and 2,000–20,000 ops/sec per-client sync — different pipeline stages, so **no ratio is claimed here and none should be inferred** ([`docs/BENCHMARK-METHODOLOGY.md`](../BENCHMARK-METHODOLOGY.md); the same-denominator rule). A same-source live head-to-head has not been run yet ([`docs/COMPARISON.md`](../COMPARISON.md) §2).

**What changes (the honest costs).**

- **Alpha.** Phase 3, launch gated; SDKs consume from the repo, not registries; no managed cloud (scoped, not shipped).
- **Postgres-only source.** MySQL and MongoDB sources are not supported.
- **Conflict resolution is LWW** — server-authoritative, WAL order (ADR-0004/0014 tier (a)). PowerSync additionally supports custom merge functions (July 2026); if your `uploadData()` path implements bespoke merges, that is a gap today — richer tiers are specced as later work ([ADR-0014](../adr/0014-tiered-conflict-resolution.md), [ADR-0030](../adr/0030-crdt-merge-tier.md)).
- **One predicate per subscription.** Sync-Streams-style parameterized queries with JOINs/CTEs and lazy streams are not shipped; parity is tracked in [`docs/plans/powersync-sdk-parity-plan.md`](../plans/powersync-sdk-parity-plan.md).
- **Web durability is deferred.** The browser client is in-memory with a best-effort `localStorage` checkpoint; OPFS-backed SQLite is a post-launch commitment (ADR-0017). PowerSync's web client persists via IndexedDB/OPFS today.
- **PowerSync remains more mature on every client surface.** That is this repo's own audited position ([`docs/launch/powersync-vs-nostos-draft.md`](../launch/powersync-vs-nostos-draft.md)) — the migration trade is license/write-back/self-host versus client-SDK polish.

---

## Pointers

- [`docs/QUICKSTART.md`](../QUICKSTART.md) — Flutter end-to-end (local + Supabase tracks)
- [`docs/OPERATING.md`](../OPERATING.md) — runbook: env, slot recovery, CLI reference, sync rules, admin token
- [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md) — the pipeline you are adopting
- [`docs/SECURITY-MODEL.md`](../SECURITY-MODEL.md) — why server-enforced predicates, not Postgres RLS, are the sync authorization layer
- [`docs/COMPARISON.md`](../COMPARISON.md) — the audited comparison, including the retired attack lines
- Migrating from Realm instead? [`from-realm.md`](from-realm.md)
