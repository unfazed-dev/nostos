# Nostos Security Model

How nostos authorizes reads + writes against the source Postgres, and the
trade-off vs Supabase RLS / PowerSync's split-write model. **Read this before
adopting nostos on a Supabase project whose security model is RLS.**

## The collapsed-write model

nostos's headline DX edge is **zero-backend-write**: the server's `PgWriteBack`
applies client writes *directly* to the source Postgres — no `uploadData`, no
app-side write code. The app talks only to `nostos-server` over `/sync`; it never
touches Postgres (`docs/plans/nostos-reference-demo-app.md`: *"The app never talks
to Postgres directly"*).

Because nostos itself runs the write SQL, it owns the **trust boundary** — the one
place a client-controlled string becomes part of a SQL statement. Three defenses
apply in order (`crates/nostos-infra/src/write_back.rs`):

1. **Table allowlist** (`NOSTOS_WRITE_TABLES`, ADR-0013) — a table not explicitly
   listed can never reach the SQL builder. Empty by default = no tables writable.
2. **Column validation** — payload keys must match `^[a-z_][a-z0-9_]*$`.
3. **Bind parameters** — values are bound, never interpolated.

On top of the SQL boundary, nostos enforces **identity + tenancy**:

- **JWT auth** (ADR-0010): `/sync` verifies the Supabase session JWT; the
  principal is threaded through every read + write.
- **Tenant scoping** (ADR-0018): on writes, the tenant column is force-stamped
  to the principal's tenant (INSERT) and writes are constrained to own-tenant
  rows (UPDATE/DELETE); cross-tenant writes are rejected outright, never silently
  applied.

## The least-privilege connection role (NOT superuser)

nostos-server connects to Postgres as a dedicated least-privilege role — **never
the `postgres` superuser**. The demo role (`docker/pg-init/02-nostos-role.sql`):

```sql
CREATE ROLE cairn_writer WITH LOGIN REPLICATION BYPASSRLS PASSWORD '<secret>';
GRANT USAGE ON SCHEMA public TO cairn_writer;
GRANT SELECT, INSERT, UPDATE, DELETE ON tasks TO cairn_writer;
```

- `REPLICATION` — consume the logical-replication slot + the initial snapshot.
- `BYPASSRLS` — nostos applies its own authz (above) and writes to synced tables;
  this lets it do so even when RLS is on.
- `GRANT` on **only** the synced tables — the database-level gate. Combined with
  the runtime `NOSTOS_WRITE_TABLES` allowlist, this is defense-in-depth.

**Blast radius (verified):** a nostos-server connected as `cairn_writer` can
INSERT/UPDATE/DELETE on granted tables but **cannot** `DROP TABLE`, read
`auth.tokens`, or touch anything outside its GRANT (`DROP TABLE tasks` →
`ERROR: must be owner of table tasks`). This is why nostos-server must **never**
connect as the `postgres` superuser in any deploy.

### Supabase setup

Run once in the Supabase SQL editor as `postgres`, then point nostos-server at the
role (direct connection — the pooler can't carry logical replication):

```sql
CREATE PUBLICATION cairn_pub FOR TABLE tasks;                       -- replication source
CREATE ROLE cairn_writer WITH LOGIN REPLICATION BYPASSRLS PASSWORD '<strong-secret>';
GRANT USAGE ON SCHEMA public TO cairn_writer;
GRANT SELECT, INSERT, UPDATE, DELETE ON tasks TO cairn_writer;      -- repeat per NOSTOS_WRITE_TABLES entry
```

```sh
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL='postgresql://cairn_writer:<strong-secret>@db.<ref>.supabase.co:5432/postgres' \
NOSTOS_PG_SLOT=cairn_slot NOSTOS_PG_PUBLICATION=cairn_pub \
NOSTOS_WRITE_TABLES=tasks NOSTOS_SYNC_AUTH=supabase-jwt \
./target/debug/nostos-server
```

Use a generated secret; never commit it (the demo's `cairn_writer_dev_pw` is a
throwaway local-Docker credential, not a real secret).

## The RLS trade-off — read before adopting on Supabase

Because nostos connects as a `BYPASSRLS` role, **its writes bypass Supabase RLS**.
nostos substitutes its own authorization (JWT + allowlist + tenant-scoping), which
is **strictly coarser** than arbitrary RLS policies:

- nostos tenant-scoping = **one tenant column** (force-stamp + cross-tenant reject).
- Supabase RLS = **any per-row policy** — e.g.
  `team_id IN (SELECT team_id FROM team_members WHERE user_id = auth.uid()) AND status = 'active'`.

**nostos fits** single-tenant apps, simple tenant-scoped multi-tenant apps, or any
app where a trusted server is the writer and complex per-user row policies aren't
the security model.

**nostos does NOT fit** apps whose security model *is* complex per-user RLS —
there, nostos's single-column tenant model is a step down. Use PowerSync (writes
go through Supabase's Data API, where your RLS applies) or PostgREST directly.
This is a deliberate architectural consequence of zero-backend-write, not a bug.

## Comparison

| | nostos (collapsed) | PowerSync (split) |
|---|---|---|
| Who applies the write | nostos-server `PgWriteBack` → direct pg | the app's `uploadData` → Supabase Data API |
| Write authorization | nostos: JWT + `NOSTOS_WRITE_TABLES` + tenant-scope | Supabase RLS (per-user JWT) |
| App write code | **none** (zero-backend-write) | developer writes `uploadData` |
| Connects to pg as | least-privilege `BYPASSRLS` role | n/a — the app uses the Data API; PowerSync only reads the WAL |

## Related

- ADR-0010 (`/sync` authentication + `Principal`), ADR-0013 (write-back allowlist),
  ADR-0018 (write-path tenant enforcement).
- `docker/pg-init/02-nostos-role.sql` — the demo least-privilege role.
