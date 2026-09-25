-- Checks 0009's vendor against a throwaway order, then rolls everything back:
-- no row survives, and no push leaves (pg_net only sends committed requests).
--
-- It ends in an error ON PURPOSE, because the error is what rolls it back.
-- "order vendor check passed" means success. Any other error is a failure.
-- Run it with psql -f or paste it into the SQL editor.
do $$
declare
  v_order uuid;
  v_old uuid;
  v_status text;
begin
  insert into public.orders (user_id) select id from auth.users limit 1
  returning id into v_order;
  assert v_order is not null, 'no auth user to own the test order';

  perform public.atlet_fulfil_orders();
  select status into v_status from public.orders where id = v_order;
  assert v_status = 'paid', 'shipped before 10 s: ' || v_status;

  update public.order_events set created_at = now() - interval '11 seconds'
   where order_id = v_order;
  perform public.atlet_fulfil_orders();
  select status into v_status from public.orders where id = v_order;
  assert v_status = 'shipped', 'not shipped after 10 s: ' || v_status;
  assert (select icon from public.order_events
           where order_id = v_order and status = 'shipped') = '🚚',
    'shipped event has no 🚚 icon';

  update public.order_events set created_at = now() - interval '16 seconds'
   where order_id = v_order and status = 'shipped';
  perform public.atlet_fulfil_orders();
  select status into v_status from public.orders where id = v_order;
  assert v_status = 'delivered', 'not delivered after 15 s: ' || v_status;

  -- The backlog guard: an order paid before the window never moves.
  insert into public.orders (user_id) select id from auth.users limit 1
  returning id into v_old;
  update public.order_events set created_at = now() - interval '3 minutes'
   where order_id = v_old;
  perform public.atlet_fulfil_orders();
  select status into v_status from public.orders where id = v_old;
  assert v_status = 'paid', 'backlog order moved: ' || v_status;

  raise exception 'order vendor check passed (rolled back)';
end $$;
