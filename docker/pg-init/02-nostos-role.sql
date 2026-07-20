-- Least-privilege role for nostos-server (ADR-0013/0018 security model).
--
-- nostos-server connects as THIS role, NOT the `cairn`/`postgres` superuser, so
-- a compromised nostos-server can only touch the synced table(s) — it can't
-- `DROP TABLE`, read `auth.tokens`, or touch anything outside its GRANT. The
-- role carries exactly what nostos needs and nothing more:
--
--   REPLICATION — consume the logical-replication slot + the initial snapshot.
--   BYPASSRLS   — nostos applies its OWN authorization (JWT ADR-0010 + table
--                 allowlist ADR-0013 + tenant-scope ADR-0018) and writes to the
--                 synced tables itself; BYPASSRLS lets it do so even when RLS is
--                 on (the collapsed-write model). Harmless here — the demo
--                 `tasks` table has no RLS — but correct for the Supabase case
--                 where the source table may carry RLS policies.
--   GRANT       — SELECT (replication reads) + INSERT/UPDATE/DELETE (writes) on
--                 the synced table(s) ONLY. Bounded by GRANT, never a superuser.
--
-- The table allowlist (`NOSTOS_WRITE_TABLES`) is the RUNTIME gate; this role's
-- GRANTs are the DATABASE-level gate. Defense-in-depth: a write must clear both.
--
-- ponytail: the password is a throwaway local-Docker dev secret (mirrors the
-- `cairn:cairn` dev creds in docker-compose.yml). Production / Supabase uses a
-- real generated secret — see the Security Model docs + the Supabase migration
-- script (never commit a production password).
CREATE ROLE cairn_writer WITH LOGIN REPLICATION BYPASSRLS PASSWORD 'cairn_writer_dev_pw';

GRANT USAGE ON SCHEMA public TO cairn_writer;
GRANT SELECT, INSERT, UPDATE, DELETE ON tasks TO cairn_writer;
-- Provider-dashboard tables (D4) — same least-privilege grant as `tasks`.
GRANT SELECT, INSERT, UPDATE, DELETE
    ON providers, clients, availabilities, appointments, invoices
    TO cairn_writer;
-- cairn_oplog (ADR-0025 slice 2 + slice 5) — nostos-server writes the op-log
-- at the fan-out chokepoint (INSERT), reads it back on reconnect replay
-- (SELECT), and compacts it to bound growth (DELETE — slice 5). Never UPDATE
-- (compaction is collapse-via-delete, not in-place rewrite). Not part of the
-- synced-table allowlist — this is nostos's internal resume table, not a
-- client-writable table.
GRANT SELECT, INSERT, DELETE ON cairn_oplog TO cairn_writer;
