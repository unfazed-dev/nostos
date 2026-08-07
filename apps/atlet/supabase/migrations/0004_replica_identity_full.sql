-- ADR-0025 F1: every cairn-synced table needs REPLICA IDENTITY FULL so live
-- DELETEs (and UPDATE old-rows) carry the tenant column ("user_id") in the WAL.
-- Under the default (PK-only) identity, tenant-scoped delete fan-out silently
-- drops the event and clients never see the row disappear.
-- cairn-server audits this at connect and logs an error per offending table.
alter table public.sessions       replica identity full;
alter table public.products       replica identity full;
alter table public.analytics_runs replica identity full;
alter table public.cart_items     replica identity full;
alter table public.orders         replica identity full;
