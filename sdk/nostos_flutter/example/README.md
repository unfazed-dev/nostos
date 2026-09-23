# nostos_flutter example — Provider Dashboard (offline-first, multi-table)

A Flutter (macOS) app demonstrating the ratified `nostos_flutter` SDK surface:
subscribe to **6 Postgres tables over one `/sync` socket**, render each
reactively via `watchMapped<T>`, write offline-first (durable outbox that
flushes on reconnect), and **pause/resume syncing for real** via
`disconnect()` / `resume()`.

A **production-quality booking application**: providers with configurable rates
(hourly / flat-fee / subscription), clients managing bookings, availabilities,
appointments, **invoices with auto-calculated billing** (rate × hours, rate
snapshotted at issue time), and a **realtime chat interface** between providers
and clients — all local-first and reactive via nostos sync.

Tables (tabs): **Providers · Clients · Availabilities · Appointments ·
Invoices · Chat**. Schema + seed live in
[`supabase/schema.sql`](../../supabase/schema.sql) (4 providers with mixed
rate types, 4 clients, 7 availabilities, 5 appointments, 3 auto-calculated
invoices, 6 chat messages).

> **nostos reads your schema — it does not create it.** Your Postgres/Supabase DB
> is the source of truth; nostos mirrors it to on-device SQLite via logical
> replication, so the tables must already exist upstream. For local dev,
> `docker/pg-init/` seeds the Docker Postgres on boot; for Supabase, paste
> [`supabase/schema.sql`](../../supabase/schema.sql) into the SQL editor (see
> "Run it (Supabase)" below). No sync tool provisions
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
NOSTOS_WRITE_TABLES=appointments,providers,clients,invoices,availabilities,messages make dev-stack

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

## Features

### Provider rate management (hourly / flat / subscription)
Each provider sets a `rate_type` + the three rate values via the tune icon on
their card. The rate type determines how invoices auto-calculate:

| Rate type | Invoice calculation | Example |
|---|---|---|
| **hourly** | `duration_min × hourly_rate_cents / 60` | 60min × $250/hr = $250.00 |
| **flat** | flat fee (duration-independent) | $180/visit |
| **subscription** | recurring monthly | $800/mo |

### Auto-calculated invoices (rate snapshot)
Creating an invoice from an appointment auto-calculates the amount via
`BillingService` (pure Dart, no server round-trip). The rate is **snapshotted**
into the invoice row at issue time (`rate_cents` + `line_type` + `hours_min`), so
a provider changing their rate later never re-prices a historical invoice — the
canonical billing pattern. The invoice creation dialog shows a live preview of
the breakdown before you commit.

### Appointments with optional auto-invoice
The appointment creation dialog has a "Auto-generate invoice" checkbox (on by
default). When checked, the invoice is created alongside the appointment in a
single flow — no separate step.

### Realtime chat (synced table = realtime stream)
The **Chat** tab is a realtime messaging interface between providers and clients.
This follows current local-first practice: the
synced `messages` table **IS** the realtime stream — no separate WebSocket. The
view watches `messages` reactively via `watchMapped`; sending a message writes to
the local outbox and it round-trips back through nostos replication in ~2-4s.

## Offline-first demo (the point of this app)

1. With the server up, watch rows stream in across all 6 tabs.
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

### The full chain (cloud topology)

```
   Supabase cloud Postgres                         your machine
   ┌──────────────────────────┐
   │ db.<ref>.supabase.co     │  IPv6-only direct host (free tier has
   │   tables + cairn_pub +   │  no IPv4 A record; pooler can't carry
   │   cairn_slot (logical    │  logical replication — must be the
   │   replication slot)      │  direct host)
   └─────────────┬────────────┘
                 │ TCP 5432, IPv6 only
                 │
        ┌────────▼─────────┐
        │  WARP relay      │  wireproxy userspace tunnel — no sudo.
        │  127.0.0.1:15433 │  `scripts/warp-ipv6-egress.sh up`
        │  → [Supabase]:5432│  (skip on real IPv6 egress / paid IPv4 add-on)
        └────────┬─────────┘
                 │ TCP 5432, IPv4 localhost, sslmode=disable
                 │
   ┌─────────────▼──────────────────┐
   │  nostos-server  (Rust binary)   │  `NOSTOS_REPLICATOR=pg
   │  • logical-replication consumer│   NOSTOS_PG_URL=…@127.0.0.1:15433/…`
   │    (slot cairn_slot → WAL)     │  Reads the WAL stream + pushes RowOps.
   │  • snapshot-on-subscribe       │  Auto-applies writes server-side
   │    (initial rows on connect)   │  (collapsed-write; gated by
   │  • GET /schema (table catalog) │  NOSTOS_WRITE_TABLES).
   └─────────────┬──────────────────┘
                 │ WebSocket  ws://127.0.0.1:8800/sync
                 │ (Subscribe → snapshot → live RowOps → Ack{lsn})
                 │
   ┌─────────────▼──────────────────┐
   │  nostos_flutter app (this app)  │  `flutter run -d macos`
   │  • SqliteStorage (cairn_data + │  One /sync socket, N subscribed tables
   │    per-table read views)       │  demuxed by WireFrame.table.
   │  • reactive watch() streams →  │  Durable outbox (offline writes flush
   │    IndexedStack pages          │  on reconnect).
   └────────────────────────────────┘
```

Writes flow the same path in reverse: app → local SQLite outbox → nostos-server
(`PgWriteBack`, gated by `NOSTOS_WRITE_TABLES`) → Supabase Postgres → back out
through logical replication as a normal RowOp → live echo through `watch()`.

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

## Troubleshooting — "snapshot works but live edits never arrive"

Symptom: rows appear on first connect (the snapshot-on-subscribe path works),
but edits made in the Supabase Dashboard never reach the app, and the
nostos-server log loops on:

```
ERROR nostos_infra::replicator::pg: replication recv error; will attempt reconnect
  error=server error: can no longer get changes from replication slot "cairn_slot" (SQLSTATE 55000)
```

**Root cause:** the logical replication slot has been *invalidated*. Postgres
discards WAL a slot hasn't consumed once it exceeds `max_slot_wal_keep_size`
(or the disk fills). This happens whenever nostos-server is **offline /
disconnected / pointed at a different DB** for too long — the slot keeps
demanding WAL that Supabase eventually reclaims. Once the WAL is gone, the
slot's `wal_status` flips to `lost` and it can only stream the *initial*
snapshot, not ongoing changes. SQLSTATE 55000
(`object_not_in_prerequisite_state`) is Postgres signaling this.

Confirm + recover (drop & recreate — an invalidated slot **cannot** resume):

```sh
# 1. confirm: wal_status should be 'lost'
PGPASSWORD=<pw> psql -h 127.0.0.1 -p 15433 -U postgres -d postgres -c \
  "SELECT slot_name, active, wal_status FROM pg_replication_slots WHERE slot_name='cairn_slot';"

# 2. STOP nostos-server first (a slot can't be dropped while a consumer holds it)

# 3. drop + recreate (must use the 'pgoutput' plugin the server expects)
PGPASSWORD=<pw> psql -h 127.0.0.1 -p 15433 -U postgres -d postgres -c \
  "SELECT pg_drop_replication_slot('cairn_slot');"
PGPASSWORD=<pw> psql -h 127.0.0.1 -p 15433 -U postgres -d postgres -c \
  "SELECT pg_create_logical_replication_slot('cairn_slot', 'pgoutput');"

# 4. relaunch nostos-server (the Step 3 command above) — it reconnects to the
#    fresh slot and live replication resumes. wal_status should now be 'reserved'.
```

**Prevention:** keep nostos-server running (or at least reconnecting) against the
Supabase project so the slot stays consumed. Don't leave it pointed at the local
Docker PG while editing the Supabase cloud DB — the slot pins WAL on the *wrong*
source and goes stale on Supabase. (Verified 2026-07-15: drop/recreate restored
live replication; a fresh `UPDATE` reached the app in ~2s.)

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
