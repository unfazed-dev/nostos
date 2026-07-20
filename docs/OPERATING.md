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
var (defined in `crates/nostos-server/src/main.rs:33-167`). Env wins when the
flag is absent; flag wins when present.

| var | default | effect |
|---|---|---|
| `NOSTOS_BIND` | `0.0.0.0:8800` | axum bind address. |
| `NOSTOS_WS_PATH` | `/sync` | WebSocket path clients connect to. |
| `NOSTOS_SESSION_BUFFER` | `1024` | Per-session bounded channel depth; slow clients that fall further behind are dropped (explicit, observable — never silent OOM). |
| `NOSTOS_REPLICATOR` | `fake` | **Critical.** `fake` = synthetic generator (zero-setup). `pg` = real Postgres logical replication. Anything else bails: `unknown NOSTOS_REPLICATOR value: {other}` (`main.rs:427`). |
| `NOSTOS_PG_URL` | _empty_ | Postgres URL for `NOSTOS_REPLICATOR=pg`. Empty under `pg` bails fast (`main.rs:370`, `main.rs:457`). |
| `NOSTOS_WRITE_TABLES` | _empty_ | **Critical.** Comma-separated tables clients may write over `/sync` (ADR-0013). Empty = no tables writable — writes are rejected with `"table not writable: '<t>' — add it to NOSTOS_WRITE_TABLES"` (`crates/nostos-infra/src/transport.rs:792`). Demo needs `NOSTOS_WRITE_TABLES=tasks`. |
| `NOSTOS_PG_SLOT` | `cairn_slot` | Logical-replication slot name. Server creates it lazily on first connect if missing (see §2). |
| `NOSTOS_PG_PUBLICATION` | `cairn_pub` | Publication name. Must exist before `nostos dev` connects — `nostos init` creates it. |
| `NOSTOS_LOG` | `info,nostos=debug` | `RUST_LOG`-style filter. |
| `NOSTOS_OPLOG_BUFFER` | `4096` | Op-log writer's internal channel depth (ADR-0025 slice 2). Raise if `cairn_oplog_dropped_total > 0`. `pg` only. |
| `NOSTOS_OPLOG_RETENTION_SECS` | `3600` | Op-log row retention window (ADR-0025 slice 5). Offline gaps beyond this fall back to snapshot-reconcile. |
| `NOSTOS_OPLOG_COMPACT_INTERVAL_SECS` | `300` | Op-log compaction tick (ADR-0025 slice 5). |
| `NOSTOS_SLOT_MAX_LAG` | `0` | WAL-bloat eviction threshold (bytes). `0` = eviction OFF. A production deploy MUST set this AND `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` (ADR-0016). |
| `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` | `0` | Postgres `max_slot_wal_keep_size` (MB) — the DB-level WAL-bloat backstop. `0` = Postgres default (unbounded). |
| `NOSTOS_SYNC_AUTH` | `none` | `/sync` auth mode (ADR-0010). `none` = anonymous (OSS dev, single-tenant only). `supabase-jwt` = verify a Supabase JWT (multi-tenant). |
| `NOSTOS_SUPABASE_JWT_SECRET` | _empty_ | Legacy HS256 Supabase JWT secret. Required-or-JWKS under `supabase-jwt`. |
| `NOSTOS_SUPABASE_URL` | _empty_ | Supabase project URL — derives the JWKS URL for RS256/ES256/EdDSA. |
| `NOSTOS_SUPABASE_JWKS_URL` | _empty_ | Explicit JWKS URL, overrides the one derived from `NOSTOS_SUPABASE_URL`. |
| `NOSTOS_TENANT_COLUMN` | `org_id` | Tenant column server-enforced on every predicate under `supabase-jwt` (ADR-0011). |
| `NOSTOS_CORS_ORIGINS` | _empty_ | Comma-separated allowed origins for browser clients. Empty = permissive (local dev). Set explicitly in production. |
| `NOSTOS_TIER` | `enterprise` | Licensing tier when no signed `NOSTOS_LICENSE` is present: `hobby`, `pro`, `scale`, `enterprise`. OSS self-host defaults to unlimited. |
| `NOSTOS_LICENSE` | _empty_ | Signed license token from Nostos Cloud. Invalid-but-present is fatal — the server refuses to silently downgrade (ADR-0006). |
| `NOSTOS_LICENSE_SECRET` | _empty_ | **Env-only (NOT a clap flag)** — signs every license a cloud deploy mints, so it must never land on argv / `ps` (`main.rs` constructs it via `std::env::var`). |

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
degraded — the `snapshotter` field stayed `None` (`main.rs:488-494`), so a
freshly-subscribing client received **zero** of the table's pre-existing rows
("connected but lists empty" / "5 in Postgres, only live inserts show"). The
bail makes the misconfiguration undiscoverable-by-accident. Fix: set
`NOSTOS_REPLICATOR=pg`.

**(b) `NOSTOS_WRITE_TABLES` empty.** Writes are silently no-op from the
client's perspective until you read the rejection frame: the transport rejects
every `ClientMessage::Write` with `"table not writable: '<t>' — add it to
NOSTOS_WRITE_TABLES"` (`crates/nostos-infra/src/transport.rs:792`,
`crates/nostos-server/src/main.rs:94`). Defense-in-depth at the SQL-injection
trust boundary (ADR-0013); empty-by-default is deliberate. Fix: add the table,
e.g. `NOSTOS_WRITE_TABLES=tasks,notes`.

**(c) `NOSTOS_REPLICATOR=pg` but `NOSTOS_PG_URL` empty.** Two bails fire,
both with actionable messages:

- `main.rs:370` — replicator cannot start: `"NOSTOS_REPLICATOR=pg requires NOSTOS_PG_URL ..."`.
- `main.rs:457` — write-back cannot start: `"NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back) ..."`.

Fix: `docker compose -f docker/docker-compose.yml up -d` then
`NOSTOS_PG_URL=postgresql://cairn:cairn@localhost:5433/cairn`.

**(d) `NOSTOS_SYNC_AUTH=supabase-jwt` with neither secret nor JWKS.** Bails at
`main.rs:258`: `"NOSTOS_SYNC_AUTH=supabase-jwt requires at least one of
NOSTOS_SUPABASE_JWT_SECRET (legacy HS256) or NOSTOS_SUPABASE_URL /
NOSTOS_SUPABASE_JWKS_URL"`. Fix: set one of the three.

**(e) Invalid `NOSTOS_LICENSE`.** Fatal at entitlement resolution
(`main.rs`, `nostos_license::resolve_entitlement`): `"NOSTOS_LICENSE
verification failed — refusing to start"`. Fix: re-issue from Nostos Cloud, or
unset `NOSTOS_LICENSE` to fall back to `NOSTOS_TIER`.

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
   `replicator: FakeReplicator (synthetic, unbounded)` vs
   `replicator: PgReplicator (real Postgres logical replication)`
   (`main.rs:363` / `main.rs:407`).
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
   set `NOSTOS_SLOT_MAX_LAG` and `NOSTOS_PG_SLOT_WAL_KEEP_SIZE` (ADR-0016) so
   lagging clients are evicted before Postgres evicts the WAL.

5. **Did the client receive a snapshot?**
   Symptom: client acks subscribe, then nothing; no error server-side.
   Check: server log for `snapshot-on-subscribe: PgSnapshotter (real source)`
   at startup (`main.rs:493`) — if absent, snapshotter is `None` (back to
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
ADR-0023.

### 4.2 `nostos-server` (crates/nostos-server/src/main.rs:33-167)

The sync server binary. Every flag has an env-var equivalent (see §1 table).

```
nostos-server [OPTIONS]

OPTIONS (most-commonly-tuned; see §1 for the full table):
  --bind <ADDR>                         bind address              [env: NOSTOS_BIND, default: 0.0.0.0:8800]
  --replicator <fake|pg>                                          [env: NOSTOS_REPLICATOR, default: fake]
  --pg-url <URL>                                                  [env: NOSTOS_PG_URL, default: -]
  --write-tables <CSV>                                            [env: NOSTOS_WRITE_TABLES, default: -]
  --pg-slot <NAME>                                               [env: NOSTOS_PG_SLOT, default: cairn_slot]
  --pg-publication <NAME>                                        [env: NOSTOS_PG_PUBLICATION, default: cairn_pub]
  --sync-auth <none|supabase-jwt>                                [env: NOSTOS_SYNC_AUTH, default: none]
  --log <FILTER>                                                 [env: NOSTOS_LOG, default: info,nostos=debug]
  --session-buffer <N>                                           [env: NOSTOS_SESSION_BUFFER, default: 1024]
  --slot-max-lag <BYTES>                                         [env: NOSTOS_SLOT_MAX_LAG, default: 0]
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

## 7. References

- Setup / install: [QUICKSTART.md](QUICKSTART.md).
- Architecture / dependency rule: [ARCHITECTURE.md](ARCHITECTURE.md).
- Security model: [SECURITY-MODEL.md](SECURITY-MODEL.md), [SECURITY.md](SECURITY.md).
- Throughput claims / how to verify them: [BENCHMARK-METHODOLOGY.md](BENCHMARK-METHODOLOGY.md).
- ADRs cited above: 0006 (license trust boundary), 0009 (ack-driven LSN
  resume), 0010 (sync auth), 0011 (server-enforced tenant predicates), 0013
  (write-back allowlist), 0016 (client WAL-bloat protection),
  0025 (persisted oplog backfill). See [adr/](adr/).
- Source code cited above: `crates/nostos-server/src/main.rs`,
  `crates/nostos-infra/src/transport.rs`,
  `crates/nostos-infra/src/replicator/pg.rs`,
  `crates/nostos-cli/src/{main.rs,commands/{init,doctor,dev}.rs}`.
