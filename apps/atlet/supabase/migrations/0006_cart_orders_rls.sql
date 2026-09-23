-- cart_items and orders were created with RLS enabled and zero policies, which
-- in Postgres means deny-all: every insert from the app came back
-- 42501 "new row violates row-level security policy" and was dead-lettered,
-- so the cart was permanently empty on screen (caught 2026-09-23).
--
-- Mirrors sessions_own from 0001: owner-scoped, all commands, checked on the
-- way in as well as out so a device cannot write a row it could not read.
create policy cart_items_own on public.cart_items
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());

create policy orders_own on public.orders
  for all using (user_id = auth.uid()) with check (user_id = auth.uid());
