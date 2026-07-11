#!/usr/bin/env bash
# Stop the `nostos dev` process tree started by nostos_live_up.sh. Does NOT
# touch docker Postgres (it may be shared with other work) — pass --pg to
# also stop it.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./nostos_env.sh

# `cargo run` backgrounded with `&` records cargo's own pid, not the
# nostos-cli/nostos-server grandchildren it execs — kill the whole descendant
# tree (bounded recursion; a dev harness never nests deep).
kill_tree() {
  local pid="$1"
  for child in $(pgrep -P "$pid" 2>/dev/null || true); do
    kill_tree "$child"
  done
  kill -TERM "$pid" 2>/dev/null || true
}

if [ -f "$NOSTOS_DEV_PID_FILE" ]; then
  pid="$(cat "$NOSTOS_DEV_PID_FILE")"
  if kill -0 "$pid" 2>/dev/null; then
    echo "stopping nostos dev (pid $pid) and its process tree..."
    kill_tree "$pid"
    for i in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.5
    done
  fi
  rm -f "$NOSTOS_DEV_PID_FILE"
  echo "  ✓ stopped"
else
  echo "  no pid file — nothing to stop"
fi

if [ "${1:-}" = "--pg" ]; then
  echo "stopping docker postgres..."
  docker compose -f "$NOSTOS_REPO_ROOT/docker/docker-compose.yml" down
fi
