# Atlet: nostos-vs-PowerSync App Suite — Implementation Plan (Flutter Pilot)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `apps/atlet/` — a benchmark-first training app (Atlet design) that runs nostos AND PowerSync behind one adapter against one Supabase DB, with an analytics tab producing internal-eval comparison numbers; Flutter is the pilot.

**Architecture:** One canonical Postgres schema on a single Supabase project; per-SDK Supabase auth users + RLS for isolation; both sync services run in a local docker-compose profile (cloud profile documented later); a thin per-language `SyncAdapter` with externally-observable instrumentation marks; a neutral JSONL bench store uploaded post-run via plain PostgREST.

**Tech Stack:** Flutter + `supabase_flutter`, `nostos_flutter` (local path dep), `powersync` (pub.dev), Docker (nostos-server + `journeyapps/powersync-service`), Supabase (auth, Postgres, PostgREST).

## Ratified decisions (operator-confirmed 2026-08-06 — do not relitigate)

1. **Benchmark first** — UI is a skin; fairness beats demo polish and doc prettiness.
2. **One canonical schema; per-SDK Supabase auth users + RLS** for isolation (no table-per-SDK).
3. **Topology as profiles** — `local` (both services in docker on one host) and `cloud` (documented, later); every recorded run tags its profile.
4. **Runtime engine toggle** in one app; full local wipe on switch; never both engines live at once.
5. **Analytics data never flows through either engine under test** — neutral local store, post-run upload via PostgREST to `analytics_runs`.
6. **Metrics = Core-4 + storage** in the pilot: cold initial sync, incremental propagation (server-clock-anchored), write→server-ack, offline queue drain, device storage bytes. Resource/Stress/UX groups are later slices. Reconnect/resume deferred to the Stress slice; battery/CPU dropped for a qualitative note.
7. **Layout `nostos/apps/atlet/`** — `spec/ supabase/ services/ design/` + one self-contained dir per SDK. NOT a Cargo workspace member; `make ci` and `sdk-e2e` must remain untouched.
8. **Pilot scope:** signin/otp + home + detail + shop (read-only bulk table) + analytics tab.
9. **Rollout after pilot:** RN+web (shared TS adapter) → kotlin+swift → node+capacitor+tauri → dotnet last. Each wave = its own follow-on plan; the adapter spec freezes at pilot retro.
10. **Numbers are internal-eval only**, labeled in-app; publication gated on FSL legal read + BENCHMARK-METHODOLOGY.md conformance + RESULTS.md landing.

**Research inputs:** `docs/plans/research-powersync-sdk-surface-2026-08-06.md`, `docs/plans/research-sync-benchmark-metrics-2026-08-06.md`, and (in flight) `docs/plans/research-powersync-perf-verification-2026-08-06.md`.

## Global Constraints

- The 833k/208×/417× moat multiples are **confirmed invalid (2026-08-06): a unit mismatch comparing nostos's fan-out rate to PowerSync's replication-ingest rate** — see `benches/results/RESULTS.md` §Correction and `docs/plans/research-powersync-perf-verification-2026-08-06.md`. Never cite them in any Atlet file, README, or UI copy. The app's analytics tab shows only its own measured runs plus the label: `Internal evaluation — not a published benchmark`.
- Instrumentation marks are defined by **externally observable contract** (row visible/absent via the adapter's normal read path) — never engine-internal callbacks or private state.
- Cross-machine latency anchors to the **server-populated `server_committed_at` column** (Postgres `now()`), with a per-run NTP-style offset estimate; client-only metrics use the client monotonic clock.
- `apps/atlet/**` never appears in the workspace `Cargo.toml`, `make ci`, or `sdk-e2e`.
- Design assets are **copied** from `/Volumes/developer_ssd/Developer/applications/asko/input/design` and frozen; the asko tree is never modified. Atlet design system rules apply (no off-palette colors, no emoji icons, frozen mark/wordmark).
- Direct-Postgres connections to Supabase are IPv6-only (`db.<ref>.supabase.co` is AAAA-only); on the dev VPN use `scripts/warp-ipv6-egress.sh` (127.0.0.1:15433, sslmode=disable). Both nostos-server and powersync-service need this in the local profile.
- Commits: single line, conventional prefix, no author mentions.
- Verify exact `nostos_flutter` signatures against `sdk/nostos_flutter/lib/src/nostos_database.dart` / `schema.dart` / `nostos.dart` before compiling adapter code; verify `powersync` package API against pub.dev docs for the installed version. Code blocks below are the intended shape, not gospel.

## File Structure

```
apps/atlet/
  README.md                      # what this is, isolation rules, numbers policy
  spec/
    adapter.md                   # SyncAdapter contract + marks + conformance checklist
    metrics.md                   # Core-4+storage definitions + clock policy
  design/                        # frozen copy of Atlet design (+ PROVENANCE.md)
  supabase/
    migrations/0001_atlet_schema.sql
    migrations/0002_powersync_replication.sql
    seed/products_seed.sql
    scripts/create_sdk_users.sh
  services/
    docker-compose.atlet.yml     # local profile: nostos-server + powersync-service
    powersync/config.yaml
    powersync/sync_rules.yaml
    .env.example
  flutter/                       # the pilot app (self-contained Flutter project)
    lib/adapters/sync_adapter.dart
    lib/adapters/nostos_adapter.dart
    lib/adapters/powersync_adapter.dart
    lib/bench/{marks.dart, clock.dart, store.dart, runner.dart, upload.dart}
    lib/ui/{signin.dart, home.dart, detail.dart, shop.dart, analytics.dart}
    lib/design/tokens.dart
    test/{adapter_conformance_test.dart, bench_math_test.dart, clock_test.dart}
```

---

### Task 1: Scaffold `apps/atlet/` + isolation guard

**Files:**
- Create: `apps/atlet/README.md`, `apps/atlet/.gitignore`

**Interfaces:**
- Produces: the directory layout above (empty dirs created on demand by later tasks).

- [ ] **Step 1: Create the tree and README**

```bash
mkdir -p apps/atlet/{spec,design,supabase/{migrations,seed,scripts},services/powersync,flutter}
```

`apps/atlet/README.md`:

```markdown
# Atlet — nostos vs PowerSync comparison suite

Benchmark-first training app (Atlet design) exercising every nostos SDK against
one Supabase database, with PowerSync behind the same adapter for neck-to-neck
internal evaluation.

## Isolation rules
- Not a Cargo workspace member. `make ci` and `sdk-e2e` never touch this tree.
- Each SDK app dir is fully self-contained (own lockfile, own build).

## Numbers policy
All numbers produced here are **internal evaluation — not a published benchmark**.
Publication requires: FSL legal review + docs/BENCHMARK-METHODOLOGY.md conformance
+ landing in benches/results/RESULTS.md.
```

`apps/atlet/.gitignore`:

```
**/build/
**/.dart_tool/
**/node_modules/
services/.env
```

- [ ] **Step 2: Verify isolation**

Run: `grep -n "apps" Cargo.toml Makefile | grep -v "^Binary" ; echo "exit=$?"`
Expected: no `apps/` references (grep exits 1).

- [ ] **Step 3: Commit**

```bash
git add apps/atlet
git commit -m "feat: scaffold apps/atlet comparison suite with isolation and numbers policy"
```

### Task 2: Freeze the design assets

**Files:**
- Create: `apps/atlet/design/**` (copied), `apps/atlet/design/PROVENANCE.md`

- [ ] **Step 1: Copy and freeze**

```bash
cp -R "/Volumes/developer_ssd/Developer/applications/asko/input/design/." apps/atlet/design/
cat > apps/atlet/design/PROVENANCE.md <<'EOF'
Frozen copy of the Atlet design (source: asko/input/design, copied 2026-08-06).
Do not edit; upstream asko tree is authoritative for design iteration.
Design-system rules (design-system.md) are gate-enforced for the apps.
EOF
```

- [ ] **Step 2: Verify key files landed**

Run: `ls apps/atlet/design/data_model.json apps/atlet/design/design-system.md apps/atlet/design/styles.css`
Expected: all three listed.

- [ ] **Step 3: Commit**

```bash
git add apps/atlet/design
git commit -m "feat: freeze Atlet design assets into apps/atlet/design with provenance"
```

### Task 3: Write the adapter + metrics specs

**Files:**
- Create: `apps/atlet/spec/adapter.md`, `apps/atlet/spec/metrics.md`

**Interfaces:**
- Produces: the contract every adapter implementation (Task 7+) and every future SDK port must satisfy.

- [ ] **Step 1: Write `spec/adapter.md`**

```markdown
# SyncAdapter contract v0 (frozen at pilot retro)

One adapter per (SDK, engine). The app and bench runner speak ONLY this surface.

## Operations
- `init(config)` — open local DB + connect engine using the signed-in Supabase
  session's access token. Idempotent.
- `signOut()` — disconnect + FULL local wipe (engine DB files deleted).
- `addSession(session) -> id` / `updateSession` / `deleteSession`
- `watchSessions() -> stream of ordered session lists` (normal read path)
- `watchProducts() -> stream` (read-only bulk table)
- `syncStatus() -> stream` (connected / syncing / offline, engine's own notion)
- `setConnected(bool)` — engine-level offline toggle for queue-drain runs.

## Instrumentation marks (externally observable ONLY)
A mark is legal iff it is derived from data visible through the adapter's normal
read path. Engine-internal callbacks/private state are forbidden (fairness rule).
- `localVisible(rowId, tMono)` — row first appears in `watchSessions` output.
- `serverAcked(rowId, tMono)` — the row's `server_committed_at` becomes non-null
  in `watchSessions` output (client inserts it as null; the server default fills
  it; the value syncs back — full round trip, engine-neutral).
- `remoteVisible(rowId, tMono, serverCommittedAt)` — a row created by the
  harness (via PostgREST) appears in `watchSessions` output.

## Conformance checklist (run per implementation)
1. init → signIn → addSession → serverAcked mark fires < 60s.
2. Row inserted via PostgREST appears (remoteVisible) < 60s.
3. setConnected(false) → 25 writes → setConnected(true) → all 25 serverAcked.
4. signOut wipes: local DB files absent; re-init cold-syncs from zero.
5. No adapter API leaks engine types into the app/bench layer.
```

- [ ] **Step 2: Write `spec/metrics.md`**

```markdown
# Metrics v0 — Core-4 + storage (pilot)

Clock policy: cross-machine intervals anchor to `server_committed_at`
(Postgres now(), sole authority) minus client wall clock corrected by a per-run
offset estimate (5× PostgREST `select now()` round trips; offset = median of
(server_now - client_mid_rtt)). Client-only intervals use the monotonic clock.

1. cold_sync_ms — signIn→init on wiped device until first watchSessions emission
   matching seeded row count. Report rows/sec alongside (seed size recorded).
2. propagation_ms — harness inserts row via PostgREST; value =
   client_wall(remoteVisible) - server_committed_at - clock_offset. N=25, report
   median/p95.
3. write_ack_ms — tMono(serverAcked) - tMono(addSession). N=25, median/p95.
4. queue_drain_ms — setConnected(false), 25 writes, setConnected(true); value =
   last serverAcked tMono - reconnect tMono.
5. db_bytes — engine's local DB file size(s) after cold sync + full drain, same
   checkpoint state (best effort; journal mode recorded).

Every run records: sdk, engine, profile (local|cloud), seed size, app version,
spec version, device model/os, timestamp. Label everywhere:
"Internal evaluation — not a published benchmark".
```

- [ ] **Step 3: Commit**

```bash
git add apps/atlet/spec
git commit -m "feat: freeze SyncAdapter contract and Core-4+storage metric definitions"
```

### Task 4: Supabase schema, RLS, seed, per-SDK users

**Files:**
- Create: `apps/atlet/supabase/migrations/0001_atlet_schema.sql`, `apps/atlet/supabase/migrations/0002_powersync_replication.sql`, `apps/atlet/supabase/seed/products_seed.sql`, `apps/atlet/supabase/scripts/create_sdk_users.sh`

**Interfaces:**
- Produces: tables `sessions`, `products`, `analytics_runs`; per-SDK users `{flutter,react_native,web,kotlin,swift,node,capacitor,tauri,dotnet}@atlet.internal`.

- [ ] **Step 1: Write `0001_atlet_schema.sql`**

```sql
-- Atlet canonical schema (decision #2: one schema, per-SDK auth users + RLS)
create table if not exists public.sessions (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid() references auth.users(id),
  title text not null,
  type text not null check (type in ('distance','reps','time')),
  metric int not null,
  unit text not null check (unit in ('km','reps','sec')),
  note text,
  streak int not null default 0,
  occurred_on date not null default current_date,
  -- clock authority for propagation metrics (spec/metrics.md); the client
  -- inserts NULL and the filled value syncing back IS the serverAcked mark.
  server_committed_at timestamptz default now()
);
create table if not exists public.products (
  id uuid primary key default gen_random_uuid(),
  name text not null,
  category text not null,
  price_cents int not null,
  rating numeric(2,1),
  plant_based boolean not null default false,
  image_url text
);
create table if not exists public.analytics_runs (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid() references auth.users(id),
  sdk text not null,
  engine text not null check (engine in ('cairn','powersync')),
  profile text not null check (profile in ('local','cloud')),
  run_type text not null,
  spec_version text not null,
  device jsonb not null,
  metrics jsonb not null,
  started_at timestamptz not null,
  uploaded_at timestamptz not null default now()
);
alter table public.sessions enable row level security;
alter table public.products enable row level security;
alter table public.analytics_runs enable row level security;
create policy sessions_own on public.sessions
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());
create policy products_read on public.products for select using (true);
create policy runs_own on public.analytics_runs
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());
create index sessions_user_occurred on public.sessions (user_id, occurred_on desc);
```

- [ ] **Step 2: Write `0002_powersync_replication.sql`** (per research doc: PowerSync needs a replication role + publication; nostos manages its own slot/publication — check nostos-server docs at wiring time)

```sql
-- PowerSync replication plumbing (research-powersync-sdk-surface-2026-08-06.md)
do $$ begin
  if not exists (select from pg_roles where rolname = 'powersync_role') then
    create role powersync_role with replication bypassrls login
      password :'POWERSYNC_ROLE_PASSWORD';
  end if;
end $$;
grant select on public.sessions, public.products to powersync_role;
do $$ begin
  if not exists (select from pg_publication where pubname = 'powersync') then
    create publication powersync for table public.sessions, public.products;
  end if;
end $$;
```

- [ ] **Step 3: Write `products_seed.sql`** — generate 1,000 rows (bulk-sync fixture; images from the frozen design's `img/` names for the first 6, placeholder URLs beyond)

```sql
insert into public.products (name, category, price_cents, rating, plant_based, image_url)
select
  'Product ' || g, (array['protein','hemp','oatshake','datenutbar','tartcherry','electrolyte'])[1 + g % 6],
  1500 + (g * 37) % 4000, round((3 + (g % 20) * 0.1)::numeric, 1), g % 3 = 0,
  'design/img/p' || (1 + g % 6) || '-' ||
  (array['protein','hemp','oatshake','datenutbar','tartcherry','electrolyte'])[1 + g % 6] || '.jpg'
from generate_series(1, 1000) g;
```

- [ ] **Step 4: Write `create_sdk_users.sh`** (Supabase Admin API; reads `SUPABASE_URL` + `SUPABASE_SERVICE_ROLE_KEY` from env)

```bash
#!/usr/bin/env bash
set -euo pipefail
: "${SUPABASE_URL:?}" "${SUPABASE_SERVICE_ROLE_KEY:?}" "${ATLET_SDK_USER_PASSWORD:?}"
for sdk in flutter react_native web kotlin swift node capacitor tauri dotnet; do
  curl -sf -X POST "$SUPABASE_URL/auth/v1/admin/users" \
    -H "apikey: $SUPABASE_SERVICE_ROLE_KEY" \
    -H "Authorization: Bearer $SUPABASE_SERVICE_ROLE_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"$sdk@atlet.internal\",\"password\":\"$ATLET_SDK_USER_PASSWORD\",\"email_confirm\":true}" \
    && echo " created: $sdk@atlet.internal" || echo " exists/failed: $sdk (check manually)"
done
```

(Note: this script runs OUTSIDE the app; the curl-block rule in CLAUDE.md applies to agent Bash calls — executing this script is done by the operator or via `ctx_execute`.)

- [ ] **Step 5: Smoke-apply 0001 against the local nostos Postgres** (schema-validity check only; auth.uid() requires Supabase — expect the two `auth.` references to fail locally, so validate with a syntax-only pass)

Run: `docker compose -f docker/docker-compose.yml up -d && docker exec -i nostos-postgres psql -U cairn -d cairn -c 'select 1'`
Then: apply `0001` to the real Supabase project via the Supabase MCP `apply_migration` tool (name `atlet_0001_schema`), `0002` via `execute_sql` with the password variable substituted, seed via `execute_sql`.
Expected: tables visible in `list_tables`; `select count(*) from products` = 1000.

- [ ] **Step 6: Run `create_sdk_users.sh`** (operator/ctx_execute) and verify: sign-in with `flutter@atlet.internal` returns a session (PostgREST `/auth/v1/token?grant_type=password`).

- [ ] **Step 7: Commit**

```bash
git add apps/atlet/supabase
git commit -m "feat: add Atlet canonical schema, RLS, powersync replication plumbing, seed, and per-SDK users script"
```

### Task 5: Local-profile services compose

**Files:**
- Create: `apps/atlet/services/docker-compose.atlet.yml`, `apps/atlet/services/powersync/config.yaml`, `apps/atlet/services/powersync/sync_rules.yaml`, `apps/atlet/services/.env.example`

**Interfaces:**
- Produces: `nostos-server` on :8080 (ws) and `powersync` on :8081 (http), both pointed at the one Supabase Postgres.

- [ ] **Step 1: Write `.env.example`**

```
# Direct Postgres (IPv6-only from Supabase; via WARP relay use 127.0.0.1:15433 sslmode=disable)
ATLET_PG_URL=postgres://postgres:PASSWORD@db.PROJECT_REF.supabase.co:5432/postgres
POWERSYNC_PG_URL=postgres://powersync_role:PASSWORD@db.PROJECT_REF.supabase.co:5432/postgres
SUPABASE_URL=https://PROJECT_REF.supabase.co
SUPABASE_JWT_SECRET=...   # powersync verifies Supabase JWTs with this (legacy) or JWKS URL
NOSTOS_WRITE_TABLES=sessions
```

- [ ] **Step 2: Write `docker-compose.atlet.yml`**

```yaml
name: atlet-services
services:
  nostos-server:
    build: { context: ../../.., dockerfile: docker/Dockerfile }  # verify path vs existing docker/
    environment:
      NOSTOS_REPLICATOR: pg
      NOSTOS_PG_URL: ${ATLET_PG_URL}
      NOSTOS_WRITE_TABLES: ${NOSTOS_WRITE_TABLES}
      NOSTOS_SUPABASE_URL: ${SUPABASE_URL}   # verify actual env names in nostos-server config
    ports: ["8080:8080"]
  powersync:
    image: journeyapps/powersync-service:latest
    command: ["start", "-r", "unified"]
    environment:
      POWERSYNC_CONFIG_PATH: /config/config.yaml
    volumes: ["./powersync:/config:ro"]
    ports: ["8081:80"]
```

- [ ] **Step 3: Write `powersync/config.yaml` + `sync_rules.yaml`** (shapes from the research doc; verify keys against powersync-ja/self-host-demo at wiring time)

```yaml
# config.yaml
replication:
  connections:
    - type: postgresql
      uri: !env POWERSYNC_PG_URL
storage:
  type: postgresql
  uri: !env POWERSYNC_PG_URL
client_auth:
  supabase: true
  supabase_jwt_secret: !env SUPABASE_JWT_SECRET
sync_rules:
  path: /config/sync_rules.yaml
```

```yaml
# sync_rules.yaml — mirrors the RLS scope (decision #2)
bucket_definitions:
  user_sessions:
    parameters: select request.user_id() as user_id
    data:
      - select * from sessions where user_id = bucket.user_id
  catalog:
    data:
      - select * from products
```

- [ ] **Step 4: Validate + boot**

Run: `cd apps/atlet/services && cp .env.example .env` (fill real values) `&& docker compose -f docker-compose.atlet.yml config -q && docker compose -f docker-compose.atlet.yml up -d`
Expected: `config -q` silent; both containers healthy; powersync logs show replication slot created; nostos-server logs show pg replicator connected. (Slot budget: Supabase default max_replication_slots — nostos + powersync each take one; prune leaked `e2e_*` slots if exhausted.)

- [ ] **Step 5: Commit**

```bash
git add apps/atlet/services
git commit -m "feat: add local-profile docker compose for nostos-server and self-hosted powersync"
```

### Task 6: Flutter app scaffold + auth + design tokens

**Files:**
- Create: `apps/atlet/flutter/` (via `flutter create`), `lib/design/tokens.dart`, `lib/ui/signin.dart`, `lib/main.dart`

**Interfaces:**
- Produces: `AtletApp` with Supabase-authenticated shell; `AtletTokens` (colors/typography from design-system.md); routes `/signin → /home`.

- [ ] **Step 1: Scaffold + deps**

Run (in `apps/atlet/flutter`): `flutter create --org internal.atlet --project-name atlet .`
`pubspec.yaml` deps: `supabase_flutter`, `nostos_flutter: { path: ../../../sdk/nostos_flutter }`, `powersync`, `path_provider`. Run `flutter pub get`.

- [ ] **Step 2: `lib/design/tokens.dart`** — transcribe design-system.md verbatim

```dart
import 'package:flutter/material.dart';
abstract final class AtletTokens {
  static const bone = Color(0xFFF5F0E8);
  static const bone2 = Color(0xFFEAE3D6);
  static const paper = Color(0xFFFBF8F2);
  static const ink = Color(0xFF1A1714);
  static const ink3 = Color(0xFF6E6760);
  static const rule = Color(0xFFD8CFBE);
  static const accent = Color(0xFFD2522B);
  static const accent2 = Color(0xFFB8431F);
  static const good = Color(0xFF4A7C3A);
  static const warn = Color(0xFFC68D2E);
  // Sans: Lexend 300-700; Mono numerals: JetBrains Mono. HIG: 34/22/17/13.
}
```

- [ ] **Step 3: signin/otp** — `supabase_flutter` password sign-in for `flutter@atlet.internal` (per-SDK user), with the design's signin layout; OTP screen wired to Supabase email OTP but password path is the default for bench runs (deterministic).

- [ ] **Step 4: Verify** — `flutter analyze` clean; `flutter run -d macos` (or booted sim) reaches home shell after sign-in.

- [ ] **Step 5: Commit**

```bash
git add apps/atlet/flutter
git commit -m "feat: scaffold Atlet flutter pilot with supabase auth and design tokens"
```

### Task 7: `SyncAdapter` interface + marks + conformance harness

**Files:**
- Create: `lib/adapters/sync_adapter.dart`, `lib/bench/marks.dart`, `test/adapter_conformance_test.dart`

**Interfaces:**
- Produces (used by Tasks 8–15):

```dart
class SessionRow {
  final String id; final String title; final String type; final int metric;
  final String unit; final String? note; final int streak;
  final DateTime occurredOn; final DateTime? serverCommittedAt;
  const SessionRow({required this.id, required this.title, required this.type,
    required this.metric, required this.unit, this.note, this.streak = 0,
    required this.occurredOn, this.serverCommittedAt});
}
class ProductRow { final String id; final String name; final String category;
  final int priceCents; final double? rating; final bool plantBased;
  final String? imageUrl; const ProductRow({/* all fields, same pattern */}); }

enum MarkKind { localVisible, serverAcked, remoteVisible }
class SyncMark {
  final MarkKind kind; final String rowId; final Duration tMono; // from bench clock
  final DateTime? serverCommittedAt;
  const SyncMark(this.kind, this.rowId, this.tMono, {this.serverCommittedAt});
}

abstract interface class SyncAdapter {
  String get engine; // 'cairn' | 'powersync'
  Future<void> init({required String supabaseUrl, required String accessToken,
    required String userId, required String dbDir});
  Future<void> signOut(); // disconnect + delete local DB files (full wipe)
  Future<String> addSession(SessionRow s); // serverCommittedAt must be null
  Future<void> deleteSession(String id);
  Stream<List<SessionRow>> watchSessions();
  Stream<List<ProductRow>> watchProducts();
  Stream<bool> get connected;
  Future<void> setConnected(bool up);
  Stream<SyncMark> get marks; // derived ONLY from watchSessions output
}
```

- [ ] **Step 1: Write the mark-derivation helper** (engine-neutral — this is what makes marks "externally observable by construction"): a `MarkDeriver` that consumes `watchSessions()` emissions and a monotonic clock, and emits `localVisible` (id first seen), `serverAcked` (serverCommittedAt transitions null→non-null), `remoteVisible` (id first seen AND serverCommittedAt already non-null AND id ∉ locally-added set).

```dart
class MarkDeriver {
  final Stopwatch clock; final _seen = <String>{}; final _acked = <String>{};
  final localIds = <String>{}; // adapter adds ids from addSession
  final _out = StreamController<SyncMark>.broadcast();
  Stream<SyncMark> get marks => _out.stream;
  MarkDeriver(this.clock);
  void onEmission(List<SessionRow> rows) {
    final t = clock.elapsed;
    for (final r in rows) {
      if (_seen.add(r.id)) {
        if (localIds.contains(r.id)) { _out.add(SyncMark(MarkKind.localVisible, r.id, t)); }
        else if (r.serverCommittedAt != null) {
          _out.add(SyncMark(MarkKind.remoteVisible, r.id, t, serverCommittedAt: r.serverCommittedAt));
        }
      }
      if (r.serverCommittedAt != null && _acked.add(r.id) && localIds.contains(r.id)) {
        _out.add(SyncMark(MarkKind.serverAcked, r.id, t, serverCommittedAt: r.serverCommittedAt));
      }
    }
  }
}
```

- [ ] **Step 2: Write the failing conformance test** against a `FakeAdapter` (in-memory impl of `SyncAdapter` that simulates ack after a delay) asserting: localVisible→serverAcked ordering per row; remoteVisible for externally-injected rows; signOut resets derivation state.

- [ ] **Step 3: Run** `flutter test test/adapter_conformance_test.dart` — FAIL (types absent) → implement → PASS.

- [ ] **Step 4: Commit** — `git commit -m "feat: define SyncAdapter contract with externally-observable mark derivation and conformance tests"`

### Task 8: Bench clock, neutral store, runner math

**Files:**
- Create: `lib/bench/clock.dart`, `lib/bench/store.dart`, `lib/bench/runner.dart`, `test/bench_math_test.dart`, `test/clock_test.dart`

**Interfaces:**
- Produces: `BenchClock.estimateOffset(SupabaseClient) -> Duration` (median of 5 `select now()` probes, RTT/2-corrected); `BenchStore.append(RunRecord)` (JSONL under app documents dir — ponytail: no DB for the neutral store, a file is enough); `Runner.coldSync/propagation/writeAck/queueDrain(adapter) -> RunRecord` implementing spec/metrics.md exactly (N=25, median+p95, seed size recorded); `RunRecord.toJson()` matching the `analytics_runs` row shape.

- [ ] **Step 1: Failing tests first**: median/p95 math on known vectors; offset estimator with a fake probe function (server ahead by 120ms, RTT 40ms → offset ≈ 120ms ± 5ms); JSONL round-trip.
- [ ] **Step 2: Implement minimal code; `flutter test` PASS.**
- [ ] **Step 3: Commit** — `git commit -m "feat: add bench clock offset estimation, neutral JSONL store, and core-4 runner math"`

### Task 9: NostosAdapter

**Files:**
- Create: `lib/adapters/nostos_adapter.dart`
- Read first: `sdk/nostos_flutter/lib/src/nostos_database.dart`, `schema.dart`, `nostos.dart`, and the SDK README (exact `NostosSchema`/`NostosConfig`/connect/watch/signOut signatures).

**Shape (verify names before compiling):**

```dart
class NostosAdapter implements SyncAdapter {
  @override final engine = 'cairn';
  late final NostosDatabase _db; final _deriver = MarkDeriver(Stopwatch()..start());
  @override
  Future<void> init({required supabaseUrl, required accessToken,
      required userId, required dbDir}) async {
    final schema = NostosSchema(tables: [
      NostosTable('sessions', columns: [/* map SessionRow fields */]),
      NostosTable('products', columns: [/* map ProductRow fields */]),
    ]);
    _db = await NostosDatabase.open(
      config: NostosConfig(nostosUrl: 'ws://localhost:8080', sqlitePath: '$dbDir/cairn.db'),
      schema: schema, accessToken: accessToken);
    _db.watch('sessions' /* verify query surface */)
       .map(_rowsToSessions).listen(_deriver.onEmission);
  }
  // addSession -> _db execute/collection write with server_committed_at omitted
  // signOut -> _db.signOut() (full wipe per WS4) + delete files if any remain
  // setConnected -> verify: engine-level disconnect/connect API; if absent,
  //   ponytail: close/reopen connection — record method in RunRecord.
}
```

- [ ] Steps: read SDK sources → implement → run conformance checklist items 1,4,5 live against the local profile (services from Task 5, `NOSTOS_WRITE_TABLES=sessions`) → commit `feat: implement nostos adapter for atlet pilot`.

### Task 10: PowerSyncAdapter

**Files:**
- Create: `lib/adapters/powersync_adapter.dart`
- Docs: research-powersync-sdk-surface-2026-08-06.md + pub.dev `powersync` API for the installed version.

**Shape:**

```dart
class PowerSyncAdapter implements SyncAdapter {
  @override final engine = 'powersync';
  late final PowerSyncDatabase _db;
  @override
  Future<void> init({...}) async {
    _db = PowerSyncDatabase(schema: Schema([
      Table('sessions', [Column.text('title'), Column.text('type'),
        Column.integer('metric'), Column.text('unit'), Column.text('note'),
        Column.integer('streak'), Column.text('occurred_on'),
        Column.text('server_committed_at'), Column.text('user_id')]),
      Table('products', [/* ... */]),
    ]), path: '$dbDir/powersync.db');
    await _db.initialize();
    await _db.connect(connector: _SupabaseConnector(supabaseUrl, accessToken));
    _db.watch('select * from sessions order by occurred_on desc')
       .map(_rowsToSessions).listen(_deriver.onEmission);
  }
}
class _SupabaseConnector extends PowerSyncBackendConnector {
  @override Future<PowerSyncCredentials?> fetchCredentials() async =>
      PowerSyncCredentials(endpoint: 'http://localhost:8081', token: _accessToken);
  @override Future<void> uploadData(PowerSyncDatabase db) async {
    final tx = await db.getNextCrudTransaction(); if (tx == null) return;
    for (final op in tx.crud) { /* op -> supabase.from(op.table).upsert/delete */ }
    await tx.complete();
  }
}
```

- [ ] Steps: implement → conformance items 1,2,4,5 against local powersync service → commit `feat: implement powersync adapter for atlet pilot`.

### Task 11: Engine toggle + wipe flow

**Files:**
- Create: `lib/engine_registry.dart`; Modify: `lib/main.dart`

- [ ] Toggle in settings sheet: switching engines runs `currentAdapter.signOut()` (full wipe), constructs the other adapter, `init()` fresh (decision #4: never both live). Assert in code: only one adapter instance non-null at a time. Verify by switching twice on device; nostos DB files absent while powersync active and vice versa. Commit `feat: add runtime engine toggle with full wipe between engines`.

### Task 12: Training UI (home + detail)

**Files:**
- Create: `lib/ui/home.dart`, `lib/ui/detail.dart`

- [ ] Home: session list from `watchSessions()`, streak chip, add-session sheet (type/metric/unit per design enums w1–w5 fixtures). Detail: session view + complete/delete. All rendering exclusively from watch streams (offline-first proof: airplane-mode add still renders instantly). Follow `apps/atlet/design/page-contexts.json` home/detail layouts + tokens; no off-palette colors. Verify with `flutter run` + airplane-mode smoke. Commit `feat: add training home and detail surfaces over adapter watch streams`.

### Task 13: Shop (bulk read-only table)

**Files:**
- Create: `lib/ui/shop.dart`

- [ ] Product grid from `watchProducts()` (1k seeded rows), images from bundled design assets. This is the cold-sync/stress fixture with real UI. Commit `feat: add shop surface backed by bulk read-only products sync`.

### Task 14: Bench runner wiring (Core-4 + storage)

**Files:**
- Create: `lib/bench/harness.dart`; Modify: `lib/bench/runner.dart`

- [ ] Wire the four runs + db_bytes against the live adapter: propagation run inserts via PostgREST (`supabase.from('sessions').insert(...)` using the same signed-in user REST session — NOT through either engine); each run ends with `BenchStore.append`. Run one full suite per engine on the local profile; records land in JSONL. Commit `feat: wire core-4 plus storage bench runs through the adapter marks`.

### Task 15: Analytics tab + upload

**Files:**
- Create: `lib/ui/analytics.dart`, `lib/bench/upload.dart`

- [ ] Tab 2: run launcher + results table (engine × metric, median/p95), permanent banner `Internal evaluation — not a published benchmark` (decision #10). Upload button posts JSONL records to `analytics_runs` via PostgREST. Verify a row lands in Supabase (`select count(*) from analytics_runs`). Commit `feat: add analytics tab with internal-eval labeling and postgrest run upload`.

### Task 16: Conformance sign-off + pilot retro gate

- [ ] Run the full `spec/adapter.md` conformance checklist for BOTH adapters on the local profile; record results in `apps/atlet/spec/conformance-flutter.md` (pass/fail per item, dates, versions).
- [ ] `flutter analyze` + `flutter test` clean; `make ci` still green and untouched by this tree (isolation proof).
- [ ] Write pilot retro (what the spec got wrong) → freeze `spec/adapter.md` v1 → THEN open the wave-2 plan (RN+web shared TS adapter). Commit `docs: record flutter pilot conformance results and freeze adapter spec v1`.

---

## Open items (carried, not in-scope here)

1. **Moat denominator — RESOLVED 2026-08-06.** `research-powersync-perf-verification-2026-08-06.md` found the 208×/417× multiples to be a unit mismatch (nostos's aggregate fan-out vs PowerSync's replication-ingest rate — different pipeline stages); the multiples are retired repo-wide (see `benches/results/RESULTS.md` §Correction). Those numbers remain quarantined here regardless — still not cited anywhere in Atlet.
2. **FSL legal read** before any external publication of comparison numbers.
3. **Cloud profile** (Fly nostos + PowerSync Cloud) — documented stub only until local profile numbers are stable.
4. **Waves 2–5** — separate plans after spec v1 freeze.
