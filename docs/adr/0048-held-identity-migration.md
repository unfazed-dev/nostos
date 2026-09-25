---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "High. After a relink the live Supabase project's schema, RPCs, triggers and pg_cron jobs are named `nostos`, and shipped apps call `nostos_pull` and store into `nostos_*` tables. Going back is a second migration with the same fan-out, against apps that no longer speak the old names."
  surprising_without_context: true
  surprise_reason: "Some names migrate in place (Postgres objects, device SQLite tables and files) and some cut over hard with no fallback (ALPN, headers, push payload keys, Tauri plugin id). ADR-0046 kept a fallback for every env/config name, so a reader would expect the same here and find that an old client cannot talk to a new server at all."
  result_of_real_tradeoff: true
  rejected_alternatives: "Keep holding the names (decision 2b) forever: the product ships under two names, and every doc has to explain the split. Dual wire identity (accept `cairn/sync/1` and `X-Cairn-Source` alongside the new ones): code kept alive for peers that don't exist before 1.0. Resync-from-zero on devices instead of migrating the tables: loses unsent outbox writes. A standalone SQL migration file for direct mode: every linked project would have to find and run it, whereas the next `nostos link --mode direct` already runs on every one of them."
  all_three_true: true
status: accepted
---

# ADR-0048: The held identity migrates — state in place, wire by hard cut

- **Status:** Accepted (2026-09-25).
- **Date:** 2026-09-25
- **Supersedes:** decision 2b in `docs/ci/decisions.md` (the hold list).
- **References:** ADR-0046 (env/config/binary fallbacks until 1.0), ADR-0025
  (the epoch gate), ADR-0047 (push payload keys), `docs/plans/nostos-name-map-2026-09-24.md`.

## Context

The cairn → nostos rename held back every persisted or on-the-wire name
(decision 2b), because renaming them is a data migration, not a text edit.
The result was a product called nostos that ships a `cairn` schema, a
`cairn-push` Edge Function and an Android channel called "cairn". Those names
are held for two reasons, and each needs a different answer:

- **State at rest:** Postgres objects, device SQLite files and tables. These hold
  rows, checkpoints and unsent writes. Renaming them on paper loses the data
  on the device or server that still has the old names.
- **Wire identity:** ALPN, headers, the BroadcastChannel, push payload keys,
  the Tauri plugin id, the cloud cookie and metric names. These hold no data,
  so they only have to match between peers. Before 1.0 the client and server
  ship together, and nobody else runs a deployment.

## Decision

1. **State at rest migrates in place, on first touch, idempotently.**
   - **Direct mode (Postgres):** the SQL from `nostos link --mode direct` now opens with
     a `do $rename$` block. It runs only when schema `cairn` exists and
     `nostos` doesn't. It renames:
     - the schema;
     - the `public.cairn_*` RPCs;
     - the `cairn_*` triggers and policies;
     - the pg_cron job commands.

     It also re-creates every function in the renamed set from
     `pg_get_functiondef` with the old name replaced, because renaming a
     schema doesn't rewrite function bodies. The rest of the file then
     create-or-replaces as always. The log keeps its rows and every client
     cursor stays valid. Pinned by `e2e_pg_direct_sql::a_pre_rename_project_is_renamed_in_place`.
   - **Native SQLite (`nostos-client`):**
     - When the asked-for `nostos*` file is missing and its `cairn*` twin exists,
       the twin is renamed over, WAL first, before open.
     - `cairn_data/meta/outbox` are renamed before the schema runs.
     - Pinned by `pre_rename_store_is_adopted_with_its_outbox`.
   - **Web (OPFS):**
     - An origin whose pool holds only `/cairn.sqlite` keeps using that file.
     - Its tables are renamed the same way.
     - The localStorage checkpoint key is not carried over, so the first load
       after the update resyncs from 0. Rows are re-applied, not lost, and the
       outbox is in the SQLite file.
   - **Atlet:** migration `0010_nostos_engine_ids.sql` moves the recorded
     engine ids and the check constraint.
2. **Wire identity cuts over hard, with no fallback.**
   - The new names are `nostos/sync/1`, `X-Nostos-Source`, `nostos:multitab`,
     `nostos_route` / `{nostos:"ring"}`, the `nostos` Tauri plugin and
     `plugins.nostos`, the cloud cookie and the `nostos_*` metrics.
   - An old client can't talk to a new server. Release them together.
   - After 1.0, this kind of change needs a protocol version bump, not a rename.
3. **Server mode gets a runbook, not code** (below). It has one known deployment:
   ours. The dev database is renamed in place too (runbook below), not
   recreated: `down -v` wipes the `docker_pgdata` volume, which other local
   projects' slots live in.
4. **What stays `cairn`:**
   - history: applied migrations, the rename docs and tool, git pins into the archive;
   - the brand metaphor;
   - the legal entity;
   - the symbols in the committed `.wasm`, which change on its next rebuild;
   - the ADR-0046 fallbacks, until 1.0.

   Each line that still says `cairn` carries a `rename:hold` comment.

## Runbooks

**Direct mode (Supabase), in this order:**

1. Run `nostos link --mode direct --push <project>/functions/v1/nostos-push
   --deploy --fcm-service-account …`, with the same `--visible` specs as
   before. It renames in place, points `nostos.push_config` at the new URL,
   mints a new secret on both sides and deploys `nostos-push`.
2. Delete the old function: `supabase functions delete cairn-push`.
3. Ship the app build that uses the new names.

Until step 3, installed builds call `cairn_pull`, which no longer exists, and
fail to sync. They keep their outbox and catch up once updated.

**Server mode:**

```sql
alter table cairn_oplog rename to nostos_oplog;             -- rename:hold
alter table cairn_push_tokens rename to nostos_push_tokens; -- rename:hold
```

Then pick one of these:

- **Keep the old slot and publication:** set `NOSTOS_PG_SLOT=cairn_slot` and
  `NOSTOS_PG_PUBLICATION=cairn_pub`. Nothing resnapshots.
- **Move to the new defaults:** start once so `nostos_slot` is created, then
  run `select pg_drop_replication_slot('cairn_slot')` and
  `drop publication cairn_pub`. The old slot holds WAL until it's dropped. The
  new slot's epoch forces every client to resnapshot (ADR-0025).

**Dev database (docker, :5433):** the init scripts only run on an empty data
directory, so the new compose credentials don't apply to an existing
volume. Rename the old names to the new ones while the old container is still
up. The session user can't rename itself, so a throwaway superuser does it.
SCRAM passwords survive a rename; they are reset because the password is part
of the new URL.

```sh
c=cairn-postgres                                                   # rename:hold
docker exec $c psql -U cairn -d postgres -c 'create role rename_admin superuser login'  # rename:hold
docker exec -i $c psql -v ON_ERROR_STOP=1 -U rename_admin -d postgres <<'SQL'
alter database cairn rename to nostos;                             -- rename:hold
alter role cairn rename to nostos;                                 -- rename:hold
alter role cairn_writer rename to nostos_writer;                   -- rename:hold
alter role nostos password 'nostos';
alter role nostos_writer password 'nostos_writer_dev_pw';
SQL
docker exec -i $c psql -v ON_ERROR_STOP=1 -U nostos -d nostos <<'SQL'
drop role rename_admin;
alter table cairn_oplog rename to nostos_oplog;                    -- rename:hold
alter table cairn_push_tokens rename to nostos_push_tokens;        -- rename:hold
alter publication cairn_pub rename to nostos_pub;                  -- rename:hold
select pg_drop_replication_slot('cairn_slot');                     -- rename:hold
SQL
docker compose -f docker/docker-compose.yml up -d  # same project + volume: recreates as nostos-postgres
```

Other slots and publications in the cluster keep their names. Their owners
name them in their own config.

## Consequences

- Nothing ships under two names except history and the ADR-0046 fallbacks.
- Mixed fleets don't work. A tab or phone on an old build stops syncing until
  it updates. That's acceptable before 1.0 and won't be after it.
- Not handled:
  - a project that already has a `public.nostos_*` function when its first
    migration runs (the rename fails loudly and the transaction rolls back);
  - an old `cairn-push` function that nobody deletes (it keeps running
    unused).
- Addendum 2026-09-25 (measured on the atlet project): `realtime.messages` is
  owned by `supabase_realtime_admin` and `postgres` is not a member, so
  neither the CLI login role, the SQL editor nor the MCP can rename
  `cairn_ring_read` (42501). The rename loop and the
  ring-policy block now soft-fail with a warning; the old policy stays behind
  (harmless, it matches `cairn:%` topics nobody sends; drop it as the owner
  when you can), and `nostos doctor --mode direct` reports whether
  `nostos_ring_read` exists. On the atlet project the guarded `create policy`
  did go through as `postgres` while the rename did not, so the doorbell
  survived the cut-over; if it does not, pull, snapshot, push and the
  foreground refresh still work without it.
