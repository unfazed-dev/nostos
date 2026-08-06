-- PowerSync replication plumbing (research-powersync-sdk-surface-2026-08-06.md)
-- Password intentionally not set here: `password :'VAR'` is psql-only interpolation
-- and errors under any other executor (execute_sql, `supabase db push`,
-- `supabase migration up`, `supabase db reset`). Set/rotate it via
-- scripts/provision_powersync_role.sh (reads POWERSYNC_ROLE_PASSWORD; uses
-- ALTER ROLE, so it's idempotent whether this migration already ran or not).
do $$ begin
  if not exists (select from pg_roles where rolname = 'powersync_role') then
    create role powersync_role with replication bypassrls login;
  end if;
end $$;
grant select on public.sessions, public.products to powersync_role;
do $$ begin
  if not exists (select from pg_publication where pubname = 'powersync') then
    create publication powersync for table public.sessions, public.products;
  end if;
end $$;
