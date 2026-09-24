-- ADR-0048: the engine ids follow the rename. The app now records `nostos` and
-- `nostosDirect`, which the check from 0001 does not admit, so every bench run
-- would fail to save. The runs already recorded move with it.
--
-- The `cairn` schema, its RPCs and the `cairn_log_*` triggers (0008 wrote
-- one by hand) are not this file's job: the next `nostos link --mode direct`
-- renames them in place.
alter table public.analytics_runs drop constraint if exists analytics_runs_engine_check;
update public.analytics_runs set engine = replace(engine, 'cairn', 'nostos')
 where engine in ('cairn', 'cairnDirect');
alter table public.analytics_runs add constraint analytics_runs_engine_check
  check (engine in ('nostos', 'nostosDirect'));
