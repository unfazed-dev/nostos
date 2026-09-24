# Security

Two parts: how to report a vulnerability, and the security model — how Nostos
authorizes reads and writes against the source Postgres, and why Postgres Row
Level Security (RLS) does **not** protect sync traffic. **Read the model before
adopting Nostos on a Supabase project whose security model is RLS.**

## Reporting a vulnerability

Report security vulnerabilities privately through **[GitHub private vulnerability
reporting](https://github.com/unfazed-dev/nostos/security/advisories/new)**
(repo → Security → Report a vulnerability). Do not open a public GitHub issue for
a suspected vulnerability — public issues are fine for everything else, but a
vulnerability report should stay private until a fix ships.

<!-- NOSTOS-IDENTITY-PENDING: no contact mailbox exists yet — docs/IDENTITY.md.
     This previously read "report privately to founders@nostos.run", a mailbox on
     an UNREGISTERED domain: such a report reaches nobody, and the reporter has
     no way to know it vanished. Add a real address here when one exists; until
     then GitHub's private reporting is the channel that actually works. -->

Include what you can: affected version/commit, reproduction steps, and
impact. We'll acknowledge your report and follow up as we investigate; credit
is given in the fix's release notes unless you ask us not to.

## Supported versions

| Version | Supported |
|---|---|
| `v0.2.x` | :white_check_mark: |
| `v0.1.x` | :x: — upgrade to `v0.2.x` |
| pre-release / `main` | best-effort, alpha software |

Nostos is pre-1.0; there is no long-term-support branch yet. Security fixes
land on `main` and are backported to the current `v0.2.x` line.

---

## Security model

Nostos sits between Postgres (or Supabase) and every device, with a privileged
connection. That position means Nostos has to be its own authorization layer
for sync traffic.

### Why RLS doesn't reach sync traffic

1. **Logical replication streams unfiltered rows.** `nostos-server`'s
   `PgReplicator` reads the WAL through a replication slot as a privileged
   Postgres role. RLS policies are evaluated per-session for normal SQL
   connections; the replication protocol has no session and no policy
   evaluation. Every row change on a published table reaches the server,
   regardless of which tenant it belongs to.
2. **Write-back is a privileged connection.** Direct write-back (ADR-0013)
   applies client-queued mutations over a single `PgWriteBack` connection —
   not as the end user, and not subject to that user's RLS policies.

So the two places RLS would normally do the work — read filtering and write
scoping — are both bypassed by construction.

### Reads: server-enforced predicates

The server never trusts a client-supplied tenant filter (ADR-0011). When
`NOSTOS_SYNC_AUTH=supabase-jwt` and `NOSTOS_TENANT_COLUMN` is set,
`build_predicate` drops any client-attested filter on that column and ANDs in
`<tenant_column> = <principal.tenant_id>` on every subscription, including
subscriptions that carry a `where_sql` clause (ADR-0012) — a client expression
can never widen scope past its own tenant.

### Writes: the collapsed-write model

Nostos's headline DX edge is **zero-backend-write**: `PgWriteBack` applies
client writes *directly* to the source Postgres — no `uploadData`, no app-side
write code. The app talks only to `nostos-server` over `/sync`; it never
touches Postgres.

Because Nostos runs the write SQL, it owns the **trust boundary** — the one
place a client-controlled string becomes part of a SQL statement. Three
defenses apply in order (`crates/nostos-infra/src/write_back.rs`):

1. **Table allowlist** (`NOSTOS_WRITE_TABLES`, ADR-0013) — a table not explicitly
   listed can never reach the SQL builder. Empty by default = no tables writable.
2. **Identifier validation** — payload keys must match `^[a-z_][a-z0-9_]*$`
   (`crates/nostos-infra/src/ident.rs`).
3. **Bind parameters** — values are bound, never interpolated.

On top of the SQL boundary, Nostos enforces **identity + tenancy**:

- **JWT auth** (ADR-0010): `/sync` verifies the Supabase session JWT; the
  principal is threaded through every read + write.
- **Tenant scoping** (ADR-0018, shipped): the tenant column is force-stamped
  to the principal's tenant on INSERT, and UPDATE/DELETE are constrained to
  own-tenant rows; cross-tenant writes are rejected outright, never silently
  applied.

### Anonymous mode is dev-only

`NOSTOS_SYNC_AUTH=none` (the OSS default) injects no tenant filter — there is
no principal to scope to — so it is single-tenant only. The server **refuses
to start** in this mode when `NOSTOS_BIND` is reachable off-host, unless
`NOSTOS_INSECURE_ANONYMOUS=1` says something in front of it authenticates. A
multi-tenant deploy (including any Supabase project with more than one
tenant's data in a synced table) **must** set `NOSTOS_SYNC_AUTH=supabase-jwt`
and `NOSTOS_TENANT_COLUMN`.

### The least-privilege connection role (NOT superuser)

`nostos-server` connects to Postgres as a dedicated least-privilege role —
**never the `postgres` superuser**. The role, publication and slot keep their
pre-rename `nostos_*` names (Nostos was formerly Nostos; these are held
identifiers). The demo role (`docker/pg-init/02-nostos-role.sql`):

```sql
CREATE ROLE nostos_writer WITH LOGIN REPLICATION BYPASSRLS PASSWORD '<secret>';
GRANT USAGE ON SCHEMA public TO nostos_writer;
GRANT SELECT, INSERT, UPDATE, DELETE ON tasks TO nostos_writer;
```

- `REPLICATION` — consume the logical-replication slot + the initial snapshot.
- `BYPASSRLS` — Nostos applies its own authz (above) and writes to synced
  tables; this lets it do so even when RLS is on.
- `GRANT` on **only** the synced tables — the database-level gate. Combined with
  the runtime `NOSTOS_WRITE_TABLES` allowlist, this is defense-in-depth.

**Blast radius (verified):** a server connected as `nostos_writer` can
INSERT/UPDATE/DELETE on granted tables but **cannot** `DROP TABLE`, read
`auth.tokens`, or touch anything outside its GRANT (`DROP TABLE tasks` →
`ERROR: must be owner of table tasks`).

#### Supabase setup

Run once in the Supabase SQL editor as `postgres`, then point `nostos-server`
at the role (direct connection — the pooler can't carry logical replication):

```sql
CREATE PUBLICATION nostos_pub FOR TABLE tasks;                       -- replication source
CREATE ROLE nostos_writer WITH LOGIN REPLICATION BYPASSRLS PASSWORD '<strong-secret>';
GRANT USAGE ON SCHEMA public TO nostos_writer;
GRANT SELECT, INSERT, UPDATE, DELETE ON tasks TO nostos_writer;      -- repeat per NOSTOS_WRITE_TABLES entry
```

```sh
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL='postgresql://nostos_writer:<strong-secret>@db.<ref>.supabase.co:5432/postgres' \
NOSTOS_PG_SLOT=nostos_slot NOSTOS_PG_PUBLICATION=nostos_pub \
NOSTOS_WRITE_TABLES=tasks NOSTOS_SYNC_AUTH=supabase-jwt \
./target/debug/nostos-server
```

Use a generated secret; never commit it (the demo's `nostos_writer_dev_pw` is a
throwaway local-Docker credential, not a real secret).

### The RLS trade-off — read before adopting on Supabase

Because Nostos connects as a `BYPASSRLS` role, **its reads and writes bypass
Supabase RLS**. Nostos substitutes its own authorization (JWT + allowlist +
tenant scoping), which is **strictly coarser** than arbitrary RLS policies:

- Nostos tenant scoping = **one tenant column** (force-stamp + cross-tenant reject).
- Supabase RLS = **any per-row policy** — e.g.
  `team_id IN (SELECT team_id FROM team_members WHERE user_id = auth.uid()) AND status = 'active'`.

**Nostos fits** single-tenant apps, simple tenant-scoped multi-tenant apps, or
any app where a trusted server is the writer and complex per-user row policies
aren't the security model.

**Nostos does NOT fit** apps whose security model *is* complex per-user RLS —
there, the single-column tenant model is a step down. Use a split-write setup
(writes go through Supabase's Data API, where your RLS applies) or PostgREST
directly. This is a deliberate consequence of zero-backend-write, not a bug.

| | Nostos (collapsed) | split write (app → Data API) |
|---|---|---|
| Who applies the write | `nostos-server` `PgWriteBack` → direct pg | the app's `uploadData` → Supabase Data API |
| Write authorization | JWT + `NOSTOS_WRITE_TABLES` + tenant scope | Supabase RLS (per-user JWT) |
| App write code | **none** (zero-backend-write) | developer writes `uploadData` |
| Connects to pg as | least-privilege `BYPASSRLS` role | n/a — the app writes via the Data API |

### Summary

| Layer | Postgres RLS | Nostos |
|---|---|---|
| Reads | Bypassed (replication has no session) | Server-injected tenant predicate (ADR-0011) |
| Writes | Bypassed (privileged write-back connection) | Allowlist + identifier check + bind params + tenant force-stamp (ADR-0013, ADR-0018) |
| Anonymous/dev mode | N/A | Single-tenant only; refuses an off-host bind |

### Related

- [ADR-0010](docs/adr/0010-sync-authentication-and-principal.md) — `/sync` authentication + `Principal`
- [ADR-0011](docs/adr/0011-server-enforced-predicates.md) — read-path enforcement
- [ADR-0013](docs/adr/0013-direct-write-back-design.md) — write-back allowlist
- [ADR-0018](docs/adr/0018-write-path-tenant-enforcement.md) — write-path tenant enforcement
- [`docker/pg-init/02-nostos-role.sql`](docker/pg-init/02-nostos-role.sql) — the demo least-privilege role
