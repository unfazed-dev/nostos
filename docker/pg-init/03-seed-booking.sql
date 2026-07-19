-- Seed data for the provider-dashboard demo.
-- Fixed UUIDs so FK refs hold. Providers use ON CONFLICT DO UPDATE so re-runs
-- force the rate columns onto pre-existing rows (ON CONFLICT DO NOTHING would
-- leave legacy rows with rate_type='hourly' default + 0 rates). Other tables
-- use DO NOTHING (their seed data is stable).
-- Sourced from 01-sources.sql (must run after it).
--
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
INSERT INTO appointments (id, provider_id, client_id, starts_at, duration_min, status, notes) VALUES
  ('44444444-4444-4444-4444-444444444401', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', '2026-07-10T09:00:00+00:00', 60, 'completed',  'Annual heart check-up'),
  ('44444444-4444-4444-4444-444444444402', '11111111-1111-1111-1111-111111111102', '22222222-2222-2222-2222-222222222204', '2026-07-11T10:00:00+00:00', 30, 'completed',  'Skin screening'),
  ('44444444-4444-4444-4444-444444444403', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', '2026-07-14T14:00:00+00:00', 45, 'completed',  'Initial concierge visit'),
  ('44444444-4444-4444-4444-444444444404', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', '2026-07-22T09:00:00+00:00', 30, 'confirmed',  'Follow-up ECG'),
  ('44444444-4444-4444-4444-444444444405', '11111111-1111-1111-1111-111111111104', '22222222-2222-2222-2222-222222222204', '2026-07-23T11:00:00+00:00', 90, 'confirmed',  'Migraine workup')
ON CONFLICT (id) DO NOTHING;

-- Invoices auto-calculated at issue (rate snapshot captured). Demonstrates each
-- line_type. amounts are the exact BillingService outputs so the demo is
-- self-consistent: Ada hourly (60min × $250/hr = $250), Marie flat ($180),
-- Alan subscription ($800).
INSERT INTO invoices (id, appointment_id, client_id, provider_id, amount_cents, line_type, rate_cents, hours_min, description, status, issued_at, due_at, paid_at) VALUES
  ('55555555-5555-5555-5555-555555555501', '44444444-4444-4444-4444-444444444401', '22222222-2222-2222-2222-222222222201', '11111111-1111-1111-1111-111111111101', 25000, 'hourly',       25000, 60, 'Cardiology consult — 1.0 hr @ $250/hr',         'paid',   '2026-07-10T10:05:00+00:00', '2026-07-24T10:05:00+00:00', '2026-07-12T08:00:00+00:00'),
  ('55555555-5555-5555-5555-555555555502', '44444444-4444-4444-4444-444444444402', '22222222-2222-2222-2222-222222222204', '11111111-1111-1111-1111-111111111102', 18000, 'flat',         18000,  0, 'Dermatology skin screening — flat fee',          'paid',   '2026-07-11T11:00:00+00:00', '2026-07-25T11:00:00+00:00', '2026-07-13T09:30:00+00:00'),
  ('55555555-5555-5555-5555-555555555503', '44444444-4444-4444-4444-444444444403', '22222222-2222-2222-2222-222222222203', '11111111-1111-1111-1111-111111111103', 80000, 'subscription', 80000,  0, 'Concierge primary care — July subscription',     'issued', '2026-07-14T15:00:00+00:00', '2026-07-28T15:00:00+00:00', NULL)
ON CONFLICT (id) DO NOTHING;

-- Sample chat threads. Demonstrates the realtime-chat-as-synced-table pattern.
INSERT INTO messages (id, provider_id, client_id, sender_type, sender_id, body, created_at) VALUES
  ('66666666-6666-6666-6666-666666666601', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'client',  '22222222-2222-2222-2222-222222222201', 'Hi Dr. Lovelace, is my Tuesday appointment still on?', '2026-07-20T15:00:00+00:00'),
  ('66666666-6666-6666-6666-666666666602', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'provider','11111111-1111-1111-1111-111111111101', 'Yes — 9:00am for the follow-up ECG. See you then.',    '2026-07-20T15:02:00+00:00'),
  ('66666666-6666-6666-6666-666666666603', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'client',  '22222222-2222-2222-2222-222222222201', 'Great, do I need to fast beforehand?',                 '2026-07-20T15:03:00+00:00'),
  ('66666666-6666-6666-6666-666666666604', '11111111-1111-1111-1111-111111111101', '22222222-2222-2222-2222-222222222201', 'provider','11111111-1111-1111-1111-111111111101', 'No fasting needed for an ECG. Just come as you are.',  '2026-07-20T15:05:00+00:00'),
  ('66666666-6666-6666-6666-666666666605', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', 'client',  '22222222-2222-2222-2222-222222222203', 'Dr. Turing, how do I pay the subscription invoice?',   '2026-07-15T09:00:00+00:00'),
  ('66666666-6666-6666-6666-666666666606', '11111111-1111-1111-1111-111111111103', '22222222-2222-2222-2222-222222222203', 'provider','11111111-1111-1111-1111-111111111103', 'The July invoice is in your Invoices tab. Tap it to mark paid once your bank clears it.', '2026-07-15T09:10:00+00:00')
ON CONFLICT (id) DO NOTHING;
