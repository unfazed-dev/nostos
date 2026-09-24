# Quickstart: Flutter + Nostos in ≤5 minutes

Nostos is Postgres logical replication → a Rust fan-out server → on-device
SQLite: local-first, offline-capable sync for Flutter, Apache-2.0 end to end.
One control plane (your Postgres/Supabase project + one CLI), no connector
class, no duplicated client-side schema. An optional server-side
`nostos_rules.toml` (ADR-0031) gates what any client can read at all; your
Dart predicates (`where_sql`) narrow further, per subscription, on top of
that.

Two tracks below:

- **Local dev** (this page's own dry-run — see the timing note at the
  bottom) — any Postgres, works today, no external account needed.
- **Supabase project** — the target launch flow. Every
  Supabase-project-specific step is marked **⏳ pending live verification**:
  W0 (`docs/plans/flutter-supabase-plug-and-play-launch.md`) needs an
  operator-provided Supabase project to empirically verify these against a
  real one (JWKS default, direct-connection reachability, slot limits — see
  that plan's "Research ground truth"). Everything NOT marked ⏳ is proven —
  either directly, or via the local track exercising the identical code path
  (auth → tenant-scoped reads → tenant-enforced write-back) against a
  same-shape HS256 JWT instead of Supabase's real RS256/JWKS token.

`nostos` ships as a prebuilt binary (GitHub Releases + `brew tap` + curl
installer) once W6 (release engineering) lands. **Until then**, every `nostos
...` command below is `cargo run -p nostos-cli -- ...` from a checkout of this
repo — the CLI itself is fully built and working (W3), only its distribution
is pending.

## Local dev (works today)

Prerequisites: a Rust toolchain (`rustup show` in this repo), Flutter ≥3.47
with native assets enabled (`flutter config --enable-native-assets`, one-time
per machine), and a Postgres with `wal_level = logical` (the repo's `docker
compose -f docker/docker-compose.yml up -d postgres` gives you one
pre-configured; its db/user/password are the held pre-rename name `nostos`).

| Step | Command | Time budget |
|---|---|---|
| 1. Start Postgres | `docker compose -f docker/docker-compose.yml up -d postgres` (or point at your own `wal_level=logical` Postgres) | 0:00–0:15 |
| 2. Create your table | `CREATE TABLE todos (id text primary key, user_id text not null, title text not null, done boolean not null default false, created_at timestamptz not null default now());` — `nostos init` creates the **publication**, not your tables | 0:15–0:45 |
| 3. `nostos init` | `cargo run -p nostos-cli -- init --db-url postgresql://nostos:nostos@localhost:5433/nostos --tables todos --write-tables todos --tenant-column user_id` | 0:45–1:15 |
| 4. `nostos dev` | `cargo run -p nostos-cli -- dev` — prints the `ws://` URL + a copy-paste Dart snippet | 1:15–1:45 (plus first-run Rust compile — see the timing note) |
| 5. Add the SDK | `flutter pub add nostos_flutter` (pub.dev, once W6 publishes it — today: a `path:` dependency on `sdk/nostos_flutter`, see `sdk/nostos_flutter/example/pubspec.yaml`) | 1:45–2:15 |
| 6. ~10 lines of Dart | see below | 2:15–3:00 |

With no `nostos_rules.toml` present, step 4's `nostos dev` runs `sync_mode =
"all"` — everything replicated is synced to every authorised client, the
zero-config default for local dev (a startup warning names the tables and
row-count estimate). To scope it down to specific tables before you go past
your own machine: `nostos rules init` (writes `nostos_rules.toml` in
`toggles` mode, every table `sync = false`) then `nostos rules edit` (toggle
the tables you want on, `w` to save) — two commands, no restart. See
[OPERATING.md](OPERATING.md#8-sync-rules) for the full mode reference.

```dart
import 'package:nostos_flutter/nostos_flutter.dart';

final nostos = await Nostos.connect(url: 'ws://127.0.0.1:8800/sync', token: jwt);
await nostos.subscribe('todos'); // no where clause needed — the server scopes
                                 // reads to YOUR rows once auth is configured
                                 // (NOSTOS_SYNC_AUTH=supabase-jwt, ADR-0011)

nostos.watch('todos').listen((rows) {
  // rows: the full current row set for `todos` — durable-offline snapshot
  // first, then re-emitted after every applied change.
});

await nostos.write('todos', op: 'upsert', pk: id, payload: {'title': 'buy milk'});
// returns as soon as the write is durable on disk — NOT once the server
// acks it. The UI never blocks on connectivity (ADR-0013's outbox).
```

`token` is a bearer JWT `nostos-server` verifies per `NOSTOS_SYNC_AUTH`. With
no auth configured (`nostos init`'s default — no `--tenant-column`'s auth
wiring active until a JWT secret exists), `token` is ignored and every
client sees every row — fine for solo local dev, wrong for anything shared.
To exercise real per-user tenant isolation locally (what the Supabase track
gets for free from RLS-adjacent enforcement — see
[`SECURITY.md`](../SECURITY.md)), mint an HS256 JWT against the dev secret `nostos
dev` picked up from `.env`'s `NOSTOS_SUPABASE_JWT_SECRET` (`sub` becomes
both account id and tenant id; a client-supplied `tenant_id` claim is ignored).

**Wire types** (ADR-0019): `watch()` rows carry native JSON types — a
Postgres `boolean` is a Dart `bool`, `int2`/`int4` are `int`, and so on. Two
precision-preserving exceptions arrive as `String`: `int8`/`numeric`/`money`
(can exceed the 2^53 range a `double`/JS `number` holds exactly — parse with
`int.parse`/a `Decimal` type, never `num.parse`), and `bytea` (base64 —
decode with `base64Decode`). Timestamps arrive as RFC 3339 UTC strings
(`...Z`) — parse with `DateTime.parse`.

### The full working example

[`sdk/nostos_flutter/example`](../sdk/nostos_flutter/example/README.md) is a
real offline-first, multi-table Flutter app (a provider dashboard: rates,
invoices, appointments, chat) wired to `nostos-server` over `make dev-stack` or
a Supabase/cloud Postgres — its README has both run paths. Its
`integration_test/nostos_server_test.dart` is the W4 acceptance test: connect →
subscribe → fan-out → `watch()` against a real `nostos-server` binary.

### History: the 2026-07-12 live proof

The original todo fixture (`fixtures/flutter/todo`, removed in `2489ffb`; see
git history) drove two real `Nostos` instances with distinct HS256 JWTs
against a real `nostos-server` + docker Postgres. It proved read isolation
(ADR-0011) and write isolation (ADR-0018), and it found a launch-blocking
`watch()` write-miss: `ApplyEngine::feed` only flushed a transaction batch
when a later frame arrived. The root causes were fixed in code 2026-07-20 —
`subscribe_changes()` now precedes `emit_snapshot()`, `ApplyEngine` gained
`has_pending()`/`flush()` driven by `SyncClientConfig::flush_quiesce` (50 ms
default), and `idle_timeout` is a `SyncClientConfig` knob. Empirical re-verification is the W5 stranger test
below.

### Timing dry-run (author's machine, NOT the stranger test)

This is the plan's own author re-running the "Local dev" steps above,
stopwatched, alone, on a machine that already has this repo checked out —
**not** the operator-mandated stranger test (fresh machine, fresh person, no
author present), which stays a launch-blocking TODO.

| Cache state | Steps 1–5 wall-clock | Note |
|---|---|---|
| Warm (`cargo`/pub caches already populated from earlier work in this repo) | ~10s docker + init + ~3s dev startup + pub get already resolved | Comfortably inside 5:00 |
| Cold (fresh `~/.cargo/registry`, no prior build of `nostos-cli`/`nostos-server`/`nostos_flutter`'s Rust crate) | **not separately measured in this pass** — cargo compiling `nostos-cli`, `nostos-server` (with the `pg` feature), and `nostos_flutter`'s native-assets fallback from scratch is realistically several minutes each, likely blowing the 5:00 budget on a cold machine | This is exactly what W6's prebuilt-binary distribution (GitHub Releases, `hook/prebuilt.json`) exists to fix — until it ships, "≤5 minutes" is a warm-cache claim, not a cold-clone one. Flagged, not fudged. |

## Supabase project

1. Create a Supabase project (or use an existing one).
2. Database → get the **direct connection string** (not the pooler — logical
   replication needs it).
   > **⚠️ IPv6 warning (verified 2026-07-12):** free-tier direct connections
   > are **IPv6-only** (AAAA records only), and a network that *assigns* your
   > machine an IPv6 address does not necessarily *route* it — we reproduced
   > exactly this on a real dev network: global IPv6 address present, all v6
   > TCP failing "no route to host". `nostos doctor` detects this case and
   > names it. Poolers do NOT carry logical replication, so you must reach the
   > direct host. Fixes, easiest first:
   > 1. **Userspace Cloudflare WARP** (free, no sudo, no macOS system
   >    extension, does not disturb an existing full-tunnel VPN) —
   >    `SUPABASE_REF=<ref> scripts/warp-ipv6-egress.sh up` runs WARP via
   >    `wireproxy` in userspace and exposes `127.0.0.1:15433` → your Supabase
   >    host. Point nostos at
   >    `postgresql://postgres:<pw>@127.0.0.1:15433/postgres?sslmode=disable`
   >    (nostos connects with NoTls today; Supabase's direct host permits
   >    plaintext, so `sslmode=disable` — *not* `require`). Verified
   >    end-to-end against a real project: the full replication e2e is green
   >    through this tunnel. `…sh down` stops it.
   > 2. A network with working IPv6 egress.
   > 3. The Supabase IPv4 add-on (paid, Pro+) for the direct connection.
3. `nostos init --db-url <direct connection string> --tables <your tables>
   --write-tables <writable subset> --tenant-column <your tenant column>
   --supabase-url https://<project-ref>.supabase.co` — creates the
   publication, derives the JWKS URL, writes `nostos.toml` + `.env`.
   ✅ verified 2026-07-12 against project `ltamqsxxumtusyxswezi`: the
   `postgres` role can create/drop a logical slot + publication (pgoutput),
   5 slots / 0 used, and nostos's `PgReplicator` runs the full snapshot +
   live + LSN-resume e2e green (`e2e_pg_replication` 3/3, `e2e_pg_snapshot`
   2/2).
4. `nostos dev` — prints the `ws://` URL.
5. `flutter pub add nostos_flutter supabase_flutter`.
6. ```dart
   final session = Supabase.instance.client.auth.currentSession!;
   final nostos = await NostosSupabase.connect(
     nostosUrl: 'ws://<your nostos dev host>:8800/sync',
     supabaseUrl: 'https://<project-ref>.supabase.co',
     accessToken: session.accessToken,
   );
   ```
   ⏳ pending live verification against a real Supabase JWT: `nostos-server`'s
   JWKS verifier (RS256/ES256, the default for projects created since
   2025-10-01) is implemented and unit-tested (W2) but not yet exercised
   against a genuine Supabase-issued token end-to-end.
7. RLS does **not** apply to Nostos's replication or write-back traffic —
   Nostos's server-side tenant predicates ARE the authorization layer for sync
   traffic. Read [`SECURITY.md`](../SECURITY.md) before treating your existing RLS
   policies as sufficient.

## Known gaps (read before you build on this)

- **`watch()` write-miss / first-write-non-reach (originally flagged
  LAUNCH-BLOCKING 2026-07-12): root causes addressed in code 2026-07-20**
  (subscribe-before-emit + `flush_quiesce` + `idle_timeout` knob — see the
  "History: the 2026-07-12 live proof" above). **Status pending empirical
  re-verification via the W5 stranger test**, which is the real launch gate —
  not yet run (the todo fixture's `nostos_live_test.dart` went with the
  fixture). The `nostos init
  --write-tables <tables>` flag at step 3 is what enables writes (the server
  allowlist defaults empty, ADR-0013); omitting it makes writes silently
  no-op, so always pass it.
- ~~**`Nostos` has no `close()`/`dispose()`**~~ — FIXED: `close()`, `disconnect()`, and
  `signOut()` all exist on the SDK (`sdk/nostos_flutter/rust/src/api/nostos.rs`;
  Dart wrappers). (Entry corrected 2026-08-17; the gap was real when
  written.)
- ~~**No client-visible write-rejection signal**~~ — FIXED: the dead-letter
  policy shipped (ADR-0027/ADR-0032 T5): a permanently rejected write is
  quarantined after `dead_letter_max_attempts` (default 50) and surfaces via
  `deadLetters()` with the server's per-row reason — no more silent
  retry-forever. (Entry corrected 2026-08-17.)
- **`nostos_flutter` forces a Rust build even for pure-Dart/mock-mode
  tests.** Once it's a `pubspec.yaml` dependency, `flutter test` resolves
  native assets for the whole package graph regardless of whether the test
  actually imports it — a contributor running only mock-mode tests still
  pays a Rust compile (cargo-fallback path, since no prebuilt binary exists
  yet) the first time. Not a correctness bug, but a CI-cost and
  onboarding-friction one.
- ~~**`nostos init` has no `--bind`/port flag**~~ — FIXED: `nostos init --bind`
  exists (`crates/nostos-cli/src/commands/init.rs`) and is wired into the
  generated `nostos.toml`. (Entry corrected 2026-08-17.)
- ~~**`nostos dev`'s printed Flutter snippet uses the wrong parameter
  names**~~ — FIXED: the banner prints `Nostos.connect(url: ..., token: ...)`,
  matching the SDK. (Entry corrected 2026-08-17.)
