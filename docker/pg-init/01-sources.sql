-- Example schema + publication for local-first demo.
-- Mirrors the canonical "tasks" workload used in the benchmark.
--
-- TYPE CHOICE NOTE (write-back safety): all money/rate columns are BIGINT
-- (cents) and duration columns are integer minutes. The Rust write-back
-- (crates/nostos-infra/src/write_back.rs) binds JSON values by shape-inference:
-- i64→INT8, f64→FLOAT8. No NUMERIC/Decimal variant exists, and INT4 columns
-- reject i64 ("error serializing parameter N"). BIGINT matches i64 natively,
-- so it's the safe type for any client-written integer.
CREATE TABLE IF NOT EXISTS tasks (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL,
    assignee_id UUID,
    title       TEXT NOT NULL,
    completed   BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_tasks_org_assignee
    ON tasks (org_id, assignee_id);

-- ADR-0025 delete-tenant follow-up: tenant-scoped tables MUST use REPLICA
-- IDENTITY FULL so a DELETE carries the full old row image (incl. org_id);
-- the op-log writer lifts the tenant from it so tenant-filtered replay still
-- matches deletes (else NULL tenant → dropped → ghost rows on reconnect).
ALTER TABLE tasks REPLICA IDENTITY FULL;

-- ── Provider-dashboard schema (multi-table demo, D4) ───────────────────────
-- Single-tenant v1: all rows sync (no where_sql partitioning). ponytail:
-- per-provider where_sql partitioning is the multi-tenant upgrade path.
--
-- rate_type governs how invoices are auto-calculated (BillingService):
--   'hourly'      → amount = duration_min × hourly_rate_cents / 60
--   'flat'        → amount = flat_rate_cents (duration-independent one-off)
--   'subscription'→ amount = subscription_rate_cents (recurring monthly)
CREATE TABLE IF NOT EXISTS providers (
    id                        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                      TEXT NOT NULL,
    specialty                 TEXT,
    email                     TEXT,
    phone                     TEXT,
    rate_type                 TEXT NOT NULL DEFAULT 'hourly',
    hourly_rate_cents         BIGINT NOT NULL DEFAULT 0,
    flat_rate_cents           BIGINT NOT NULL DEFAULT 0,
    subscription_rate_cents   BIGINT NOT NULL DEFAULT 0,
    bio                       TEXT,
    avatar_color              TEXT DEFAULT '#2E6FDB',
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (rate_type IN ('hourly','flat','subscription'))
);

CREATE TABLE IF NOT EXISTS clients (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL,
    email      TEXT,
    phone      TEXT,
    notes      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Recurring weekly availability windows: weekday (0=Sun..6=Sat) + minute
-- offsets within the day (0..1440). ponytail: dated exception/override slots.
CREATE TABLE IF NOT EXISTS availabilities (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    weekday     INT  NOT NULL CHECK (weekday BETWEEN 0 AND 6),
    start_min   INT  NOT NULL CHECK (start_min BETWEEN 0 AND 1439),
    end_min     INT  NOT NULL CHECK (end_min BETWEEN 1 AND 1440),
    CHECK (end_min > start_min)
);

CREATE TABLE IF NOT EXISTS appointments (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id  UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    client_id    UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    starts_at    TIMESTAMPTZ NOT NULL,
    duration_min INT NOT NULL CHECK (duration_min > 0),
    status       TEXT NOT NULL DEFAULT 'confirmed',
    notes        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('confirmed','completed','cancelled','no_show'))
);

-- Invoices carry a rate SNAPSHOT (rate_cents + line_type + hours_min) captured
-- at issue time, so a provider changing their rate later never re-prices a
-- historical invoice (canonical billing pattern — rate snapshot, not live join).
-- amount_cents is BIGINT (i64→INT8 bind match, no NUMERIC needed).
CREATE TABLE IF NOT EXISTS invoices (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    appointment_id UUID NOT NULL REFERENCES appointments(id) ON DELETE CASCADE,
    client_id      UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    provider_id    UUID REFERENCES providers(id) ON DELETE SET NULL,
    amount_cents   BIGINT NOT NULL CHECK (amount_cents >= 0),
    line_type      TEXT NOT NULL DEFAULT 'hourly',
    rate_cents     BIGINT NOT NULL DEFAULT 0,
    hours_min      BIGINT NOT NULL DEFAULT 0,
    description    TEXT,
    status         TEXT NOT NULL DEFAULT 'issued',
    issued_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    due_at         TIMESTAMPTZ,
    paid_at        TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('issued','paid','void','refunded')),
    CHECK (line_type IN ('hourly','flat','subscription'))
);

-- Realtime chat: synced table = realtime stream (local-first 2026 best
-- practice). Messages flow through nostos replication like
-- any other row; the chat view watches this table reactively via watchMapped.
-- No separate WebSocket needed.
CREATE TABLE IF NOT EXISTS messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    client_id   UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    sender_type TEXT NOT NULL,
    sender_id   UUID NOT NULL,
    body        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at     TIMESTAMPTZ,
    CHECK (sender_type IN ('provider','client'))
);

-- ── Migrations for pre-existing DBs ─────────────────────────────────────────
-- CREATE TABLE IF NOT EXISTS skips a table that already exists, so columns
-- added after initial bootstrap must be ALTER-added here to migrate an older
-- DB forward. ADD COLUMN IF NOT EXISTS is a no-op on fresh DBs (the CREATE
-- TABLE already has them) and adds them on legacy DBs. Safe to re-run.
ALTER TABLE providers ADD COLUMN IF NOT EXISTS rate_type               TEXT    NOT NULL DEFAULT 'hourly';
ALTER TABLE providers ADD COLUMN IF NOT EXISTS hourly_rate_cents       BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE providers ADD COLUMN IF NOT EXISTS flat_rate_cents         BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE providers ADD COLUMN IF NOT EXISTS subscription_rate_cents BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE providers ADD COLUMN IF NOT EXISTS bio                     TEXT;
ALTER TABLE providers ADD COLUMN IF NOT EXISTS avatar_color            TEXT    DEFAULT '#2E6FDB';

ALTER TABLE appointments ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'confirmed';

ALTER TABLE invoices ADD COLUMN IF NOT EXISTS provider_id    UUID    REFERENCES providers(id) ON DELETE SET NULL;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS line_type     TEXT    NOT NULL DEFAULT 'hourly';
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS rate_cents    BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS hours_min     BIGINT  NOT NULL DEFAULT 0;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS description   TEXT;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS due_at        TIMESTAMPTZ;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS paid_at       TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_availabilities_provider ON availabilities (provider_id, weekday);
CREATE INDEX IF NOT EXISTS idx_appointments_provider  ON appointments (provider_id, starts_at);
CREATE INDEX IF NOT EXISTS idx_appointments_client    ON appointments (client_id, starts_at);
CREATE INDEX IF NOT EXISTS idx_invoices_appointment   ON invoices (appointment_id);
CREATE INDEX IF NOT EXISTS idx_invoices_client        ON invoices (client_id);
CREATE INDEX IF NOT EXISTS idx_invoices_provider      ON invoices (provider_id);
CREATE INDEX IF NOT EXISTS idx_messages_thread        ON messages (provider_id, client_id, created_at);

-- The publication is what the replicator subscribes to via logical replication.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'nostos_pub') THEN
        CREATE PUBLICATION nostos_pub FOR TABLE tasks;
    END IF;
END $$;
-- Add the booking tables to the publication (idempotent across re-init/re-runs).
DO $$
DECLARE t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY['providers','clients','availabilities','appointments','invoices','messages'] LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_publication_tables
            WHERE pubname = 'nostos_pub' AND schemaname = 'public' AND tablename = t
        ) THEN
            EXECUTE format('ALTER PUBLICATION nostos_pub ADD TABLE public.%I', t);
        END IF;
    END LOOP;
END $$;

-- ===========================================================================
-- nostos_oplog — persisted operation log for reconnect resume (ADR-0025 slice 2).
-- Written at the fan-out chokepoint (batched, off the fan-out loop by a
-- background flush task) and replayed on reconnect from a client's checkpoint
-- so a resuming client receives missed INSERT/UPDATE/DELETE ops in-window
-- instead of re-snapshotting. Snapshot-reconcile (slice 1) remains the
-- fallback for long gaps / first-connect / epoch mismatch.
--
-- tenant_id is lifted from each row's tenant column at write time (NULL for
-- non-tenant-scoped tables); the (tenant_id, lsn) index is the replay path.
-- (table_name, pk) supports future compaction (slice 5).
--
-- NOT IN nostos_pub: this is nostos's internal resume table, not a synced app
-- table. Publishing it would feed its own writes back through logical
-- replication as spurious client events (a feedback loop). Keep it out of the
-- publication.
-- ===========================================================================
CREATE TABLE IF NOT EXISTS nostos_oplog (
    op_id      BIGSERIAL   PRIMARY KEY,
    lsn        BIGINT      NOT NULL,
    table_name TEXT        NOT NULL,
    pk         TEXT        NOT NULL,
    op         TEXT        NOT NULL,                 -- 'upsert' | 'delete'
    payload    JSONB,                                -- NULL for deletes
    tenant_id  TEXT,                                  -- NULL when the row's table has no tenant column
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_nostos_oplog_tenant_lsn ON nostos_oplog (tenant_id, lsn);
CREATE INDEX IF NOT EXISTS idx_nostos_oplog_table_pk   ON nostos_oplog (table_name, pk);

-- ===========================================================================
-- nostos_push_tokens — push-notification transport-token registry (ADR-0037 §3).
-- Registered via REST (POST /push-tokens) authenticated like /sync; tenant_id
-- and account_id are stamped SERVER-SIDE from the principal (ADR-0018
-- discipline — a client-attested tenant on a token row is an
-- exfiltration-adjacent bug). The token is the PK: one token = one device =
-- one current account, so re-registering under a different account MIGRATES
-- the row (a leaked registration must never push the previous principal's
-- data to the next user). Pruned on APNs 410 / FCM UNREGISTERED. The
-- (account_id, tenant_id) index is the offline-account device lookup.
--
-- NOT in nostos_pub, same as nostos_oplog: nostos-internal table, and publishing
-- it would replicate device push tokens through the sync stream.
-- ===========================================================================
CREATE TABLE IF NOT EXISTS nostos_push_tokens (
    token      TEXT        PRIMARY KEY,
    platform   TEXT        NOT NULL,                -- 'apns' | 'fcm' | 'webpush'
    account_id TEXT        NOT NULL,
    tenant_id  TEXT        NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_nostos_push_tokens_account
    ON nostos_push_tokens (account_id, tenant_id);
