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
