#!/usr/bin/env bash
# Sets/rotates powersync_role's password. Split out of 0002_powersync_replication.sql
# because psql's `:'var'` interpolation only resolves in an actual psql session, so
# a literal password in the migration file breaks any other executor (execute_sql,
# `supabase db push`, `supabase migration up`, `supabase db reset`).
#
# DATABASE_URL needs a direct Postgres connection. db.<ref>.supabase.co is
# IPv6-only (AAAA-only DNS) — if the dev network drops IPv6, route through
# scripts/warp-ipv6-egress.sh (relay at 127.0.0.1:15433, sslmode=disable) first.
set -euo pipefail
: "${DATABASE_URL:?}" "${POWERSYNC_ROLE_PASSWORD:?}"
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -v pw="$POWERSYNC_ROLE_PASSWORD" <<'SQL'
alter role powersync_role with password :'pw';
SQL
echo "powersync_role password set"
