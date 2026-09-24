# Nostos Operator Runbook

How to operate and debug a running `nostos-server`. For initial setup and
install steps, see [QUICKSTART.md](QUICKSTART.md); this doc is the triage and
operate companion. It exists to close audit P0 #3
(`docs/plans/nostos-soundness-audit-2026-07-19.md` §"P0→v0.2.1 Operator
playbook").

Scope: the Rust server + CLI + the Postgres logical-replication boundary it
depends on. Flutter / WASM client SDKs are out of scope here.

## 1. `nostos-server` environment

Every knob is a clap `#[arg]` with both a `--long` flag and a `NOSTOS_*` env
var (defined in `crates/nostos-server/src/main.rs:33-205`). Env wins when the
flag is absent; flag wins when present.

| var | default | effect |
|---|---|---|
| `NOSTOS_BIND` | `0.0.0.0:8800` | axum bind address. |
| `NOSTOS_WS_PATH` | `/sync` | WebSocket path clients connect to. |
| `NOSTOS_TRANSPORT` | `ws` | Sync transport (ADR-0041): `ws` = `/sync` over HTTP/WS on `NOSTOS_BIND`; `iroh` = sync sessions served natively over an iroh/QUIC endpoint, printing the QR-native `iroh://` dial URL. Requires a `--features iroh` build; off-default in every shipped artifact until ADR-0041's conditions clear. The HTTP ops surface binds `NOSTOS_BIND` in both modes. See §9. |
| `NOSTOS_IROH_RELAY_URL` | _empty_ | **Env-only (NOT a clap flag)** — read only in `--features iroh` builds under `NOSTOS_TRANSPORT=iroh`; kept out of clap so binaries without the feature don't advertise a knob they can't use. URL of a self-hosted iroh relay replacing n0's default relay fleet. Unparseable = startup-fatal. See §9. |
| `NOSTOS_SESSION_BUFFER` | `1024` | Per-session bounded channel depth; slow clients that fall further behind are dropped (explicit, observable — never silent OOM). |
| `NOSTOS_REPLICATOR` | `fake` | **Critical.** `fake` = synthetic generator (zero-setup). `pg` = real Postgres logical replication. Anything else bails: `unknown NOSTOS_REPLICATOR value: {other}` (`main.rs:466`). |
| `NOSTOS_PG_URL` | _empty_ | Postgres URL for `NOSTOS_REPLICATOR=pg`. Empty under `pg` bails fast (`main.rs:406`, `main.rs:497`). |
| `NOSTOS_FAKE_EPS` | `20` | Fake-replicator emission rate, events/sec. `0` = unbounded firehose. `fake` only; the benchmark builds its own config, so this never touches the moat numbers (A10). |
| `NOSTOS_FAKE_KEYS` | `50` | Fake-replicator distinct primary keys; `0` = monotonic (table grows forever). Client apply is an upsert on `(table, pk)`, so this bounds the *table* — which is what keeps a full-table watch snapshot O(1) in session length. `fake` only (A10). |
| `NOSTOS_WRITE_TABLES` | _empty_ | **Critical.** Comma-separated tables clients may write over `/sync` (ADR-0013). Empty = no tables writable — writes are rejected with `"table not writable: '<t>' — add it to NOSTOS_WRITE_TABLES"` (`crates/nostos-infra/src/transport.rs:792`). Demo needs `NOSTOS_WRITE_TABLES=tasks`. |
| `NOSTOS_PG_SLOT` | `cairn_slot` | Logical-replication slot name. Server creates it lazily on first connect if missing (see §2). |
| `NOSTOS_PG_PUBLICATION` | `cairn_pub` | Publication name. Must exist before `nostos dev` connects — `nostos init` creates it. |
| `NOSTOS_LOG` | `info,nostos=debug` | `RUST_LOG`-style filter. |
| `NOSTOS_OPLOG_BUFFER` | `4096` | Op-log writer's internal channel depth (ADR-0025 slice 2). Raise if `cairn_oplog_dropped_total > 0`. `pg` only. |
| `NOSTOS_OPLOG_RETENTION_SECS` | `3600` | Op-log row retention window (ADR-0025 slice 5). Offline gaps beyond this fall back to snapshot-reconcile. |
| `NOSTOS_OPLOG_COMPACT_INTERVAL_SECS` | `300` | Op-log compaction tick (ADR-0025 slice 5). |
| `NOSTOS_SLOT_MAX_LAG` | `1073741824` (1 GiB) | WAL-bloat eviction threshold (bytes). A live client lagging further than this is disconnected and resyncs; the slot is never dropped. `0` = eviction OFF (server warns at startup). Only protects the primary while nostos-server is running (ADR-0043). |
| `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` | `0` | Postgres `max_slot_wal_keep_size` (MB) — the DB-level WAL-bloat backstop and the ONLY bound on an abandoned slot (server gone). `0` = Postgres default (unbounded). Set in production; `nostos doctor` flags `-1` (ADR-0043). |
| `NOSTOS_SYNC_AUTH` | `none` | `/sync` auth mode (ADR-0010). `none` = anonymous (OSS dev, single-tenant only). `supabase-jwt` = verify a Supabase JWT (multi-tenant). |
| `NOSTOS_SUPABASE_JWT_SECRET` | _empty_ | Legacy HS256 Supabase JWT secret. Required-or-JWKS under `supabase-jwt`. |
| `NOSTOS_SUPABASE_URL` | _empty_ | Supabase project URL — derives the JWKS URL for RS256/ES256/EdDSA. |
| `NOSTOS_SUPABASE_JWKS_URL` | _empty_ | Explicit JWKS URL, overrides the one derived from `NOSTOS_SUPABASE_URL`. |
| `NOSTOS_TENANT_COLUMN` | `org_id` | Tenant column server-enforced on every predicate under `supabase-jwt` (ADR-0011). |
| `NOSTOS_CORS_ORIGINS` | _empty_ | Comma-separated allowed origins for browser clients. Empty = permissive (local dev). Set explicitly in production. |
| `NOSTOS_TIER` | `enterprise` | Licensing tier when no signed `NOSTOS_LICENSE` is present: `hobby`, `pro`, `scale`, `enterprise`. OSS self-host defaults to unlimited. |
| `NOSTOS_LICENSE` | _empty_ | Signed license token from Nostos Cloud. Invalid-but-present is fatal — the server refuses to silently downgrade (ADR-0006). |
| `NOSTOS_LICENSE_SECRET` | _empty_ | **Env-only (NOT a clap flag)** — signs every license a cloud deploy mints, so it must never land on argv / `ps` (`main.rs` constructs it via `std::env::var`). |
| `NOSTOS_ADMIN_TOKEN` | _empty_ | **Env-only (NOT a clap flag)**, same argv-leak reasoning as `NOSTOS_LICENSE_SECRET`. Bearer token gating `PUT /rules` (Task 21, ADR-0031 D5). Unset → the route **404s** (not mounted). Set → must be ≥32 chars or the server refuses to start (see §1.1(f)). See §7. |

### 1.1 Startup-failure modes (the ones that have bitten the demo)

Each of these is a "server starts, clients connect, something is silently
wrong" class — read the symptom carefully.

**(a) `NOSTOS_REPLICATOR` unset (defaults to `fake`) with `NOSTOS_PG_URL` set.**
The misconfiguration guard **bails at startup** (`main.rs`, C10 — the
`cfg.replicator != "pg" && !cfg.pg_url.trim().is_empty()` guard):

```
Error: NOSTOS_PG_URL is set but NOSTOS_REPLICATOR="fake" is not 'pg' —
snapshot-on-subscribe (ADR-0014) is OFF, so clients would silently receive
none of the table's pre-existing rows on connect. Set NOSTOS_REPLICATOR=pg,
or unset NOSTOS_PG_URL.
```

The server **refuses to start** (non-zero exit). This is deliberate (C10,
2026-07-20): the guard previously only `warn!`ed and let the server start
degraded — the `snapshotter` field stayed `None` (`main.rs:520-534`), so a
freshly-subscribing client received **zero** of the table's pre-existing rows
("connected but lists empty" / "5 in Postgres, only live inserts show"). The
bail makes the misconfiguration undiscoverable-by-accident. Fix: set
`NOSTOS_REPLICATOR=pg`.

**(b) `NOSTOS_WRITE_TABLES` empty.** Writes are silently no-op from the
client's perspective until you read the rejection frame: the transport rejects
every `ClientMessage::Write` with `"table not writable: '<t>' — add it to
NOSTOS_WRITE_TABLES"` (`crates/nostos-infra/src/transport.rs:792`,
`crates/nostos-server/src/main.rs:112`). Defense-in-depth at the SQL-injection
trust boundary (ADR-0013); empty-by-default is deliberate. Fix: add the table,
e.g. `NOSTOS_WRITE_TABLES=tasks,notes`.

**(c) `NOSTOS_REPLICATOR=pg` but `NOSTOS_PG_URL` empty.** Two bails fire,
both with actionable messages:

- `main.rs:406` — replicator cannot start: `"NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set ..."`.
- `main.rs:497` — write-back cannot start: `"NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back) ..."`.

> Line numbers in this document are hints, not anchors — they drift whenever
> `main.rs` gains a line. **Grep the quoted error string**, which is stable.

Fix: `docker compose -f docker/docker-compose.yml up -d` then
`NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn`.

**(d) `NOSTOS_SYNC_AUTH=supabase-jwt` with neither secret nor JWKS.** Bails at
`main.rs:277`: `"NOSTOS_SYNC_AUTH=supabase-jwt requires at least one of
NOSTOS_SUPABASE_JWT_SECRET (legacy HS256) or NOSTOS_SUPABASE_URL /
NOSTOS_SUPABASE_JWKS_URL"`. Fix: set one of the three.

**(e) Invalid `NOSTOS_LICENSE`.** Fatal at entitlement resolution
(`main.rs`, `nostos_license::resolve_entitlement`): `"NOSTOS_LICENSE
verification failed — refusing to start"`. Fix: re-issue from Nostos Cloud, or
unset `NOSTOS_LICENSE` to fall back to `NOSTOS_TIER`.

**(f) `NOSTOS_ADMIN_TOKEN` set but shorter than 32 chars.** Bails at startup
(`main.rs`, right after `init_tracing`): `"NOSTOS_ADMIN_TOKEN is set but only
{len} chars (minimum 32) — refusing to start rather than serve a guessable
admin route on PUT /rules"`. The message reports the length only, never the
token itself. Fix: generate a longer token (see §7).

## 2. Logical-replication slot

Nostos consumes Postgres logical replication via a single slot (default
`cairn_slot`, `NOSTOS_PG_SLOT`). A publication (default `cairn_pub`,
`NOSTOS_PG_PUBLICATION`) must already exist — `nostos init` creates it. The slot
itself is created lazily by `nostos-server` on first connect.

### 2.1 Auto-recovery: the `SlotProbe` trichotomy

On every connect, `PgReplicator::ensure_slot_and_publication` probes
`pg_replication_slots` and switches on a three-way classification
(`crates/nostos-infra/src/replicator/pg.rs:82-96`, probe body at `pg.rs:329-388`):

- **`Healthy { restart_lsn }`** — slot exists, `wal_status ∈ {reserved,
  extended}` (or any unknown future value — ponytail: a new PG major version
  adding a wal_status variant falls through to Healthy; the lag gauge and
  recreate counter remain the operator signal). WAL is retained; replication
  resumes from `confirmed_flush_lsn` (ADR-0009).
- **`Lost { slot_existed: false }`** — slot row MISSING. The retained WAL is
  gone.
- **`Lost { slot_existed: true }`** — slot row present but
  `wal_status = 'lost'` — Postgres evicted the WAL the slot needed.

Both `Lost` cases are the same data-loss class for our purposes. Recovery is
automatic: `ensure_slot_and_publication` drops the dead slot row (if present),
creates a fresh one, and emits a **snapshot-reconcile** pass so the client
catches up to the current table state. The client sees this as a reconnect +
fresh snapshot; no operator action required.

`pg_replication_slots.wal_status` reference (PG docs, cited at `pg.rs:334`):
`reserved`/`extended` = retained; `unreserved`/`lost` = WAL evicted.

### 2.2 Manual slot recreate

You normally never need this — §2.1 handles it. Use manual recreate when:

- `nostos-server` is down and you want a clean slate before restart,
- the slot name is wrong / collides with another consumer,
- Postgres itself refused the auto-recreate (e.g. `max_replication_slots`
  exhausted — check `nostos doctor` slot-headroom output first).

From a `psql` session on the source DB:

```sql
-- 1. drop the existing slot (idempotent — OK if missing)
SELECT pg_drop_replication_slot('cairn_slot')
  WHERE EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = 'cairn_slot');

-- 2. recreate it against the publication
SELECT pg_create_logical_replication_slot('cairn_slot', 'pgoutput');
```

(This is the SQL fallback documented in
`docs/plans/complete-nostos-fully-wired-operational.md:490`, kept as the
authoritative manual path. The default `nostos-server` path creates the slot
through `pgwire-replication` instead — both produce the same slot row.)

Then restart `nostos-server`. The first client subscribe triggers a full
snapshot (no `confirmed_flush_lsn` to resume from).

Pre-flight checks before recreate:

```sql
SELECT slot_name, wal_status, restart_lsn, confirmed_flush_lsn
  FROM pg_replication_slots WHERE slot_name = 'cairn_slot';
SELECT pubname FROM pg_publication WHERE pubname = 'cairn_pub';
```

If `wal_level` is not `logical`, the recreate will fail — fix with
`ALTER SYSTEM SET wal_level = logical; ALTER SYSTEM SET max_replication_slots = 10;`
and restart Postgres. The bundled `docker/docker-compose.yml` already sets
both (`wal_level=logical`, `max_replication_slots=10`).

## 3. "Connected but lists empty" — 5-line triage

Run this in order. Each line is **symptom → check → fix**.

1. **Is `NOSTOS_REPLICATOR=pg` actually set?**
   Symptom: clients connect, subscribe acks, zero rows arrive, only live
   inserts show.
   Check: `grep NOSTOS_REPLICATOR .env` or read server startup logs for
   `replicator: FakeReplicator (synthetic; 0 = unbounded)` vs
   `replicator: PgReplicator (real Postgres logical replication)`
   (`main.rs:397` / `main.rs:442`).
   Fix: `NOSTOS_REPLICATOR=pg`. See §1.1 (a).

2. **Is `NOSTOS_WRITE_TABLES` populated?**
   Symptom: writes succeed on the client (the SDK optimistically applies
   locally) but never land in Postgres; the WriteResult frame carries
   `"table not writable: '<t>' — add it to NOSTOS_WRITE_TABLES"`.
   Check: server log for `"write rejected: table not writable"`; or
   `grep NOSTOS_WRITE_TABLES .env`.
   Fix: `NOSTOS_WRITE_TABLES=tasks` (or the comma-separated set).
   Source: `crates/nostos-infra/src/transport.rs:792`.

3. **Supabase / IPv6 — is the WARP relay up?**
   Symptom: `PgReplicator` connect fails with `no route to host` against
   `db.<ref>.supabase.co`.
   Check: `dig +short AAAA db.<ref>.supabase.co` returns addresses but
   `dig +short A db.<ref>.supabase.co` is empty — the host is IPv6-only and
   your network has broken IPv6 egress.
   Fix: `SUPABASE_REF=<ref> ./scripts/warp-ipv6-egress.sh up` (userspace
   Cloudflare WARP via `wireproxy`, exposes
   `127.0.0.1:15433` → your Supabase host). Then set
   `NOSTOS_PG_URL='postgresql://postgres:<pw>@127.0.0.1:15433/postgres?sslmode=disable'`
   — nostos connects `NoTls`, so `sslmode=require` would break it
   (`docs/QUICKSTART.md` IPv6 warning; full background in
   `docs/QUICKSTART.md:180-197`). `./scripts/warp-ipv6-egress.sh down` stops
   the relay.
   Canonical check: `nostos doctor` prints the IPv6-only hint automatically
   (`crates/nostos-cli/src/commands/doctor.rs:ipv6_only_hint`).

4. **Is the replication slot healthy?**
   Symptom: server logs `pg_replication_slots.wal_status = 'lost' (WAL
   evicted; data-loss class)` (`pg.rs:360`); client reconnects but receives a
   fresh full snapshot every time instead of LSN resume.
   Check: `nostos doctor` (slot-status line), or directly:
   `SELECT slot_name, wal_status, restart_lsn, confirmed_flush_lsn FROM
   pg_replication_slots WHERE slot_name='cairn_slot';`.
   Fix: nothing — auto-recreate per §2.1 handles it. If it keeps recurring,
   check `NOSTOS_SLOT_MAX_LAG` is not `0` (default 1 GiB, ADR-0043) so lagging
   clients are evicted before Postgres evicts the WAL, and set
   `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` above the eviction threshold.

5. **Did the client receive a snapshot?**
   Symptom: client acks subscribe, then nothing; no error server-side.
   Check: server log for `snapshot-on-subscribe: PgSnapshotter (real source)`
   at startup (`main.rs:526`) — if absent, snapshotter is `None` (back to
   line 1). Under `pg`, also confirm the publication actually contains the
   table: `SELECT * FROM pg_publication_tables WHERE pubname='cairn_pub';`.
   Fix: re-run `nostos init` (it reconciles the publication's table set).

If all five pass and clients are still empty, capture: server log at
`NOSTOS_LOG=debug,nostos=trace`, the client's first three wire frames (the
subscribe + the first server frame), and the output of `nostos doctor`. File
an issue with those three artifacts.

## 4. CLI reference

Nostos ships two CLI binaries. `nostos` (the `nostos-cli` crate) is the
operator's entry point; `nostos-server` is the sync server itself. Both use
clap. Invoke through `cargo run -p <crate> --` during development; a release
build puts both on `$PATH` as `nostos` and `nostos-server`.

### 4.1 `nostos` (crates/nostos-cli/src/main.rs)

Top-level (`nostos-cli/src/main.rs:11-20`):

```
nostos — a local-first sync backend for Postgres + Supabase

Commands:
  init      Connect to Postgres, create/update the publication, write nostos.toml + .env
  dev       Run nostos-server locally using nostos.toml + .env
  doctor    Connectivity, replication health, and JWKS reachability checks
  deploy    Generate a self-host deploy config (fly/railway) from nostos.toml
  link      App-side: scaffold .nostos/ (config.json + gitignored local/)
  pull      App-side: fetch GET /schema → .nostos/schema.json
  gen       App-side: generate per-SDK source from .nostos/
```

`nostos init` flags (`nostos-cli/src/commands/init.rs:17-46`) — idempotent;
re-running reconciles the publication without erroring on what exists:

| flag | default | notes |
|---|---|---|
| `--db-url <URL>` | _prompted_ | Direct Postgres connection string (NOT the pooler). |
| `--tables <csv>` | _prompted_ | Tables to sync; also the publication's scope. |
| `--write-tables <csv>` | _empty_ | Must be a subset of `--tables`. Empty = read-only sync. |
| `--tenant-column <col>` | `org_id` | Enforced on every predicate under `supabase-jwt` (ADR-0011). |
| `--supabase-url <URL>` | _none_ | Derives the JWKS URL for `doctor` + auth. |
| `--publication <name>` | `cairn_pub` | |
| `--slot <name>` | `cairn_slot` | Records the name only — `nostos-server` creates the slot lazily. |
| `--bind <addr>` | `0.0.0.0:8800` | Written to `nostos.toml`. |

`nostos doctor` — read-only health checks. Runs: Postgres reachable,
`wal_level = logical`, publication exists + its table list, slot headroom
(`used/max`), slot status (`exists`, `lag_bytes`, `confirmed_flush_lsn`),
JWKS reachable (`crates/nostos-cli/src/commands/doctor.rs`). Emits the IPv6-only
hint on connect failure. Exits non-zero (`doctor found blocking issues`) if any
check fails — safe to wire into a deploy readiness gate. Never creates or
alters anything (that's `init`'s job).

`nostos dev` — runs `nostos-server` from the current project's `nostos.toml` +
`.env`. `docker/docker-compose.yml` should be up first if you want real
Postgres; otherwise set `NOSTOS_REPLICATOR=fake` for a synthetic stream.

`nostos deploy <args>` — generates a self-host config (fly / railway) from
`nostos.toml` (`nostos-cli/src/commands/deploy.rs`). Out of scope for triage;
see the deploy guide (TBD).

`nostos link` / `nostos pull` / `nostos gen` — app-side (Flutter / WASM) commands,
not used to operate the server. Documented for completeness; see
ADR-0023. One operator-facing note: when the server runs with
`NOSTOS_PROTECT_METADATA=1`, `nostos pull` needs `--token <TOKEN>` or the
`NOSTOS_TOKEN` env var to read `GET /schema` (`--token` wins when both are
set); the token goes through the same `NOSTOS_SYNC_AUTH` adapter as sync
clients, is never stored in `.nostos/config.json`, and is never printed. A
token is only sent over `https://` or to loopback (`127.0.0.1`, `localhost`,
`::1` — the `nostos dev` case); plain `http://` to any other host is refused
unless `--allow-insecure-token` is passed.

### 4.2 `nostos-server` (crates/nostos-server/src/main.rs:33-205)

The sync server binary. Every flag has an env-var equivalent (see §1 table).

```
nostos-server [OPTIONS]

OPTIONS (most-commonly-tuned; see §1 for the full table):
  --bind <ADDR>                         bind address              [env: NOSTOS_BIND, default: 0.0.0.0:8800]
  --replicator <fake|pg>                                          [env: NOSTOS_REPLICATOR, default: fake]
  --fake-events-per-sec <N>             0 = unbounded             [env: NOSTOS_FAKE_EPS, default: 20]
  --fake-distinct-keys <N>              0 = grows forever         [env: NOSTOS_FAKE_KEYS, default: 50]
  --pg-url <URL>                                                  [env: NOSTOS_PG_URL, default: -]
  --write-tables <CSV>                                            [env: NOSTOS_WRITE_TABLES, default: -]
  --pg-slot <NAME>                                               [env: NOSTOS_PG_SLOT, default: cairn_slot]
  --pg-publication <NAME>                                        [env: NOSTOS_PG_PUBLICATION, default: cairn_pub]
  --sync-auth <none|supabase-jwt>                                [env: NOSTOS_SYNC_AUTH, default: none]
  --log <FILTER>                                                 [env: NOSTOS_LOG, default: info,nostos=debug]
  --session-buffer <N>                                           [env: NOSTOS_SESSION_BUFFER, default: 1024]
  --slot-max-lag <BYTES>                                         [env: NOSTOS_SLOT_MAX_LAG, default: 1073741824]
  --pg-slot-wal-keep-size <MB>                                   [env: NOSTOS_PG_SLOT_WAL_KEEP_SIZE, default: 0]
  -h, --help              Print help
  -V, --version           Print version
```

Flags and env vars are interchangeable; clap's `#[arg(env = "...")]` makes
the env var act as the default for the flag. Pass `--help` for the full list
including the `NOSTOS_OPLOG_*` knobs (ADR-0025) and the auth/CORS flags.

## 5. Operational `make` targets

Defined in `Makefile`. The four you'll actually use triaging a deploy:

- **`make ci`** — `fmt-check + clippy (-D warnings) + full test suite`.
  The gate for every change.
- **`make dev-stack`** — real-Postgres quickstart: `docker compose up -d`,
  poll for the `cairn_pub` publication (not just `pg_isready` — the entrypoint
  restarts mid-init), then run `nostos-server` with `PgReplicator` against
  `NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn`. Ctrl-C stops
  the server.
- **`make pg-down`** — tear down the compose stack.
- **`make bench`** — the throughput benchmark. Record env, report drop rates;
  never compare eval-only numbers against end-to-end numbers
  (see [BENCHMARK-METHODOLOGY.md](BENCHMARK-METHODOLOGY.md)).

Real-Postgres e2e (when you suspect a regression at the PG boundary):

```
docker compose -f docker/docker-compose.yml up -d
NOSTOS_E2E_PG=1 \
NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn \
  cargo test -p nostos-infra --features pg
```

Without `NOSTOS_E2E_PG=1`, the `e2e_pg_*` tests **self-skip and report a
false-positive pass** (`crates/nostos-infra/tests/e2e_pg_snapshot.rs:43`,
`e2e_pg_schema.rs:28`, `e2e_pg_oplog_replay.rs:52`). Always set the flag for
a real run; a green result without it proves nothing.

## 6. Docker stack

`docker/docker-compose.yml` — single Postgres 16-alpine container:

- host port **`5433:5432`** (5433 on host to avoid colliding with a local PG),
- user / db / pass = **`nostos` / `nostos` / `nostos`**,
- `wal_level=logical`, `max_wal_senders=10`, `max_replication_slots=10`,
  `max_connections=200`,
- healthcheck on `pg_isready -U cairn -d cairn` (note: `make dev-stack` does
  NOT rely on this healthcheck — it polls for the `cairn_pub` publication
  directly, because the entrypoint runs a temporary server to apply
  pg-init scripts and then restarts, flipping accepting → rejecting →
  accepting),
- init scripts in `docker/pg-init/` — apply `01-sources.sql` (creates the
  source tables + `cairn_pub` publication) and `02-nostos-role.sql` (the
  least-privilege `cairn_writer` role the server connects as — ADR-0013/0018).

The bundled stack is for local dev only. Production points `NOSTOS_PG_URL` at
Supabase direct (see §3 line 3) or a self-hosted Postgres with the same
`wal_level`/slot settings.

## 7. Admin token (`PUT /rules`)

`PUT /rules` lets a caller rewrite the server's active sync ruleset — a
config-mutating route, deliberately gated separately from `/sync`'s
application-user auth (`NOSTOS_SYNC_AUTH`). See ADR-0031 (D5 addendum) for the
full reasoning; this section is the day-to-day operator procedure.

**Set it.** Generate 32+ random bytes and export as `NOSTOS_ADMIN_TOKEN`:

```
export NOSTOS_ADMIN_TOKEN=$(openssl rand -hex 32)
```

Leave it unset in any deployment that never needs to change rules at
runtime — the route then 404s, so there is nothing to attack. Set-but-short
(<32 chars) refuses to boot (§1.1(f)) rather than serve a guessable route.

**Use it.**

```
curl -X PUT https://your-server/rules \
  -H "Authorization: Bearer $NOSTOS_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sync_mode": "toggles", "tables": [...]}'
```

A Supabase (or any `/sync`) JWT is never accepted here, no matter how valid —
the two auth systems are intentionally unrelated.

**Rotate it.** Generate a new token the same way, update the server's env,
and restart. There is no overlap window: the old token stops working the
moment the new process starts. Rotate on any suspected leak (token in a
committed file, shared over an insecure channel, a departing operator who
had it) and on a routine schedule if your compliance posture calls for one.

**If it leaks:** rotate immediately (above), then read the audit log —
every successful mutation emits one `nostos::audit` line
(`rules_mutation actor=<8-hex> source=api mode_before=... mode_after=...
checksum_before=0x... checksum_after=0x... tables_changed=N`) — to see what,
if anything, an attacker changed while the old token was valid. `actor` is
the first 8 hex chars of SHA-256(token): stable per token, but not reversible
back to it, so it tells you *whether* a given token was used without ever
printing the token itself. Compare `mode_before`/`mode_after` and the
checksums against your own change history to spot anything you didn't make.

## 8. Sync rules

`nostos_rules.toml` (ADR-0031) decides what each authorised client is allowed
to read at all — layered *underneath* any client-side `where_sql`, which only
narrows further, per subscription. It has one `sync_mode`, one of:

- **`all`** — no gating; every replicated table reaches every client. The
  zero-config default when no `nostos_rules.toml` file exists. A fresh boot in
  this mode prints a warning naming every table and its estimated row count
  (`unknown rows (never analyzed)` if Postgres has no stats yet), ending with:
  ```
  This is the zero-config development default. For production, run
  `nostos rules init` and switch sync_mode to "toggles".
  ```
- **`toggles`** — per-table on/off plus an optional scope predicate. `nostos
  rules init` writes this mode with every table `sync = false`
  (`--sync-all` flips the default).
- **`hand`** — a raw `[[rules]]` predicate grammar (`column <op>
  claims.<field>` / `column <op> <literal>`, `AND`-only). Written and edited
  only via `nostos rules edit --mode hand`; `PUT /rules` refuses to touch it
  (below).

**Switching truth.** The file on disk is the only truth; `sync_mode` just
selects which part of it gets read. Switching modes never deletes or rewrites
the other mode's data — a `[[rules]]` hand section written once survives
untouched under `toggles` or `all`, so going back to `hand` later picks up
where you left it.

**What triggers a resync.** Any change to any *subscribed* table's
rule-decision — narrowing **or widening** — closes that client's socket and
makes it reconnect and re-snapshot from scratch. There is no in-place
predicate swap; a `nostos_rules.toml` edit is a coarse, whole-connection
invalidation, not a live re-scope (`crates/nostos-infra/src/transport.rs`).
Editing a table nobody has subscribed to costs nothing.

**Two authoring surfaces, one file, last writer wins.** `nostos rules edit`
(local CLI, terminal UI) and the web panel's `PUT /rules` (gated by
`NOSTOS_ADMIN_TOKEN`, see §7) both write the same `nostos_rules.toml` on the
server. There is no locking or optimistic-concurrency check between them: if
two edits race, whichever write lands last silently wins, full stop — check
`nostos rules check` (or reload the panel) after any edit made without
certainty you're the only editor. The web panel holds the admin token in the
browser tab's memory for that session only; it is never written to
localStorage or a cookie, closing the tab discards it. Persisting it would
turn the browser into an XSS target for a credential that can rewrite what
every client is allowed to read.

**Upgrading existing clients.** The first time any pre-ADR-0031 client
reconnects to an ADR-0031 server, it doesn't yet send `rules_checksum` in its
`Subscribe` — the server treats that as a checksum mismatch and forces one
full re-snapshot per client, one time. This is expected, not a regression;
say so in release notes so it isn't reported as a bug.

## 9. iroh transport: relays, discovery, and who sees what (ADR-0041)

`NOSTOS_TRANSPORT=iroh` (build with `cargo build -p nostos-server --features
iroh`) serves sync sessions natively over an iroh/QUIC endpoint instead of
HTTP/WS; the boot log prints a QR-native `iroh://…?ticket=…` dial URL. iroh
is **off by default** in every shipped artifact until ADR-0041's acceptance
conditions clear — treat it as preview.

**Third-party contact by default.** `presets::N0` — the endpoint preset
nostos uses (`crates/nostos-infra/src/iroh_sync.rs`) — talks to two n0
(number zero) services:

- **Relay fleet** (`RelayMode::Default`) — relays establish connectivity
  and carry traffic when NAT hole-punching fails (restrictive NATs and
  cellular commonly land here). Payloads are end-to-end encrypted QUIC with
  device-keyed TLS: the relay operator **cannot read sync data**, but sees
  connection metadata — endpoint IDs, source IPs, timing, byte volume.
- **Address discovery** (`iroh.link` DNS + pkarr publish/resolve) — the
  endpoint publishes its endpoint-id → relay/address mapping, signed by the
  endpoint key, and resolves peers through the same service.

The pairing ticket inside the dial URL carries the relay + direct-address
hints, so **pairing never depends on discovery** — discovery is a
re-resolution convenience, not a hard dependency.

**Self-hosting the relay.** Run iroh's relay server (the `iroh-relay` crate
ships the binary; nostos vendors iroh 1.1.0 — match versions):

- Dev: `iroh-relay --dev` — localhost, plain HTTP on :3340.
- Prod: config file with TLS certs or ACME; default ports 80/443 (HTTP/S),
  7842 (QUIC), 9090 (metrics) — the crate's `main.rs` / `defaults.rs`.

Then point the server at it:

```
NOSTOS_TRANSPORT=iroh NOSTOS_IROH_RELAY_URL=https://relay.example.com nostos-server
```

(`http://127.0.0.1:3340` against `--dev`.) An unparseable value is
startup-fatal (`invalid NOSTOS_IROH_RELAY_URL …`). Boot logs
`self-hosted relay replaces the n0 default fleet` naming the relay, and the
printed dial URL's ticket now carries YOUR relay — **unmodified clients
dial straight through it, no client-side config** (iroh's relay transport
treats any relay URL in the peer's address as dialable). iroh's built-in
`IROH_FORCE_STAGING_RELAYS` env (n0 staging fleet) is overridden when
`NOSTOS_IROH_RELAY_URL` is set.

**What self-hosting does NOT cut off today.**

- Discovery stays on n0's `iroh.link` even with a custom relay
  (`presets::N0` publishes regardless — `ponytail:` at
  `bind_sync_endpoint`). Pairing doesn't depend on it; a cutoff knob
  (`clear_address_lookup`) lands if an operator actually asks.
- A `nostos-client` default endpoint still homes onto the n0 fleet and
  publishes to `iroh.link` for its own addressing. A zero-third-party
  deployment needs client-side knobs that don't exist yet — tracked under
  ADR-0041's SDK-wiring item (D7), which keeps iroh off-default in shipped
  artifacts regardless.

## 10. References

- Setup / install: [QUICKSTART.md](QUICKSTART.md).
- Architecture / dependency rule: [ARCHITECTURE.md](ARCHITECTURE.md).
- Security model + vulnerability reporting: [SECURITY.md](../SECURITY.md).
- Throughput claims / how to verify them: [BENCHMARK-METHODOLOGY.md](BENCHMARK-METHODOLOGY.md).
- ADRs cited above: 0006 (license trust boundary), 0009 (ack-driven LSN
  resume), 0010 (sync auth), 0011 (server-enforced tenant predicates), 0013
  (write-back allowlist), 0016 (client WAL-bloat protection),
  0025 (persisted oplog backfill), 0031 (sync rules modes + checksum
  resync), 0041 (ws | iroh transport, relay/discovery third-party contact).
  See [adr/](adr/).
- Source code cited above: `crates/nostos-server/src/main.rs`,
  `crates/nostos-infra/src/transport.rs`,
  `crates/nostos-infra/src/iroh_sync.rs`,
  `crates/nostos-infra/src/replicator/pg.rs`,
  `crates/nostos-cli/src/{main.rs,commands/{init,doctor,dev,rules}.rs}`.
