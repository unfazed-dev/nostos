#!/usr/bin/env bash
# apps/atlet/flutter/tool/device_roundtrip.sh — the NO-CREDENTIALS device leg:
# the A5/B5 sync substance (offline -> server-side insert -> online -> delta
# apply) with zero operator-owned secrets. push_smoke.sh proves the full
# real-rail doorbell but needs NOSTOS_FCM_CREDENTIALS_JSON + a live Supabase
# project; this companion needs only docker + a device/emulator, so it runs
# anywhere — the honest fallback when the smoke self-skips.
#
# Usage (from anywhere):
#   apps/atlet/flutter/tool/device_roundtrip.sh                 # android leg
#   PUSH_SMOKE_DEVICE=ios PUSH_SMOKE_DEVICE_ID=<id> \
#     NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:8081/sync \
#     apps/atlet/flutter/tool/device_roundtrip.sh               # iOS device
#
# Self-skips (exit 0, SKIP <reason>) like push_smoke.sh.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$SCRIPT_DIR/.."
ROOT_DIR="$SCRIPT_DIR/../../.."

GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; RED=$'\033[0;31m'; RESET=$'\033[0m'

SLOT="${PUSH_SMOKE_SLOT:-atlet_roundtrip_slot}"
PUB="${PUSH_SMOKE_PUB:-atlet_roundtrip_pub}"
TABLE="sessions"
PORT=8081
SERVER_LOG=/tmp/atlet-roundtrip-server.log
APP_LOG=/tmp/atlet-roundtrip-app.log
PG_CONTAINER=nostos-postgres
SERVER_PID=""

skip() { printf "  ${YELLOW}SKIP${RESET}  %s\n" "$1"; exit 0; }

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null
  fi
  docker exec "$PG_CONTAINER" psql -U cairn -d cairn -qc \
    "SELECT pg_drop_replication_slot('$SLOT') FROM pg_replication_slots WHERE slot_name='$SLOT';" \
    >/dev/null 2>&1
}
trap cleanup EXIT

# ---- 1. inputs ------------------------------------------------------------
command -v flutter >/dev/null 2>&1 || skip "flutter not on PATH"
command -v cargo  >/dev/null 2>&1 || skip "cargo not on PATH"

DEVICE_MODE="${PUSH_SMOKE_DEVICE:-android}"
case "$DEVICE_MODE" in
  android)
    ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
    [ -x "$ADB" ] || skip "adb not found (set ANDROID_HOME or install Android SDK)"
    if grep -q '^emulator-[0-9]*[[:space:]]*device' <<< "$("$ADB" devices 2>/dev/null || true)"; then
      DEVICE_ID="$(grep '^emulator-[0-9]*[[:space:]]*device' <<< "$("$ADB" devices 2>/dev/null)" | head -1 | cut -f1)"
    else
      skip "no booted Android emulator"
    fi
    BIND=127.0.0.1
    SYNC_URL="${NOSTOS_SYNC_URL:-ws://10.0.2.2:$PORT/sync}"
    ;;
  ios)
    [ -n "${PUSH_SMOKE_DEVICE_ID:-}" ] || skip "ios mode needs PUSH_SMOKE_DEVICE_ID=<physical device id>"
    [ -n "${NOSTOS_SYNC_URL:-}" ]       || skip "ios mode needs NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:$PORT/sync"
    DEVICE_ID="$PUSH_SMOKE_DEVICE_ID"
    BIND=0.0.0.0
    SYNC_URL="$NOSTOS_SYNC_URL"
    ;;
  *) skip "PUSH_SMOKE_DEVICE must be 'android' or 'ios' (got '$DEVICE_MODE')";;
esac

# ---- 2. docker PG (repo e2e stack) ---------------------------------------
if ! docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1; then
  docker compose -f "$ROOT_DIR/docker/docker-compose.yml" up -d >/dev/null 2>&1 \
    || skip "docker PG not reachable and failed to start"
fi
for _ in $(seq 1 30); do
  docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1 \
  || skip "docker PG never became ready"
psql_exec() { docker exec "$PG_CONTAINER" psql -U cairn -d cairn -qAt -c "$1"; }

psql_exec "DROP PUBLICATION IF EXISTS $PUB;" >/dev/null
psql_exec "DROP TABLE IF EXISTS $TABLE;" >/dev/null
psql_exec "CREATE TABLE $TABLE (
  id text PRIMARY KEY,
  user_id text NOT NULL,
  title text NOT NULL,
  type text NOT NULL,
  metric int NOT NULL,
  unit text NOT NULL,
  occurred_on date NOT NULL);" >/dev/null
psql_exec "ALTER TABLE $TABLE REPLICA IDENTITY FULL;" >/dev/null
psql_exec "CREATE PUBLICATION $PUB FOR TABLE $TABLE;" >/dev/null
psql_exec "SELECT pg_drop_replication_slot('$SLOT') FROM pg_replication_slots WHERE slot_name='$SLOT';" >/dev/null

# ---- 3. nostos-server: auth=none, pg replicator, NO push rails -------------
# Rule-file isolation: the server resolves nostos_rules.toml from its cwd,
# and the repo root carries the operator's hand-mode atlet rules whose
# claim-gated scopes reject every token-less subscribe. Explicit all-mode
# rules = the documented zero-config default, independent of cwd.
RULES_ALL="$(mktemp /tmp/nostos_roundtrip_rules.XXXXXX.toml)"
printf 'version = 1\nsync_mode = "all"\n' >"$RULES_ALL"
printf "  starting nostos-server (cargo run, log: $SERVER_LOG)…\n"
NOSTOS_BIND="$BIND:$PORT" \
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL="postgres://cairn:cairn@localhost:5433/cairn" \
NOSTOS_PG_PUBLICATION="$PUB" \
NOSTOS_PG_SLOT="$SLOT" \
NOSTOS_SYNC_AUTH=none \
NOSTOS_WRITE_TABLES="$TABLE" \
NOSTOS_RULES_FILE="$RULES_ALL" \
  cargo run -q -p nostos-server >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 90); do
  if python3 -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:$PORT/healthz',timeout=2).status==200 else 1)" 2>/dev/null; then break; fi
  kill -0 "$SERVER_PID" 2>/dev/null || { printf "  ${RED}FAIL${RESET} nostos-server exited early\n"; tail -5 "$SERVER_LOG"; exit 1; }
  sleep 1
done
python3 -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:$PORT/healthz',timeout=2).status==200 else 1)" 2>/dev/null \
  || { printf "  ${RED}FAIL${RESET} nostos-server never became healthy\n"; exit 1; }

# ---- 4. device leg --------------------------------------------------------
ROW_ID="roundtrip-$(date +%s)"
printf "  running atlet round-trip on %s [%s] (log: $APP_LOG)…\n" "$DEVICE_MODE" "$DEVICE_ID"
( cd "$APP_DIR" && flutter pub get >/dev/null 2>&1 && \
  flutter test integration_test/device_roundtrip_test.dart -d "$DEVICE_ID" \
    --dart-define=NOSTOS_SYNC_URL="$SYNC_URL" \
    --dart-define=ROUNDTRIP_ROW_ID="$ROW_ID" \
    --dart-define=ROUNDTRIP_INSERT_SECS=12 ) >"$APP_LOG" 2>&1 &
APP_PID=$!

for _ in $(seq 1 600); do
  grep -q 'DEVICE_ROUNDTRIP_READY' "$APP_LOG" 2>/dev/null && break
  kill -0 "$APP_PID" 2>/dev/null || break
  sleep 1
done
grep -q 'DEVICE_ROUNDTRIP_READY' "$APP_LOG" 2>/dev/null \
  || { printf "  ${RED}FAIL${RESET}  app never reached DEVICE_ROUNDTRIP_READY\n"; tail -20 "$APP_LOG"; wait "$APP_PID" 2>/dev/null; exit 1; }
printf "  device paused — inserting the triggering row…\n"

psql_exec "INSERT INTO $TABLE (id, user_id, title, type, metric, unit, occurred_on)
  VALUES ('$ROW_ID', 'roundtrip', 'device roundtrip', 'distance', 7, 'km', now());" >/dev/null \
  || { printf "  ${RED}FAIL${RESET}  row insert failed\n"; exit 1; }

for _ in $(seq 1 180); do
  grep -q 'DEVICE_ROUNDTRIP_PASS' "$APP_LOG" 2>/dev/null && break
  kill -0 "$APP_PID" 2>/dev/null || break
  sleep 1
done
wait "$APP_PID" 2>/dev/null; APP_EXIT=$?
if grep -q 'DEVICE_ROUNDTRIP_PASS' "$APP_LOG" 2>/dev/null && [ "$APP_EXIT" -eq 0 ]; then
  printf "  ${GREEN}PASS${RESET}  device offline→online round-trip: pause → pg insert → resume → delta apply ($ROW_ID)\n"
  exit 0
else
  printf "  ${RED}FAIL${RESET}  round-trip did not complete (log: $APP_LOG)\n"
  grep -a 'DEVICE_ROUNDTRIP' "$APP_LOG" | tail -6
  exit 1
fi
