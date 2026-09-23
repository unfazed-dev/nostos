-- The Shop tables. Backfilled from the live migration atlet_cart_orders
-- (applied 2026-08-07, never checked in), publication name updated to
-- atlet_pub. Replica identity is set in 0005, RLS policies in 0006, the
-- fulfilment statuses in 0007.
create table if not exists public.cart_items (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid(),
  product_id uuid not null references public.products(id),
  qty integer not null default 1 check (qty > 0),
  added_at timestamptz not null default now()
);
create table if not exists public.orders (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid(),
  status text not null default 'paid' check (status in ('pending','paid','failed')),
  subtotal_cents integer not null default 0,
  tax_cents integer not null default 0,
  shipping_cents integer not null default 0,
  total_cents integer not null default 0,
  payment_ref text,
  items_json text,
  created_at timestamptz not null default now()
);
-- Live got RLS from Supabase's ensure_rls event trigger; explicit here so a
-- project without that trigger ends up the same.
alter table public.cart_items enable row level security;
alter table public.orders enable row level security;
alter publication atlet_pub add table public.cart_items, public.orders;
