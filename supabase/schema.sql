-- supabase/schema.sql — Nostos Provider Dashboard schema for Supabase (BYO schema).
--
-- nostos READS your schema via Postgres logical replication; it does NOT create
-- the tables upstream. This file IS the "bring your own schema" step. Paste it
-- into Supabase Dashboard → SQL Editor → New query → Run. Idempotent
-- (CREATE TABLE IF NOT EXISTS / ON CONFLICT DO NOTHING) — safe to re-run.
--
-- After it runs:
--   1. 6 tables (tasks, providers, clients, availabilities, appointments,
--      invoices) + the `cairn_pub` publication exist in your Supabase DB.
--   2. Point nostos-server at Supabase with the DIRECT connection (NOT the
--      pooler — logical replication needs a direct connection):
--        NOSTOS_REPLICATOR=pg "\
--        NOSTOS_PG_URL='postgresql://postgres:<pw>@db.<project>.supabase.co:5432/postgres' "\
--        NOSTOS_WRITE_TABLES=tasks,providers,clients,availabilities,appointments,invoices \
--        cargo run -p nostos-server
--   3. In the app repo: `nostos pull && nostos gen` rebuilds .nostos/schema.json
--      + nostos.g.dart from your Supabase schema.
--
-- Mirrors docker/pg-init/01-sources.sql (schema + publication) + the demo seed
-- (03-seed-booking.sql). Supabase manages DB roles itself, so the least-priv
-- `cairn_writer` role (02-nostos-role.sql) is omitted — nostos connects as
-- `postgres` here. For production, create a dedicated REPLICATION role (ADR-0013/0018).

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
CREATE TABLE IF NOT EXISTS providers (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL,
    specialty  TEXT,
    email      TEXT,
    phone      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
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
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS invoices (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    appointment_id UUID NOT NULL REFERENCES appointments(id) ON DELETE CASCADE,
    client_id      UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    amount_cents   INT NOT NULL CHECK (amount_cents >= 0),
    status         TEXT NOT NULL DEFAULT 'issued',
    issued_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_availabilities_provider ON availabilities (provider_id, weekday);
CREATE INDEX IF NOT EXISTS idx_appointments_provider  ON appointments (provider_id, starts_at);
CREATE INDEX IF NOT EXISTS idx_appointments_client    ON appointments (client_id, starts_at);
CREATE INDEX IF NOT EXISTS idx_invoices_appointment   ON invoices (appointment_id);
CREATE INDEX IF NOT EXISTS idx_invoices_client        ON invoices (client_id);

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
    FOREACH t IN ARRAY ARRAY['providers','clients','availabilities','appointments','invoices'] LOOP
        IF NOT EXISTS (
            SELECT 1 FROM pg_publication_tables
            WHERE pubname = 'cairn_pub' AND schemaname = 'public' AND tablename = t
        ) THEN
            EXECUTE format('ALTER PUBLICATION cairn_pub ADD TABLE public.%I', t);
        END IF;
    END LOOP;
END $$;

-- ── demo seed (from 03-seed-booking.sql) ────────────────────────────────────

-- Seed data for the provider-dashboard demo (D4).
-- appointments/invoices are created at runtime via the app (offline-first
-- writes round-trip through nostos). Fixed UUIDs so FK refs hold; idempotent so
-- re-init is safe. Sourced from 01-sources.sql (must run after it).
INSERT INTO providers (id, name, specialty, email, phone) VALUES
  ('11111111-1111-1111-1111-111111111101', 'Dr. Ada Lovelace', 'Cardiology',   'ada@nostos.run',   '555-0101'),
  ('11111111-1111-1111-1111-111111111102', 'Dr. Marie Curie',  'Dermatology',  'marie@nostos.run', '555-0102'),
  ('11111111-1111-1111-1111-111111111103', 'Dr. Alan Turing',  'General',      'alan@nostos.run',  '555-0103')
ON CONFLICT (id) DO NOTHING;

INSERT INTO clients (id, name, email, phone, notes) VALUES
  ('22222222-2222-2222-2222-222222222201', 'Grace Hopper',     'grace@nostos.run', '555-0201', 'Prefers mornings'),
  ('22222222-2222-2222-2222-222222222202', 'Katherine Johnson','kate@nostos.run',  '555-0202', NULL),
  ('22222222-2222-2222-2222-222222222203', 'Linus Torvalds',   'linus@nostos.run', '555-0203', 'New patient'),
  ('22222222-2222-2222-2222-222222222204', 'Hedy Lamarr',      'hedy@nostos.run',  '555-0204', NULL)
ON CONFLICT (id) DO NOTHING;

-- Recurring weekly slots (weekday: 1=Mon..5=Fri). Minutes-from-midnight:
-- 09:00=540, 12:00=720, 13:00=780, 17:00=1020.
INSERT INTO availabilities (id, provider_id, weekday, start_min, end_min) VALUES
  ('33333333-3333-3333-3333-333333333301', '11111111-1111-1111-1111-111111111101', 1, 540,  720),
  ('33333333-3333-3333-3333-333333333302', '11111111-1111-1111-1111-111111111101', 1, 780, 1020),
  ('33333333-3333-3333-3333-333333333303', '11111111-1111-1111-1111-111111111101', 3, 540,  720),
  ('33333333-3333-3333-3333-333333333304', '11111111-1111-1111-1111-111111111102', 2, 540, 1020),
  ('33333333-3333-3333-3333-333333333305', '11111111-1111-1111-1111-111111111103', 4, 780, 1020)
ON CONFLICT (id) DO NOTHING;
