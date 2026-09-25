-- Product images move from bundled assets to Supabase Storage, managed by the
-- nostos T6 attachments planes (ADR-0034): metadata = public.attachments
-- (synced, public scope), bytes = bucket `product-images` (object key = id).
-- Applied to the nostos project 2026-09-26 via MCP; the six objects were
-- uploaded with `supabase storage cp design/img/<f> ss:///product-images/<f>`.
insert into storage.buckets (id, name, public)
values ('product-images', 'product-images', true)
on conflict (id) do nothing;

create policy product_images_read on storage.objects
  for select using (bucket_id = 'product-images');

create table if not exists public.attachments (
  id         text primary key,
  filename   text not null,
  size       bigint not null default 0,
  media_type text not null,
  state      text not null default 'synced',
  timestamp  bigint not null default (extract(epoch from now()) * 1000)::bigint
);
alter table public.attachments enable row level security;
-- Catalog images: every authenticated device reads, nobody writes from a device.
create policy attachments_read on public.attachments for select using (true);

-- Direct-mode change log, public scope (no trigger args = scope 'public',
-- mirrors `nostos link --mode direct --public attachments`).
create trigger nostos_log_attachments
  after insert or update or delete on public.attachments
  for each row execute function nostos.log_change();

alter table public.products
  add column if not exists image_id text references public.attachments(id);

insert into public.attachments (id, filename, size, media_type)
values
  ('p1-protein.jpg',     'p1-protein.jpg',      49568, 'image/jpeg'),
  ('p2-hemp.jpg',        'p2-hemp.jpg',        194884, 'image/jpeg'),
  ('p3-oatshake.jpg',    'p3-oatshake.jpg',     86578, 'image/jpeg'),
  ('p4-datenutbar.jpg',  'p4-datenutbar.jpg',   98683, 'image/jpeg'),
  ('p5-tartcherry.jpg',  'p5-tartcherry.jpg',  117330, 'image/jpeg'),
  ('p6-electrolyte.jpg', 'p6-electrolyte.jpg',  48644, 'image/jpeg')
on conflict (id) do nothing;

update public.products set image_id = split_part(image_url, '/', 3)
where image_id is null and image_url like 'design/img/%';
