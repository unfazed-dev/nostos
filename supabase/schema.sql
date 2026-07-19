-- supabase/schema.sql — Nostos Provider Dashboard schema for Supabase (BYO schema).
--
-- nostos READS your schema via Postgres logical replication; it does NOT create
-- the tables upstream. This file IS the "bring your own schema" step. Paste it
-- into Supabase Dashboard → SQL Editor → New query → Run. Idempotent
-- (CREATE TABLE IF NOT EXISTS / ALTER ... ADD COLUMN IF NOT EXISTS /
-- ON CONFLICT DO NOTHING) — safe to re-run.
--
-- After it runs:
--   1. 7 tables (tasks, providers, clients, availabilities, appointments,
--      invoices, messages) + the `cairn_pub` publication exist in your Supabase DB.
--   2. Point nostos-server at Supabase with the DIRECT connection (NOT the
--      pooler — logical replication needs a direct connection):
--        NOSTOS_REPLICATOR=pg \
--        NOSTOS_PG_URL='postgresql://postgres:<pw>@db.<project>.supabase.co:5432/postgres' \
--        NOSTOS_WRITE_TABLES=tasks,providers,clients,availabilities,appointments,invoices,messages \
--        cargo run -p nostos-server
--   3. In the app repo: `nostos pull && nostos gen` rebuilds .nostos/schema.json
--      + nostos.g.dart from your Supabase schema.
--
-- Mirrors docker/pg-init/01-sources.sql (schema + publication) + the demo seed
-- (03-seed-booking.sql). Supabase manages DB roles itself, so the least-priv
-- `cairn_writer` role (02-nostos-role.sql) is omitted — nostos connects as
-- `postgres` here. For production, create a dedicated REPLICATION role (ADR-0013/0018).
--
-- TYPE CHOICE NOTE (write-back safety):
-- All money/rate columns are BIGINT (cents), and all duration columns are
-- integer minutes. The Rust write-back (crates/nostos-infra/src/write_back.rs)
-- binds JSON values by shape-inference: i64→INT8, f64→FLOAT8. There is no
-- NUMERIC/Decimal bind variant, and INT4 columns reject i64 values
-- ("error serializing parameter N"). BIGINT is the safe type for any
-- integer that round-trips through a client write — it matches i64 natively.

-- Example schema + publication for local-first demo.
-- Mirrors the canonical "tasks" workload used in the benchmark.
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

-- Realtime chat: synced table = realtime stream (local-first 2026 best practice
-- — PowerSync/LiveStore pattern). Messages flow through nostos replication like
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
    IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = 'cairn_pub') THEN
        CREATE PUBLICATION cairn_pub FOR TABLE tasks;
    END IF;
END $$;
-- Add the booking tables to the publication (idempotent across re-init/re-runs).
DO $$
DECLARE t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY['providers','clients','availabilities','appointments','invoices','messages'] LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_publication_tables
            WHERE pubname = 'cairn_pub' AND schemaname = 'public' AND tablename = t
        ) THEN
            EXECUTE format('ALTER PUBLICATION cairn_pub ADD TABLE public.%I', t);
        END IF;
    END LOOP;
END $$;

-- ── demo seed ────────────────────────────────────────────────────────────────
-- Fixed UUIDs so FK refs hold. Providers use ON CONFLICT DO UPDATE so re-runs
-- force the rate columns onto pre-existing rows (ON CONFLICT DO NOTHING would
-- leave legacy rows with rate_type='hourly' default + 0 rates). Other tables
-- use DO NOTHING (their seed data is stable).

-- Providers: each demonstrates a different rate_type so the auto-calc engine
-- has realistic variety. hourly_rate_cents: Ada $250/hr = 25000. flat: Marie
-- $180/visit = 18000. subscription: Alan $800/mo = 80000. Barbara hourly $310.
INSERT INTO providers (id, name, specialty, email, phone, rate_type, hourly_rate_cents, flat_rate_cents, subscription_rate_cents, bio, avatar_color) VALUES
  ('11111111-1111-1111-1111-111111111101', 'Dr. Ada Lovelace',  'Cardiology',   'ada@nostos.run',    '555-0101', 'hourly',      25000, 0,     0,     'Pioneer of computational cardiology. Accepting new patients.',         '#2E6FDB'),
  ('11111111-1111-1111-1111-111111111102', 'Dr. Marie Curie',   'Dermatology',  'marie@nostos.run',  '555-0102', 'flat',            0, 18000, 0,     'Flat-fee dermatology consultations and skin cancer screenings.',      '#B83227'),
  ('11111111-1111-1111-1111-111111111103', 'Dr. Alan Turing',   'General',      'alan@nostos.run',   '555-0103', 'subscription',    0, 0,     80000, 'Concierge primary care — unlimited visits under monthly subscription.','#6F2DBD'),
  ('11111111-1111-1111-1111-111111111104', 'Dr. Barbara Liskov','Neurology',    'barbara@nostos.run','555-0104', 'hourly',      31000, 0,     0,     'Subtyping pioneer, now seeing neurology patients.',                   '#0E8388')
ON CONFLICT (id) DO UPDATE SET
  rate_type = EXCLUDED.rate_type,
  hourly_rate_cents = EXCLUDED.hourly_rate_cents,
  flat_rate_cents = EXCLUDED.flat_rate_cents,
  subscription_rate_cents = EXCLUDED.subscription_rate_cents,
  bio = EXCLUDED.bio,
  avatar_color = EXCLUDED.avatar_color;

INSERT INTO clients (id, name, email, phone, notes) VALUES
  ('22222222-2222-2222-2222-222222222201', 'Grace Hopper',      'grace@nostos.run', '555-0201', 'Prefers mornings'),
  ('22222222-2222-2222-2222-222222222202', 'Katherine Johnson', 'kate@nostos.run',  '555-0202', NULL),
  ('22222222-2222-2222-2222-222222222203', 'Linus Torvalds',    'linus@nostos.run', '555-0203', 'New patient'),
  ('22222222-2222-2222-2222-222222222204', 'Hedy Lamarr',       'hedy@nostos.run',  '555-0204', 'Recurring migraines')
ON CONFLICT (id) DO NOTHING;

-- Recurring weekly slots (weekday: 1=Mon..5=Fri). Minutes-from-midnight:
-- 09:00=540, 12:00=720, 13:00=780, 17:00=1020.
INSERT INTO availabilities (id, provider_id, weekday, start_min, end_min) VALUES
  ('33333333-3333-3333-3333-333333333301', '11111111-1111-1111-1111-111111111101', 1, 540,  720),
  ('33333333-3333-3333-3333-333333333302', '11111111-1111-1111-1111-111111111101', 1, 780, 1020),
  ('33333333-3333-3333-3333-333333333303', '11111111-1111-1111-1111-111111111101', 3, 540,  720),
  ('33333333-3333-3333-3333-333333333304', '11111111-1111-1111-1111-111111111102', 2, 540, 1020),
  ('33333333-3333-3333-3333-333333333305', '11111111-1111-1111-1111-111111111103', 4, 780, 1020),
  ('33333333-3333-3333-3333-333333333306', '11111111-1111-1111-1111-111111111104', 1, 540, 1020),
  ('33333333-3333-3333-3333-333333333307', '11111111-1111-1111-1111-111111111104', 3, 780, 1020)
ON CONFLICT (id) DO NOTHING;

-- Sample appointments (past + upcoming) so invoices have something to attach to.
-- Ada (hourly) — 60min completed consult with Grace. Invoice below = $250.00.
INSERT INTO appointments (id, provider_id, client_id, starts_at, duration_min, status, notes) VALUES
  ('44444444-4444-4444-4444-444444444401', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', '2026-07-10T09:00:00+00:00', 60, 'completed',  'Annual heart check-up'),
  ('44444444-4444-4444-4444-444444444402', '11111111-1111-1111-1111-111111111102', '22222222-2222-2222-2222-222222222204', '2026-07-11T10:00:00+00:00', 30, 'completed',  'Skin screening'),
  ('44444444-4444-4444-4444-444444444403', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', '2026-07-14T14:00:00+00:00', 45, 'completed',  'Initial concierge visit'),
  ('44444444-4444-4444-4444-444444444404', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', '2026-07-22T09:00:00+00:00', 30, 'confirmed',  'Follow-up ECG'),
  ('44444444-4444-4444-4444-444444444405', '11111111-1111-1111-1111-111111111104', '22222222-2222-2222-2222-222222222204', '2026-07-23T11:00:00+00:00', 90, 'confirmed',  'Migraine workup')
ON CONFLICT (id) DO NOTHING;

-- Invoices auto-calculated at issue (rate snapshot captured). Demonstrates each
-- line_type: Ada hourly (60min × $250/hr = $250), Marie flat ($180), Alan
-- subscription ($800), Barbara hourly (sample past, $0 pending app creation).
-- amounts are the exact BillingService outputs so the demo is self-consistent.
INSERT INTO invoices (id, appointment_id, client_id, provider_id, amount_cents, line_type, rate_cents, hours_min, description, status, issued_at, due_at, paid_at) VALUES
  ('55555555-5555-5555-5555-555555555501', '44444444-4444-4444-4444-444444444401', '22222222-2222-2222-2222-222222222201', '11111111-1111-1111-1111-111111111101', 25000, 'hourly',      25000, 60, 'Cardiology consult — 1.0 hr @ $250/hr',                 'paid', '2026-07-10T10:05:00+00:00', '2026-07-24T10:05:00+00:00', '2026-07-12T08:00:00+00:00'),
  ('55555555-5555-5555-5555-555555555502', '44444444-4444-4444-4444-444444444402', '22222222-2222-2222-2222-222222222204', '11111111-1111-1111-1111-111111111102', 18000, 'flat',        18000,  0, 'Dermatology skin screening — flat fee',                  'paid', '2026-07-11T11:00:00+00:00', '2026-07-25T11:00:00+00:00', '2026-07-13T09:30:00+00:00'),
  ('55555555-5555-5555-5555-555555555503', '44444444-4444-4444-4444-444444444403', '22222222-2222-2222-2222-222222222203', '11111111-1111-1111-1111-111111111103', 80000, 'subscription',80000,  0, 'Concierge primary care — July subscription',             'issued','2026-07-14T15:00:00+00:00', '2026-07-28T15:00:00+00:00', NULL)
ON CONFLICT (id) DO NOTHING;

-- Sample chat thread between Ada (provider) and Grace (client), plus a second
-- thread (Alan + Linus). Demonstrates the realtime-chat-as-synced-table pattern.
INSERT INTO messages (id, provider_id, client_id, sender_type, sender_id, body, created_at) VALUES
  ('66666666-6666-6666-6666-666666666601', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'client',  '22222222-2222-2222-2222-222222222201', 'Hi Dr. Lovelace, is my Tuesday appointment still on?', '2026-07-20T15:00:00+00:00'),
  ('66666666-6666-6666-6666-666666666602', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'provider','11111111-1111-1111-1111-111111111101', 'Yes — 9:00am for the follow-up ECG. See you then.',    '2026-07-20T15:02:00+00:00'),
  ('66666666-6666-6666-6666-666666666603', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'client',  '22222222-2222-2222-2222-222222222201', 'Great, do I need to fast beforehand?',                 '2026-07-20T15:03:00+00:00'),
  ('66666666-6666-6666-6666-666666666604', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'provider','11111111-1111-1111-1111-111111111101', 'No fasting needed for an ECG. Just come as you are.',  '2026-07-20T15:05:00+00:00'),
  ('66666666-6666-6666-6666-666666666605', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', 'client',  '22222222-2222-2222-2222-222222222203', 'Dr. Turing, how do I pay the subscription invoice?',   '2026-07-15T09:00:00+00:00'),
  ('66666666-6666-6666-6666-666666666606', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', 'provider','11111111-1111-1111-1111-111111111103', 'The July invoice is in your Invoices tab. Tap it to mark paid once your bank clears it.', '2026-07-15T09:10:00+00:00')
ON CONFLICT (id) DO NOTHING;

-- ===========================================================================
-- cairn_oplog — persisted operation log for reconnect resume (ADR-0025 slice 2).
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
-- NOT IN cairn_pub: this is nostos's internal resume table, not a synced app
-- table. Publishing it would feed its own writes back through logical
-- replication as spurious client events (a feedback loop). Keep it out of the
-- publication.
-- ===========================================================================
CREATE TABLE IF NOT EXISTS cairn_oplog (
    op_id      BIGSERIAL   PRIMARY KEY,
    lsn        BIGINT      NOT NULL,
    table_name TEXT        NOT NULL,
    pk         TEXT        NOT NULL,
    op         TEXT        NOT NULL,                 -- 'upsert' | 'delete'
    payload    JSONB,                                -- NULL for deletes
    tenant_id  TEXT,                                  -- NULL when the row's table has no tenant column
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_cairn_oplog_tenant_lsn ON cairn_oplog (tenant_id, lsn);
CREATE INDEX IF NOT EXISTS idx_cairn_oplog_table_pk   ON cairn_oplog (table_name, pk);
