#!/usr/bin/env bash
# Bring up the Nostos "local live" harness for the todo fixture (W5):
#   1. docker Postgres up (idempotent — reuses an already-running container).
#   2. create the `todos` table (idempotent — CREATE TABLE IF NOT EXISTS).
#   3. `nostos init` — real CLI, creates/reconciles the publication, writes
#      nostos.toml + .env under .nostos/ (idempotent re-run per its own doc).
#   4. append the dev JWT secret to .env (nostos init only ever writes
#      NOSTOS_PG_URL there) and pin the server bind to a port that won't
#      collide with the zero-setup default (8800) or the SDK's own
#      integration test (8801).
#   5. `nostos dev` — real CLI, backgrounded; waits for /healthz.
#
# Safe to re-run: each step no-ops or reconciles rather than erroring. Prints
# the ws:// URL + two ready-to-use dev JWTs (user-a / user-b) on success.
#
# Requires: docker, cargo, openssl. First run compiles nostos-cli + nostos-server
# from scratch (see the timing dry-run in docs/QUICKSTART.md).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./nostos_env.sh

echo "== 1/5: docker Postgres =="
docker compose -f "$NOSTOS_REPO_ROOT/docker/docker-compose.yml" up -d postgres
for i in $(seq 1 60); do
  if docker exec nostos-postgres pg_isready -U cairn -d cairn >/dev/null 2>&1; then
    echo "  postgres ready after ${i}s"
    break
  fi
  sleep 1
done
docker exec nostos-postgres pg_isready -U cairn -d cairn >/dev/null 2>&1 \
  || { echo "postgres did not become ready in 60s"; exit 1; }

echo "== 2/5: todos table =="
docker exec -i nostos-postgres psql -U cairn -d cairn -v ON_ERROR_STOP=1 <<'SQL'
CREATE TABLE IF NOT EXISTS todos (
  id text primary key,
  user_id text not null,
  title text not null,
  done boolean not null default false,
  created_at timestamptz not null default now()
);
SQL
echo "  ✓ todos table present"

mkdir -p "$NOSTOS_STATE_DIR"

echo "== 3/5: nostos init =="
# nostos-cli's `init`/`dev`/`doctor` all resolve nostos.toml/.env against the
# PROCESS cwd (crates/nostos-cli/src/main.rs), so this must run from .nostos/.
(cd "$NOSTOS_STATE_DIR" && cargo run --quiet --manifest-path "$NOSTOS_REPO_ROOT/Cargo.toml" -p nostos-cli -- init \
  --db-url "$NOSTOS_PG_URL" \
  --tables todos \
  --write-tables todos \
  --tenant-column "$NOSTOS_TENANT_COLUMN" \
  --publication "$NOSTOS_PUBLICATION" \
  --slot "$NOSTOS_SLOT")

echo "== 4/5: dev JWT secret + bind port =="
if ! grep -q '^NOSTOS_SUPABASE_JWT_SECRET=' "$NOSTOS_STATE_DIR/.env" 2>/dev/null; then
  echo "NOSTOS_SUPABASE_JWT_SECRET=$NOSTOS_DEV_JWT_SECRET" >> "$NOSTOS_STATE_DIR/.env"
fi
# Pin the port `nostos init` doesn't expose a flag for (server.bind always
# defaults to 0.0.0.0:8800 — see crates/nostos-cli/src/commands/init.rs).
sed -i.bak "s#^bind = \".*\"#bind = \"$NOSTOS_BIND\"#" "$NOSTOS_STATE_DIR/nostos.toml"
rm -f "$NOSTOS_STATE_DIR/nostos.toml.bak"
echo "  ✓ $NOSTOS_STATE_DIR/nostos.toml + .env ready"

echo "== 5/5: nostos dev =="
if [ -f "$NOSTOS_DEV_PID_FILE" ] && kill -0 "$(cat "$NOSTOS_DEV_PID_FILE")" 2>/dev/null; then
  echo "  already running (pid $(cat "$NOSTOS_DEV_PID_FILE"))"
else
  (cd "$NOSTOS_STATE_DIR" && nohup cargo run --quiet --manifest-path "$NOSTOS_REPO_ROOT/Cargo.toml" -p nostos-cli -- dev \
    > "$NOSTOS_DEV_LOG" 2>&1 &
    echo $! > "$NOSTOS_DEV_PID_FILE")
  echo "  started (pid $(cat "$NOSTOS_DEV_PID_FILE")); waiting for $NOSTOS_HEALTH_URL ..."
  ready=""
  for i in $(seq 1 180); do
    if curl -sf -o /dev/null "$NOSTOS_HEALTH_URL"; then
      ready=1
      echo "  healthy after ${i}s"
      break
    fi
    sleep 1
  done
  if [ -z "$ready" ]; then
    echo "nostos-server did not become healthy in 180s — see $NOSTOS_DEV_LOG"
    exit 1
  fi
fi

echo
echo "ws URL:  $NOSTOS_WS_URL"
echo "user-a token: $(./mint_jwt.sh user-a)"
echo "user-b token: $(./mint_jwt.sh user-b)"
echo
echo "tool/nostos_live_down.sh to stop."
