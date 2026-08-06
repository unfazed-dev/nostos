-- Atlet canonical schema (decision #2: one schema, per-SDK auth users + RLS)
create table if not exists public.sessions (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid() references auth.users(id),
  title text not null,
  type text not null check (type in ('distance','reps','time')),
  metric int not null,
  unit text not null check (unit in ('km','reps','sec')),
  note text,
  streak int not null default 0,
  occurred_on date not null default current_date,
  -- clock authority for propagation metrics (spec/metrics.md); the client
  -- inserts NULL and the filled value syncing back IS the serverAcked mark.
  server_committed_at timestamptz default now()
);
create table if not exists public.products (
  id uuid primary key default gen_random_uuid(),
  name text not null,
  category text not null,
  price_cents int not null,
  rating numeric(2,1),
  plant_based boolean not null default false,
  image_url text
);
create table if not exists public.analytics_runs (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid() references auth.users(id),
  sdk text not null,
  engine text not null check (engine in ('cairn','powersync')),
  profile text not null check (profile in ('local','cloud')),
  run_type text not null,
  spec_version text not null,
  device jsonb not null,
  metrics jsonb not null,
  started_at timestamptz not null,
  uploaded_at timestamptz not null default now()
);
alter table public.sessions enable row level security;
alter table public.products enable row level security;
alter table public.analytics_runs enable row level security;
create policy sessions_own on public.sessions
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());
create policy products_read on public.products for select using (true);
create policy runs_own on public.analytics_runs
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());
create index sessions_user_occurred on public.sessions (user_id, occurred_on desc);
