-- A stand-in vendor, so an order moves on after checkout with nobody at a
-- terminal: paid → shipped 10 s after payment, shipped → delivered 15 s after
-- shipping. The push pilot exists for the notification that lands while the
-- app is closed (0007), yet nothing but the push smoke harness ever moved an
-- order past `paid`, so with the app killed there was simply nothing to send.
--
-- Each flip takes the path a real fulfilment would: the update fires 0008's
-- record_order_event, the event row fires cairn.log_change, and with no device
-- awake the change log posts the templated push.
--
-- ponytail: a 1-second pg_cron poll, so the flips land 10–11 s and 15–16 s
-- out. A real vendor would set the status itself; drop the job when one does.

create extension if not exists pg_cron;

-- The notification's icon: one glyph per status, the same pictures the History
-- tab draws (lib/ui/history.dart `_statusIcon`). APNs has no per-notification
-- icon — the app icon is the icon — so the glyph rides at the front of the
-- title, filled from `{icon}` like any other column of the row.
alter table public.order_events
  add column if not exists icon text generated always as (
    case status
      when 'pending' then '⏳'
      when 'paid' then '💳'
      when 'failed' then '⚠️'
      when 'shipped' then '🚚'
      when 'delivered' then '✅'
    end
  ) stored;

-- Only a status set in the last 2 minutes moves. Orders already sitting at
-- `paid` stay there, so switching the vendor on does not ship the whole backlog
-- at once — one push per order. The cost: an order the job misses for 2
-- minutes (cron down) stays where it is.
create or replace function public.atlet_fulfil_orders() returns void
language sql
set search_path = ''
as $$
  update public.orders o set status = 'delivered'
   where o.status = 'shipped'
     and exists (
       select 1 from public.order_events e
        where e.order_id = o.id and e.status = 'shipped'
          and e.created_at between now() - interval '2 minutes'
                               and now() - interval '15 seconds');
  update public.orders o set status = 'shipped'
   where o.status = 'paid'
     and exists (
       select 1 from public.order_events e
        where e.order_id = o.id and e.status = 'paid'
          and e.created_at between now() - interval '2 minutes'
                               and now() - interval '10 seconds');
$$;

-- The vendor is the cron job, not a device: public functions are callable
-- over PostgREST, and a customer must not be able to fulfil their own order.
revoke execute on function public.atlet_fulfil_orders() from public, anon, authenticated;

select cron.schedule('atlet-order-vendor', '1 seconds', 'select public.atlet_fulfil_orders()');

-- pg_cron keeps a row per run and never deletes one; a 1-second job adds 86k a
-- day.
select cron.schedule(
  'atlet-order-vendor-trim', '*/10 * * * *',
  $$delete from cron.job_run_details
     where jobid = (select jobid from cron.job where jobname = 'atlet-order-vendor')
       and end_time < now() - interval '10 minutes'$$
);

-- Put the icon in the push. The row belongs to `nostos link --visible`, so a
-- re-link must pass the same title or the icon goes. The `collapse` option
-- (ADR-0047) is per order AND status since 0011: the same status re-sent
-- replaces itself, but paid, shipped and delivered each keep their own
-- notification (with `collapse=order-{order_id}` only "delivered" survived):
--   order_events:action@/history/{id}[collapse=order-{order_id}-{status}]:order_status:{icon} Atlet order update:Your order is {status}
-- Guarded because cairn.push_templates exists only once a project is linked
-- with push.
do $$
begin
  if to_regclass('cairn.push_templates') is not null then
    update cairn.push_templates set title = '{icon} ' || title
     where table_name = 'order_events' and title not like '{icon}%';
  end if;
end $$;
