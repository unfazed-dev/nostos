#!/usr/bin/env bash
# apps/atlet/flutter/tool/push_smoke.sh — ADR-0037 real-rail FCM push smoke
# for the Atlet pilot. Mirrors the NOSTOS_E2E_PG / NOSTOS_E2E_FCM self-skip
# convention: exits 0 with a SKIP line whenever an operator-owned input is
# absent, so it is honest on a secrets-less box.
#
# What it proves, end to end on the REAL FCM rail:
#   atlet (device) signs in → registers its FCM token (POST /push-tokens) →
#   goes offline (pauseSync — doorbells target offline accounts only) →
#   harness inserts a `sessions` row into the local docker PG → nostos-server
#   replicates it → doorbell → FCM HTTP v1 send → device receives the
#   {table,lsn} data message. Server side asserted via /metrics
#   (cairn_push_sent_total ≥ 1); device side via the integration test's
#   PUSH_SMOKE_RECEIVED marker.
#
# Automated device path: a booted ANDROID EMULATOR (FCM fully works there,
# including data-message delivery to the foregrounded app). iOS is a
# physical-device-only leg — see PUSH_SMOKE.md for why the iOS simulator
# cannot receive real FCM pushes.
#
# Usage (from anywhere):
#   apps/atlet/flutter/tool/push_smoke.sh                       # android leg
#   PUSH_SMOKE_DEVICE=ios PUSH_SMOKE_DEVICE_ID=<id> \
#     NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:8080/sync \
#     apps/atlet/flutter/tool/push_smoke.sh                     # iOS device leg
#
# Operator-owned env (NEVER committed — mirrors apps/atlet/services/.env):
#   NOSTOS_FCM_CREDENTIALS_JSON  FCM service-account JSON (raw) or a file path
#   SUPABASE_URL                atlet Supabase project URL (app sign-in)
#   SUPABASE_ANON_KEY           its publishable/anon key
#   NOSTOS_SUPABASE_JWT_SECRET   the SAME project's JWT secret (verifies the
#                               app's tokens on the smoke nostos-server)
# Optional: NOSTOS_SYNC_URL (device→server URL override), PUSH_SMOKE_SLOT,
#   PUSH_SMOKE_PUB, PUSH_SMOKE_TABLE.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$SCRIPT_DIR/.."
ROOT_DIR="$SCRIPT_DIR/../../.."

GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; RED=$'\033[0;31m'
BOLD=$'\033[1m';   RESET=$'\033[0m'

SLOT="${PUSH_SMOKE_SLOT:-atlet_push_smoke_slot}"
PUB="${PUSH_SMOKE_PUB:-atlet_push_smoke_pub}"
TABLE="${PUSH_SMOKE_TABLE:-sessions}"
PORT=8080
SERVER_LOG=/tmp/atlet-push-smoke-server.log
APP_LOG=/tmp/atlet-push-smoke-app.log
PG_CONTAINER=nostos-postgres
SERVER_PID=""

skip() { printf "  ${YELLOW}SKIP${RESET}  %s\n" "$1"; exit 0; }

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
  fi
  # Best-effort slot drop — a leaked slot only pins WAL in the throwaway PG.
  docker exec "$PG_CONTAINER" psql -U cairn -d cairn -qc \
    "SELECT pg_drop_replication_slot('$SLOT') FROM pg_replication_slots WHERE slot_name='$SLOT';" \
    >/dev/null 2>&1
}
trap cleanup EXIT

# ---- 1. operator-owned inputs (self-skip, NOSTOS_E2E_PG convention) --------
[ -n "${NOSTOS_FCM_CREDENTIALS_JSON:-}" ] || skip "NOSTOS_FCM_CREDENTIALS_JSON not set (FCM service-account JSON, raw or a file path) — see tool/PUSH_SMOKE.md"
[ -n "${SUPABASE_URL:-}" ]               || skip "SUPABASE_URL not set — see tool/PUSH_SMOKE.md"
[ -n "${SUPABASE_ANON_KEY:-}" ]          || skip "SUPABASE_ANON_KEY not set — see tool/PUSH_SMOKE.md"
[ -n "${NOSTOS_SUPABASE_JWT_SECRET:-}" ]  || skip "NOSTOS_SUPABASE_JWT_SECRET not set — see tool/PUSH_SMOKE.md"
command -v flutter >/dev/null 2>&1       || skip "flutter not on PATH"
command -v cargo  >/dev/null 2>&1        || skip "cargo not on PATH"

# FCM creds: file path or raw JSON (the server parses raw JSON).
FCM_JSON="$NOSTOS_FCM_CREDENTIALS_JSON"
if [ -f "$FCM_JSON" ]; then FCM_JSON="$(cat "$FCM_JSON")"; fi
case "$FCM_JSON" in *project_id*) ;; *) skip "NOSTOS_FCM_CREDENTIALS_JSON is neither a readable file nor service-account JSON";; esac

# ---- 2. device leg --------------------------------------------------------
DEVICE_MODE="${PUSH_SMOKE_DEVICE:-android}"
case "$DEVICE_MODE" in
  android)
    [ -f "$APP_DIR/android/app/google-services.json" ] \
      || skip "android/app/google-services.json absent (drop the Firebase config there — see tool/PUSH_SMOKE.md)"
    ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
    [ -x "$ADB" ] || skip "adb not found (set ANDROID_HOME or install Android SDK)"
    # Here-string, never a pipe: see scripts/sdk-e2e.sh's SIGPIPE note.
    if grep -q '^emulator-[0-9]*[[:space:]]*device' <<< "$("$ADB" devices 2>/dev/null || true)"; then
      DEVICE_ID="$(grep '^emulator-[0-9]*[[:space:]]*device' <<< "$("$ADB" devices 2>/dev/null)" | head -1 | cut -f1)"
    else
      skip "no booted Android emulator (flutter emulators / avdmanager, keep the screen unlocked)"
    fi
    BIND=127.0.0.1                        # 10.0.2.2 (emulator) → host loopback
    SYNC_URL="${NOSTOS_SYNC_URL:-ws://10.0.2.2:$PORT/sync}"
    ;;
  ios)
    [ -f "$APP_DIR/ios/Runner/GoogleService-Info.plist" ] \
      || skip "ios/Runner/GoogleService-Info.plist absent — see tool/PUSH_SMOKE.md"
    [ -n "${PUSH_SMOKE_DEVICE_ID:-}" ]   || skip "ios mode needs PUSH_SMOKE_DEVICE_ID=<physical device id> (flutter devices); the simulator cannot receive real FCM"
    [ -n "${NOSTOS_SYNC_URL:-}" ]         || skip "ios mode needs NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:$PORT/sync (a device cannot use host loopback)"
    DEVICE_ID="$PUSH_SMOKE_DEVICE_ID"
    BIND=0.0.0.0                          # reachable from the LAN
    SYNC_URL="$NOSTOS_SYNC_URL"
    ;;
  *) skip "PUSH_SMOKE_DEVICE must be 'android' or 'ios' (got '$DEVICE_MODE')";;
esac

# ---- 3. local docker PG (repo's own e2e stack, port 5433) -----------------
if ! docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1; then
  command -v docker >/dev/null 2>&1 || skip "docker PG not reachable and docker absent"
  printf "  starting docker PG (docker/docker-compose.yml)…\n"
  docker compose -f "$ROOT_DIR/docker/docker-compose.yml" up -d >/dev/null 2>&1 \
    || skip "failed to start the docker PG"
fi
for _ in $(seq 1 30); do
  docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG_CONTAINER" pg_isready -U cairn -d cairn >/dev/null 2>&1 \
  || skip "docker PG never became ready"

psql_exec() { docker exec "$PG_CONTAINER" psql -U cairn -d cairn -qAt -c "$1"; }

# Throwaway schema: atlet's sessions shape (tenant column user_id), REPLICA
# IDENTITY FULL (ADR-0025), dedicated publication + slot.
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

# ---- 4. nostos-server (real PG replicator + FCM rail + doorbell table) -----
printf "  starting nostos-server (cargo run, log: $SERVER_LOG)…\n"
NOSTOS_BIND="$BIND:$PORT" \
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL="postgres://cairn:cairn@localhost:5433/cairn" \
NOSTOS_PG_PUBLICATION="$PUB" \
NOSTOS_PG_SLOT="$SLOT" \
NOSTOS_SYNC_AUTH=supabase-jwt \
NOSTOS_SUPABASE_JWT_SECRET="$NOSTOS_SUPABASE_JWT_SECRET" \
NOSTOS_TENANT_COLUMN=user_id \
NOSTOS_WRITE_TABLES="$TABLE" \
NOSTOS_PUSH_TABLES="$TABLE" \
NOSTOS_FCM_CREDENTIALS_JSON="$FCM_JSON" \
  cargo run -q -p nostos-server >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 90); do
  # curl only here; failures are the normal "not up yet".
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    printf "  ${RED}FAIL${RESET}  nostos-server exited early (log: $SERVER_LOG)\n"
    tail -5 "$SERVER_LOG"
    exit 1
  fi
  sleep 1
done
curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 \
  || { printf "  ${RED}FAIL${RESET}  nostos-server never became healthy\n"; exit 1; }

sent_before="$(curl -s "http://127.0.0.1:$PORT/metrics" | awk '/^cairn_push_sent_total/ {print $2; exit}')"
sent_before="${sent_before:-0}"

# ---- 5. device leg: register token, go offline, listen -------------------
printf "  running atlet push smoke on %s [%s] (log: $APP_LOG)…\n" "$DEVICE_MODE" "$DEVICE_ID"
( cd "$APP_DIR" && flutter pub get >/dev/null 2>&1 && \
  flutter test integration_test/push_smoke_test.dart -d "$DEVICE_ID" \
    --dart-define=SUPABASE_URL="$SUPABASE_URL" \
    --dart-define=SUPABASE_ANON_KEY="$SUPABASE_ANON_KEY" \
    --dart-define=NOSTOS_SYNC_URL="$SYNC_URL" \
    --dart-define=ATLET_PUSH_PILOT=1 ) >"$APP_LOG" 2>&1 &
APP_PID=$!

# READY is printed only after: sign-in → token registered → first sync →
# pauseSync (offline). The user id rides the earlier PUSH_SMOKE_USER line.
USER_ID=""
for _ in $(seq 1 600); do
  [ -n "$USER_ID" ] && grep -q 'PUSH_SMOKE_READY' "$APP_LOG" 2>/dev/null && break
  USER_ID="$(grep -oE 'PUSH_SMOKE_USER=[0-9a-f-]+' "$APP_LOG" 2>/dev/null | head -1 | cut -d= -f2)"
  kill -0 "$APP_PID" 2>/dev/null || break
  sleep 1
done
if [ -z "$USER_ID" ] || ! grep -q 'PUSH_SMOKE_READY' "$APP_LOG" 2>/dev/null; then
  printf "  ${RED}FAIL${RESET}  app never reached PUSH_SMOKE_READY (log: $APP_LOG)\n"
  tail -20 "$APP_LOG"
  wait "$APP_PID" 2>/dev/null
  exit 1
fi
printf "  device ready: user=%s — inserting the triggering row…\n" "$USER_ID"

# ---- 6. trigger: server-side row change (the push-worthy commit) ----------
ROW_ID="push-smoke-$(date +%s)"
psql_exec "INSERT INTO $TABLE (id, user_id, title, type, metric, unit, occurred_on)
  VALUES ('$ROW_ID', '$USER_ID', 'push smoke', 'distance', 5, 'km', now());" >/dev/null \
  || { printf "  ${RED}FAIL${RESET}  row insert failed\n"; exit 1; }

# ---- 7. assert the rail fired (server metrics) ----------------------------
sent_after=""
for _ in $(seq 1 90); do
  sent_after="$(curl -s "http://127.0.0.1:$PORT/metrics" | awk '/^cairn_push_sent_total/ {print $2; exit}')"
  sent_after="${sent_after:-0}"
  [ "$sent_after" -gt "$sent_before" ] 2>/dev/null && break
  sleep 1
done
if [ "$sent_after" -le "$sent_before" ] 2>/dev/null; then
  printf "  ${RED}FAIL${RESET}  cairn_push_sent_total did not move ($sent_before → $sent_after)\n"
  curl -s "http://127.0.0.1:$PORT/metrics" | grep '^cairn_push_' || true
  tail -10 "$SERVER_LOG"
  exit 1
fi
printf "  server: cairn_push_sent_total %s → %s\n" "$sent_before" "$sent_after"

# ---- 8. assert the device received the doorbell ---------------------------
wait "$APP_PID"
APP_STATUS=$?
if [ $APP_STATUS -eq 0 ] && grep -q 'PUSH_SMOKE_RECEIVED' "$APP_LOG"; then
  grep -o 'PUSH_SMOKE_RECEIVED.*' "$APP_LOG" | head -1 | sed 's/^/  device: /'
  printf "  ${GREEN}PASS${RESET}  real-rail FCM doorbell: PG row → nostos-server → FCM → device\n"
  exit 0
fi
printf "  ${RED}FAIL${RESET}  device leg (exit=$APP_STATUS, log: $APP_LOG)\n"
grep -E 'PUSH_SMOKE_(TOKEN|MESSAGE|TIMEOUT)' "$APP_LOG" | head -5 || true
exit 1
