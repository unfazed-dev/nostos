-- 0009 updated the order-event title only when cairn.push_templates existed.
-- Fresh projects linked after ADR-0048 have nostos.push_templates instead, so
-- that guard skipped the icon. Correct those projects without doubling the
-- prefix on projects where a re-link already supplied it.
do $$
begin
  if to_regclass('nostos.push_templates') is not null then
    update nostos.push_templates
       set title = '{icon} ' || title
     where table_name = 'order_events' and title not like '{icon}%';
  end if;
end $$;
