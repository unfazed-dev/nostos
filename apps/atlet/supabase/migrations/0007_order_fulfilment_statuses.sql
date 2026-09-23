-- 0001 modelled an order as a payment outcome only ('pending'|'paid'|'failed'),
-- so there was nothing to notify about after checkout succeeded — the push
-- pilot's whole point is a status change that arrives while the app is closed.
-- Fulfilment is the part the customer actually waits for; add it as two more
-- terminal-ish states rather than a separate table, because the order row is
-- already what `watchOrders()` renders and what the doorbell wakes for.
alter table public.orders drop constraint orders_status_check;
alter table public.orders add constraint orders_status_check
  check (status in ('pending', 'paid', 'failed', 'shipped', 'delivered'));
