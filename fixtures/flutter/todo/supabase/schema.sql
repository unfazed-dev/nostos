create table if not exists todos (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references auth.users (id) on delete cascade,
  title text not null,
  done boolean not null default false,
  created_at timestamptz not null default now()
);

alter table todos enable row level security;

create policy "owner full access" on todos
  for all using (auth.uid() = user_id) with check (auth.uid() = user_id);
