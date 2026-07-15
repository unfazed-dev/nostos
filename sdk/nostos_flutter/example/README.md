# nostos_flutter example — Provider Dashboard (offline-first, multi-table)

A Flutter (macOS) app demonstrating the ratified `nostos_flutter` SDK surface:
subscribe to **5 Postgres tables over one `/sync` socket**, render each
reactively via `watchMapped<T>`, write offline-first (durable outbox that
flushes on reconnect), and **pause/resume syncing for real** via
`disconnect()` / `resume()`.

Tables (NavigationRail tabs): **Providers · Clients · Availabilities ·
Appointments · Invoices**. Schema + seed live in
[`docker/pg-init/`](../../docker/pg-init/) (3 providers, 4 clients, 5
availabilities; appointments + invoices are unseeded — create them in-app).

> **nostos reads your schema — it does not create it.** Your Postgres/Supabase DB
> is the source of truth; nostos mirrors it to on-device SQLite via logical
> replication, so the tables must already exist upstream. For local dev,
> `docker/pg-init/` seeds the Docker Postgres on boot; for Supabase, paste
> [`supabase/schema.sql`](../../supabase/schema.sql) into the SQL editor (see
> "Run it (Supabase)" below). This matches PowerSync — no sync tool provisions
> your source schema.

The app connects to `ws://127.0.0.1:8800/sync` by default
(`--dart-define=NOSTOS_URL=...` to override) and persists to a temp SQLite file.

## Run it (local Postgres)

Two terminals:

```sh
# Terminal 1 — start nostos-server against a real Postgres (Docker).
# `make dev-stack` composes PG, waits for the `cairn_pub` publication, then runs
# nostos-server with NOSTOS_REPLICATOR=pg + NOSTOS_PG_URL set.
#
# Writes are allowlist-gated (ADR-0013): nostos AUTO-APPLIES writes server-side
# (collapsed-write model — no uploadData), so it gates them at the SQL trust
# boundary. NOSTOS_WRITE_TABLES defaults EMPTY (no tables writable). Set it to
# the dashboard's 5 tables so create/complete/cancel/issue work:
NOSTOS_WRITE_TABLES=appointments,providers,clients,invoices,availabilities make dev-stack

# Terminal 2 — run the Flutter app (from this example/ dir).
flutter run -d macos
```

The app auto-fetches the server's typed schema (`GET /schema`) and materializes
read-views, so each table renders with real columns (`name`, `status`, …).

**Writes are off by default — that's deliberate, not a bug.** nostos's
collapsed-write model means the *server* applies your writes (no `uploadData`),
so it must allowlist which tables a client may write (`NOSTOS_WRITE_TABLES`,
empty by default = none — defense-in-depth at the SQL-injection boundary). If
you skip the `NOSTOS_WRITE_TABLES=…` prefix, create/edit/delete are rejected with
`table not writable: '<table>' — add it to NOSTOS_WRITE_TABLES …` (the error
names the exact fix). Full security model (least-privilege role, the RLS
trade-off): [`../../docs/SECURITY.md`](../../docs/SECURITY.md).

## Offline-first demo (the point of this app)

1. With the server up, watch rows stream in across all 5 tabs.
2. Tap the **Disconnect** icon (top-right). The badge → `disconnected`; the app
   stays fully usable — reads/writes/UI keep working because `disconnect()`
   aborts ONLY the `/sync` loop, not the local client or storage.
3. Add appointments / invoices while offline. The amber banner counts writes
   queued locally in the durable outbox.
4. Tap **Resume**. The outbox flushes; your writes echo back live through the
   reactive watches, and the badge → `connected`.

(For a *transport-level* cut instead of an in-app pause, just kill `nostos-server`
mid-session — the app stays usable; restart the server and it auto-resumes +
flushes.)

## Run it (Supabase / cloud Postgres)

nostos-server sits between the app and your Supabase Postgres: the app talks to
the local server (`ws://127.0.0.1:8800/sync`); the server reads Supabase via
logical replication. Three steps.

**1. Create the schema in Supabase (bring your own schema).** Paste
[`supabase/schema.sql`](../../supabase/schema.sql) into the Supabase Dashboard
→ SQL Editor → Run. It creates the 6 tables + the `cairn_pub` publication + the
demo seed (idempotent — `CREATE IF NOT EXISTS` / `ON CONFLICT DO NOTHING`).

**2. Reach the direct host — it's IPv6-only on Supabase.** `db.<project>.supabase.co`
has an **AAAA record only** (no IPv4 A record), and the pooler can't carry
logical replication — so nostos-server must reach the direct host. On a network
that drops IPv6 egress (most dev VPNs), tunnel via the userspace Cloudflare
WARP relay (no sudo, no `warp-cli`):

```sh
SUPABASE_REF=<project-ref> scripts/warp-ipv6-egress.sh up   # 127.0.0.1:15433 -> [Supabase-v6]:5432
```

(On a box with real IPv6 egress, or with the paid Supabase IPv4 add-on, skip the
relay and point `NOSTOS_PG_URL` at `db.<project>.supabase.co:5432` directly.)

**3. Point nostos-server at the relay** (nostos connects `NoTls`, so use
`sslmode=disable` — not `require`):

```sh
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL='postgresql://postgres:<pw>@127.0.0.1:15433/postgres?sslmode=disable' \
NOSTOS_WRITE_TABLES=tasks,providers,clients,availabilities,appointments,invoices \
cargo run -p nostos-server
```

Then `nostos pull && nostos gen` rebuilds `.nostos/schema.json` + `nostos.g.dart`
from your Supabase schema, and `flutter run -d macos` syncs from Supabase.
Verified 2026-07-12 against a real project: full snapshot + live + LSN-resume
e2e green through this relay. (`scripts/warp-ipv6-egress.sh down` stops it. For
least-privilege, create a dedicated `REPLICATION` role instead of `postgres` —
see `docker/pg-init/02-nostos-role.sql` + `docs/SECURITY.md`.)

## ⚠️ Do NOT use `make run` for this demo

`make run` starts nostos-server with the **`fake`** replicator (no Postgres).
With the fake replicator:

- Writes are **not** replicated (`ok:false` → dead-lettered) — "create does
  nothing." (Write-back requires `NOSTOS_REPLICATOR=pg`.)
- Rows render as **raw bytes**, not typed columns (the fake replicator emits
  filler, not JSON), so the lists look empty/garbled.
- The server logs a warning: *"NOSTOS_PG_URL is set but NOSTOS_REPLICATOR is not
  'pg' — snapshot-on-subscribe is OFF; clients will not receive pre-existing
  rows on connect."*

Always use `make dev-stack` (local) or the `NOSTOS_REPLICATOR=pg` launch block
above (cloud). If lists look empty with rows in Postgres, you are in fake mode —
stop the server and relaunch with `NOSTOS_REPLICATOR=pg`.
