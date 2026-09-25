-- image_id = the attachments row / storage object key (migration 0012).
insert into public.products (name, category, price_cents, rating, plant_based, image_url, image_id)
select
  'Product ' || g, (array['protein','hemp','oatshake','datenutbar','tartcherry','electrolyte'])[1 + g % 6],
  1500 + (g * 37) % 4000, round((3 + (g % 20) * 0.1)::numeric, 1), g % 3 = 0,
  'design/img/p' || (1 + g % 6) || '-' ||
  (array['protein','hemp','oatshake','datenutbar','tartcherry','electrolyte'])[1 + g % 6] || '.jpg',
  'p' || (1 + g % 6) || '-' ||
  (array['protein','hemp','oatshake','datenutbar','tartcherry','electrolyte'])[1 + g % 6] || '.jpg'
from generate_series(1, 1000) g;
