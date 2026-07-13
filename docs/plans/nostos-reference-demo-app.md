# Nostos Reference Demo App — Offline-First Tasks (all SDKs)

**Started:** 2026-07-13. **Owner:** Claude (tech lead). **Bar (operator-approved):**
engineer + design ONE application that demonstrates **all** nostos features, with
the **same** app implemented across every SDK — starting with **Flutter** (iOS +
Android), then the other iOS/Android-capable SDKs (React Native, Kotlin, Swift,
Capacitor, .NET). The operator must **visually see** the app running with action
controls (pause / resume-restart / stop / airplane-mode), and nostos must operate
as a **local offline-first** engine (PowerSync-equivalent): reads + writes against
the local store while offline, durable queued writes, automatic sync on reconnect.

Per the standing scope rule, every SDK app **lives as a nostos test fixture** under
`sdk/<sdk>/example` (or the SDK's app dir) — not a separate product repo. This
plan is the shared spec; Flutter is the reference implementation; the other SDKs
port it.

## What "all nostos features" means here

| nostos capability | how the demo shows it |
|---|---|
| Live replication (server → client) | task list updates as rows arrive over `/sync` |
| Reactive watch | `watch(table)` / equivalent drives the list `Stream` |
| Reactive SQL watch (PowerSync parity) | `watchQuery` / equivalent for a filtered view (e.g. open vs done) |
| Durable offline writes (outbox) | add a task while "offline" → it survives in the local store, flushes on reconnect (ADR-0013) |
| Auto-reconnect | connection-state badge cycles Disconnected → Reconnecting → Connected |
| Client → server echo | the writer's own write re-emits via the server's WriteBack and lands locally |
| Checkpoint / Lsn | a small "synced through Lsn N" line in the status panel |
| Connection control | operator buttons: Pause / Resume / Stop / Airplane |

## Architecture (same for every SDK)

```
Supabase cloud PG (db.<ref>.supabase.co:5432, table `tasks`, publication `cairn_pub`)
        ▲
        │ logical replication (PgReplicator, `--features pg`)
        │
   nostos-server (runs on the dev host / a box with line-of-sight to cloud + device)
        ▲
        │ WebSocket  /sync  (JSON wire frames; ADR-0009)
        │
   SDK app (Flutter / RN / Kotlin / …) on device/sim
        │
   local SQLite (nostos-owned) — the offline-first source of truth
```

The app **never** talks to Postgres directly. It talks to one `nostos-server`
over `/sync`. Offline = the `/sync` WebSocket is down (airplane) or the app has
`close()`d its session (pause/stop); the local SQLite remains the read + write
surface, and the durable outbox holds writes until reconnect.

## The cloud source (Supabase)

- **Direct connection only** (`db.<ref>.supabase.co:5432`) — the pooler cannot
  carry logical replication (documented in `docs/plans/flutter-supabase-plug-and-play-launch.md`).
- **Schema + publication:** apply `docker/pg-init/01-sources.sql` as the `postgres`
  role — creates the `tasks` table + `cairn_pub` publication. (The
  `fixtures/flutter/todo/supabase/schema.sql` is `auth.users`-bound + RLS — NOT a
  replication source; do not use it for this.)
- **nostos-server launch:** `cargo run --features pg -p nostos-server --
  --replicator pg --pg-url 'postgresql://postgres:<pw>@db.<ref>.supabase.co:5432/postgres'
  --pg-publication cairn_pub --pg-slot cairn_slot --sync-auth none`.
- **Secret handling:** the password lives in the gitignored root `.env`
  (`SUPABASE_PASSWORD` + `NOSTOS_PG_URL_CLOUD`); `.env.example` carries placeholders
  only. Never commit the password.

### IPv6-only reachability block (verified 2026-07-13)

`db.ltamqsxxumtusyxswezi.supabase.co` resolves **IPv6-only** (`2406:da1c:…`), and
this dev host has **no IPv6 route** (`No route to host` to the AAAA and even to
`ipv6.google.com`). So nostos-server's `PgReplicator` cannot reach the cloud from
here — the cloud-backed demo is **blocked by network**, not code. This is the
exact scenario `nostos doctor`'s IPv6-only hint flags. Unblock options for the
operator: (a) enable IPv6 on the dev network, (b) add the Supabase **IPv4 add-on**
to the project (gives `db.<ref>.supabase.co` an A record), or (c) run the demo
from a host with IPv6. The moment one of those is true, the launch command above
works as-is. **Until then, the visual demo runs against the local e2e spine**
(real JSON task payloads, write+echo, full offline-first proof — the same backend
the other SDK slices use).

## Operator controls (the same five on every SDK)

The app surfaces a control panel + a connection-state badge. Mapping to the
nostos client surface (Flutter as reference; others mirror):

| control | nostos action | what the operator sees |
|---|---|---|
| **Pause** | `close()` the active subscription (keep the `Nostos` handle) | badge → Disconnected; writes queue locally |
| **Resume / Restart** | `subscribe(table)` again on the same handle | badge → Connecting → Connected; queued writes flush |
| **Stop** | `close()` + release the handle | session fully torn down |
| **Airplane mode** | toggle real device network (platform channel) where supported; else = Pause | the hero offline proof — nostos rides a true network drop |
| (badge) | `connectionState` stream | Connecting / Connected / Reconnecting / Disconnected |

**Why this is a *real* offline-first demo, not theater:** nostos's durable outbox
is the on-device SQLite store (ADR-0013). `close()` aborts the sync loop but the
SQLite file — including pending writes — persists. Re-subscribe opens the same
file; pending writes flush to the server on reconnect. So Pause → add tasks →
Resume visibly shows queued writes syncing. That is the PowerSync-equivalent
contract.

## Per-SDK status (fills as each ships)

| SDK | platform(s) | status |
|---|---|---|
| Flutter | iOS / Android / macOS | 🟡 reference — in progress 2026-07-13 |
| React Native | iOS / Android | ⏳ port (after Flutter reference locks) |
| Kotlin | Android | ⏳ port |
| Swift | iOS / macOS | ⏳ port |
| Capacitor | iOS / Android (webview) | ⏳ port |
| .NET | iOS / Android | ⏳ port |
| Node / Tauri / Rust / Web | desktop / server / browser | ⏳ later (not "iOS/Android first") |

Ports are fanned out via the Swarm **after** the Flutter reference is locked +
visually verified — each port is independently compiled + run on a device before
it's marked green (this session's process lesson: agents' green self-reports are
unverified until reproduced).

## Sequencing

1. **Flutter reference** (this effort): redesign `sdk/nostos_flutter/example/lib/main.dart`
   into the Tasks UI + controls; run against the cloud-backed `nostos-server` on
   iOS sim (fallback macOS); screenshot the offline-first flow.
2. **Cloud wiring + per-SDK runner tables** (infra, parallel).
3. **Swarm fan-out:** RN → Kotlin → Swift → Capacitor → .NET, each porting the
   Flutter spec, each verified on a device.
