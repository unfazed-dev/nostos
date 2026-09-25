-- The shop's history, as rows rather than as banners.
--
-- An order row only ever carries its CURRENT status, so "paid at 14:02,
-- shipped at 16:40, delivered yesterday" existed nowhere: not on the server,
-- not on the device, and a notification the user missed was simply gone. The
-- push pilot needs exactly that list to be testable — every transition it
-- should have notified about, durable, and reaching the device through the
-- same change-log path as everything else (ADR-0045).
--
-- Written by a trigger on public.orders rather than by the app: the statuses
-- that matter for the pilot (shipped, delivered) are set by whoever fulfils
-- the order, which is never the device.
create table if not exists public.order_events (
  id uuid primary key default gen_random_uuid(),
  order_id uuid not null references public.orders(id) on delete cascade,
  user_id uuid not null references auth.users(id) on delete cascade,
  status text not null
    check (status in ('pending', 'paid', 'failed', 'shipped', 'delivered')),
  -- NULL on the row that records the order's creation: there was no previous.
  previous_status text,
  note text,
  created_at timestamptz not null default now()
);

create index if not exists order_events_user_created_idx
  on public.order_events (user_id, created_at desc);

alter table public.order_events enable row level security;

-- Read-only to the device. Every row is written by the trigger below, inside
-- the same transaction as the status change it describes; a device that could
-- insert here could also lie about its own order history.
drop policy if exists order_events_own_read on public.order_events;
create policy order_events_own_read on public.order_events
  for select using (user_id = auth.uid());

grant select on public.order_events to authenticated;

-- SECURITY DEFINER because the insert happens under whichever role changed the
-- order — including `authenticated`, which the policy above deliberately gives
-- no insert path.
create or replace function public.record_order_event() returns trigger
language plpgsql
security definer
set search_path = public
as $$
begin
  -- `update ... set status = status` is how a policy change gets replayed into
  -- the change log; it must not manufacture a history entry.
  if tg_op = 'UPDATE' and new.status is not distinct from old.status then
    return new;
  end if;
  insert into public.order_events (order_id, user_id, status, previous_status)
  values (
    new.id,
    new.user_id,
    new.status,
    case when tg_op = 'UPDATE' then old.status end
  );
  return new;
end;
$$;

drop trigger if exists orders_record_event on public.orders;
create trigger orders_record_event
  after insert or update of status on public.orders
  for each row execute function public.record_order_event();

-- Sync it the way every other user-scoped table syncs: nostos.log_change stamps
-- `sub:<user_id>`, so a pull only ever hands a device its own rows. Guarded:
-- on a project not yet linked (fresh, 2026-09-25) the schema does not exist,
-- and `nostos link` owns this trigger anyway now that order_events is in
-- nostos_rules.toml. (Written as `cairn` before ADR-0048; the link renames.)
do $$
begin
  if to_regnamespace('nostos') is not null then
    drop trigger if exists nostos_log_order_events on public.order_events;
    create trigger nostos_log_order_events
      after insert or delete or update on public.order_events
      for each row execute function nostos.log_change('user_id', 'sub');
  end if;
end $$;

-- Orders that predate the trigger get one row each, so the History tab is not
-- empty for an account that has been shopping since August.
insert into public.order_events (order_id, user_id, status, note, created_at)
select o.id, o.user_id, o.status, 'backfilled from the order row', o.created_at
from public.orders o
where not exists (
  select 1 from public.order_events e where e.order_id = o.id
);

-- nostos_snapshot() is generated per table list (`nostos link`/`nostos deploy`),
-- so adding a table means regenerating it. Bootstrap and 410-recovery both go
-- through here; without the new arm a fresh device would see order_events only
-- for transitions that happen after it first syncs.
create or replace function public.nostos_snapshot() returns jsonb
language sql
stable
security invoker
set search_path = public
as $$
  with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon),
  snap as (
    select h.horizon, null::text as table_name, null::text as pk, null::jsonb as "row" from h
    union all
    select h.horizon, 'sessions'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'sessions'::text, r.id::text, to_jsonb(r) from public.sessions r, h
    union all
    select h.horizon, 'products'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'products'::text, r.id::text, to_jsonb(r) from public.products r, h
    union all
    select h.horizon, 'analytics_runs'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'analytics_runs'::text, r.id::text, to_jsonb(r) from public.analytics_runs r, h
    union all
    select h.horizon, 'cart_items'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'cart_items'::text, r.id::text, to_jsonb(r) from public.cart_items r, h
    union all
    select h.horizon, 'orders'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'orders'::text, r.id::text, to_jsonb(r) from public.orders r, h
    union all
    select h.horizon, 'order_events'::text, null::text, null::jsonb from h
    union all
    select h.horizon, 'order_events'::text, r.id::text, to_jsonb(r) from public.order_events r, h
  )
  select coalesce(jsonb_agg(to_jsonb(snap)), '[]'::jsonb) from snap
$$;

-- A trigger function has no business being an RPC endpoint. It is SECURITY
-- DEFINER (the insert must outrun the device's own read-only policy), and
-- PostgREST exposes every function in `public` by default — the linter flags
-- exactly that pair.
revoke execute on function public.record_order_event() from public, anon, authenticated;
