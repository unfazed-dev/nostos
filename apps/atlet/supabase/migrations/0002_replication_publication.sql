-- The scoped publication atlet's cairn-server replicates from
-- (services/docker-compose.atlet.yml: CAIRN_PG_PUBLICATION=atlet_pub), instead
-- of the FOR ALL TABLES cairn_pub cairn would otherwise auto-create.
do $$ begin
  if not exists (select from pg_publication where pubname = 'atlet_pub') then
    create publication atlet_pub for table public.sessions, public.products, public.analytics_runs;
  end if;
end $$;
