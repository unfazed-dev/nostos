-- PowerSync replication plumbing (research-powersync-sdk-surface-2026-08-06.md)
do $$ begin
  if not exists (select from pg_roles where rolname = 'powersync_role') then
    create role powersync_role with replication bypassrls login
      password :'POWERSYNC_ROLE_PASSWORD';
  end if;
end $$;
grant select on public.sessions, public.products to powersync_role;
do $$ begin
  if not exists (select from pg_publication where pubname = 'powersync') then
    create publication powersync for table public.sessions, public.products;
  end if;
end $$;
