-- ADR-0047's `collapse` on order_events was `order-{order_id}`: every status
-- push for an order carried the same apns-collapse-id / FCM collapse_key, so
-- shipped replaced paid and delivered replaced shipped. A user who missed the
-- tray saw one notification per order, never the three events (measured on
-- atlet iOS 2026-09-25). Per order AND status: a re-sent status still replaces
-- itself, each distinct status keeps its own row in the tray.
--
-- The row belongs to `nostos link --visible`; a re-link must pass the same
-- spec (see 0009) or this reverts. Guarded because nostos.push_templates
-- exists only once a project is linked with push.
do $$
begin
  if to_regclass('nostos.push_templates') is not null then
    update nostos.push_templates
       set options = coalesce(options, '{}'::jsonb)
                     || jsonb_build_object('collapse', 'order-{order_id}-{status}')
     where table_name = 'order_events';
  end if;
end $$;
