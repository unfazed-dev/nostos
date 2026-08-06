-- bench_now(): exposes Postgres server time to the bench harness via RPC.
-- PostgREST has no bare "select now()" endpoint; a stable function must be
-- exposed explicitly. Named bench_now (not now) to avoid shadowing the
-- builtin in the public schema. Used by lib/bench/clock.dart's
-- BenchClock.estimateOffset default probe (RTT/2-corrected offset
-- estimation, spec/metrics.md). Not applied automatically by this task —
-- flagged to team-lead to run against the live project.
create or replace function public.bench_now()
returns timestamptz
language sql
stable
as $$
  select now();
$$;
