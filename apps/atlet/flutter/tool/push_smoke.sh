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
#   (nostos_push_sent_total ≥ 1); device side via the integration test's
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
  docker exec "$PG_CONTAINER" psql -U nostos -d nostos -qc \
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
    FLUTTER_TEST_EXTRA_ARGS=""
    # FCM data messages reach the test's onMessage only while the app is
    # FOREGROUNDED — anything else holding the screen (the launcher, another
    # app) silently starves the smoke. .MainActivity is singleTop, so this is
    # a no-restart focus restore for an already-running test.
    keep_foreground() {
      case "$("$ADB" shell dumpsys window 2>/dev/null | grep -m1 mCurrentFocus)" in
        *internal.atlet.atlet*) ;;
        *) "$ADB" shell am start -n internal.atlet.atlet/.MainActivity >/dev/null 2>&1 || true ;;
      esac
    }
    # A failed `flutter test` UNINSTALLS the app when it exits, so a grant
    # made earlier in the script has no package to attach to on the next run
    # — the test then hangs in requestPermission()'s untapped dialog. Install
    # the previously built APK first (if any) so the POST_NOTIFICATIONS grant
    # lands; flutter test reinstalls the same APK over it, preserving both.
    # First-ever run on a clean box: no APK yet — tap Allow once, or grant
    # manually, then it persists.
    prep_device() {
      local apk="$APP_DIR/build/app/outputs/flutter-apk/app-debug.apk"
      [ -f "$apk" ] && "$ADB" install -t "$apk" >/dev/null 2>&1 || true
      "$ADB" shell pm grant internal.atlet.atlet android.permission.POST_NOTIFICATIONS >/dev/null 2>&1 || true
    }
    ;;
  ios)
    [ -f "$APP_DIR/ios/Runner/GoogleService-Info.plist" ] \
      || skip "ios/Runner/GoogleService-Info.plist absent — see tool/PUSH_SMOKE.md"
    [ -n "${PUSH_SMOKE_DEVICE_ID:-}" ]   || skip "ios mode needs PUSH_SMOKE_DEVICE_ID=<physical device id> (flutter devices); the simulator cannot receive real FCM"
    [ -n "${NOSTOS_SYNC_URL:-}" ]         || skip "ios mode needs NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:$PORT/sync (a device cannot use host loopback)"
    DEVICE_ID="$PUSH_SMOKE_DEVICE_ID"
    BIND=0.0.0.0                          # reachable from the LAN
    SYNC_URL="$NOSTOS_SYNC_URL"
    # Android-only helpers, no-op'd for iOS: no adb on the leg — the operator
    # keeps the screen unlocked/awake and taps Allow on the first
    # notification-permission prompt (persistent afterwards).
    prep_device() { :; }
    keep_foreground() { :; }
    # Keep-foreground/prep are Android-only (no-op'd above). Wireless tethers
    # can trip the tool's app-launch check; the reliable path is a USB cable —
    # PUSH_SMOKE_PUBLISH_PORT exists only for a future flutter-drive variant.
    FLUTTER_TEST_EXTRA_ARGS="${PUSH_SMOKE_PUBLISH_PORT:+--publish-port $PUSH_SMOKE_PUBLISH_PORT}"
    ;;
  *) skip "PUSH_SMOKE_DEVICE must be 'android' or 'ios' (got '$DEVICE_MODE')";;
esac

# ---- 3. local docker PG (repo's own e2e stack, port 5433) -----------------
if ! docker exec "$PG_CONTAINER" pg_isready -U nostos -d nostos >/dev/null 2>&1; then
  command -v docker >/dev/null 2>&1 || skip "docker PG not reachable and docker absent"
  printf "  starting docker PG (docker/docker-compose.yml)…\n"
  docker compose -f "$ROOT_DIR/docker/docker-compose.yml" up -d >/dev/null 2>&1 \
    || skip "failed to start the docker PG"
fi
for _ in $(seq 1 30); do
  docker exec "$PG_CONTAINER" pg_isready -U nostos -d nostos >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG_CONTAINER" pg_isready -U nostos -d nostos >/dev/null 2>&1 \
  || skip "docker PG never became ready"

psql_exec() { docker exec "$PG_CONTAINER" psql -U nostos -d nostos -qAt -c "$1"; }

# Throwaway schema: atlet's REAL table shapes so the actual app (Shop tab,
# cart, checkout) runs unmodified against the smoke server. REPLICA
# IDENTITY FULL (ADR-0025), dedicated publication + slot. `products` is
# seeded (global catalog); user-owned tables carry user_id.
psql_exec "DROP PUBLICATION IF EXISTS $PUB;" >/dev/null
for t in $TABLE products cart_items orders; do
  psql_exec "DROP TABLE IF EXISTS $t;" >/dev/null
done
psql_exec "CREATE TABLE $TABLE (
  id text PRIMARY KEY,
  user_id text NOT NULL,
  title text NOT NULL,
  type text NOT NULL,
  metric int NOT NULL,
  unit text NOT NULL,
  occurred_on date NOT NULL);" >/dev/null
psql_exec "CREATE TABLE products (
  id text PRIMARY KEY,
  name text NOT NULL,
  category text NOT NULL,
  price_cents int NOT NULL,
  rating real,
  plant_based int NOT NULL DEFAULT 0,
  image_url text);" >/dev/null
psql_exec "CREATE TABLE cart_items (
  id text PRIMARY KEY,
  user_id text NOT NULL,
  product_id text NOT NULL,
  qty int NOT NULL,
  added_at timestamptz NOT NULL);" >/dev/null
psql_exec "CREATE TABLE orders (
  id text PRIMARY KEY,
  user_id text NOT NULL,
  status text NOT NULL,
  subtotal_cents int NOT NULL,
  tax_cents int NOT NULL,
  shipping_cents int NOT NULL,
  total_cents int NOT NULL,
  payment_ref text,
  items_json text,
  created_at timestamptz NOT NULL);" >/dev/null
for t in $TABLE products cart_items orders; do
  psql_exec "ALTER TABLE $t REPLICA IDENTITY FULL;" >/dev/null
done
psql_exec "INSERT INTO products (id,name,category,price_cents,rating,plant_based) VALUES
  ('p1','Trail Runner','shoes',12900,4.6,0),
  ('p2','Energy Bar','nutrition',250,4.2,1),
  ('p3','Water Bottle','gear',1500,4.4,0),
  ('p4','Training Tee','apparel',3500,4.0,1),
  ('p5','Speed Laces','gear',900,3.9,0),
  ('p6','Recovery Mix','nutrition',2200,4.7,1);" >/dev/null
psql_exec "CREATE PUBLICATION $PUB FOR TABLE $TABLE, products, cart_items, orders;" >/dev/null
psql_exec "SELECT pg_drop_replication_slot('$SLOT') FROM pg_replication_slots WHERE slot_name='$SLOT';" >/dev/null
# The token registry persists in this docker volume across runs, and every
# flutter-test install mints a FRESH FCM token (app data wiped per install)
# — stale rows otherwise accumulate and eat send quota (each is now pruned
# on first UNREGISTERED, but start each run clean regardless).
psql_exec "TRUNCATE nostos_push_tokens;" >/dev/null 2>&1 || true

# ---- 4. nostos-server (real PG replicator + FCM rail + doorbell table) -----
printf "  starting nostos-server (cargo run, log: $SERVER_LOG)…\n"
NOSTOS_BIND="$BIND:$PORT" \
NOSTOS_REPLICATOR=pg \
NOSTOS_PG_URL="postgres://nostos:nostos@localhost:5433/nostos" \
NOSTOS_PG_PUBLICATION="$PUB" \
NOSTOS_PG_SLOT="$SLOT" \
NOSTOS_SYNC_AUTH=supabase-jwt \
NOSTOS_SUPABASE_JWT_SECRET="$NOSTOS_SUPABASE_JWT_SECRET" \
# NOSTOS_TENANT_COLUMN=user_id is REQUIRED here (root-caused 2026-08-27):
# the push doorbell's fully-offline fallback (fanout.rs — the killed-app
# case, which pauseSync models) targets tenants by reading the ROW's tenant
# column; with the column unset (empty opt-out) that path has no tenant to
# read and enqueues NOTHING — leg 1 dies with nostos_push_sent_total 0. And
# the column must never be merely UNSET: the clap default is "org_id", a
# column no smoke table has — every snapshot AND live predicate 42703s /
# never matches (no frames, no doorbells). The original leg-2 failure
# (products starving) was this same clause 42703ing the global catalog's
# snapshot — fixed SERVER-side 2026-08-27: PgSnapshotter now skips the
# tenant clause for tables lacking the column (deliberately-global
# tables), so =user_id and the products catalog coexist.
NOSTOS_TENANT_COLUMN=user_id \
NOSTOS_WRITE_TABLES="$TABLE,cart_items,orders" \
NOSTOS_PUSH_TABLES="$TABLE;orders:action:order_status:Atlet order update:Your order {id} is {status}" \
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

sent_before="$(curl -s "http://127.0.0.1:$PORT/metrics" | awk '/^nostos_push_sent_total/ {print $2; exit}')"
sent_before="${sent_before:-0}"

# ---- 5. device leg: register token, go offline, listen -------------------
printf "  running atlet push smoke on %s [%s] (log: $APP_LOG)…\n" "$DEVICE_MODE" "$DEVICE_ID"
prep_device
( cd "$APP_DIR" && mkdir -p build/ios/SourcePackages build/macos/SourcePackages && flutter pub get >/dev/null 2>&1 && \
  flutter test integration_test/push_smoke_test.dart -d "$DEVICE_ID" \
    $FLUTTER_TEST_EXTRA_ARGS \
    --dart-define=SUPABASE_URL="$SUPABASE_URL" \
    --dart-define=SUPABASE_ANON_KEY="$SUPABASE_ANON_KEY" \
    --dart-define=NOSTOS_SYNC_URL="$SYNC_URL" \
    --dart-define=ATLET_PUSH_PILOT=true ) >"$APP_LOG" 2>&1 &
APP_PID=$!

# READY is printed only after: sign-in → token registered → first sync →
# pauseSync (offline). The user id rides the earlier PUSH_SMOKE_USER line.
USER_ID=""
for _ in $(seq 1 600); do
  [ -n "$USER_ID" ] && grep -q 'PUSH_SMOKE_READY' "$APP_LOG" 2>/dev/null && break
  USER_ID="$(grep -oE 'PUSH_SMOKE_USER=[0-9a-f-]+' "$APP_LOG" 2>/dev/null | head -1 | cut -d= -f2)"
  kill -0 "$APP_PID" 2>/dev/null || break
  keep_foreground
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
  sent_after="$(curl -s "http://127.0.0.1:$PORT/metrics" | awk '/^nostos_push_sent_total/ {print $2; exit}')"
  sent_after="${sent_after:-0}"
  [ "$sent_after" -gt "$sent_before" ] 2>/dev/null && break
  sleep 1
done
if [ "$sent_after" -le "$sent_before" ] 2>/dev/null; then
  printf "  ${RED}FAIL${RESET}  nostos_push_sent_total did not move ($sent_before → $sent_after)\n"
  curl -s "http://127.0.0.1:$PORT/metrics" | grep '^nostos_push_' || true
  tail -10 "$SERVER_LOG"
  exit 1
fi
printf "  server: nostos_push_sent_total %s → %s\n" "$sent_before" "$sent_after"

# ---- 8. assert the device received the doorbell ---------------------------
wait "$APP_PID"
APP_STATUS=$?
if [ $APP_STATUS -eq 0 ] && grep -q 'PUSH_SMOKE_RECEIVED' "$APP_LOG"; then
  grep -o 'PUSH_SMOKE_RECEIVED.*' "$APP_LOG" | head -1 | sed 's/^/  device: /'
  printf "  ${GREEN}PASS${RESET}  real-rail FCM doorbell: PG row → nostos-server → FCM → device\n"
else
  printf "  ${RED}FAIL${RESET}  device leg (exit=$APP_STATUS, log: $APP_LOG)\n"
  grep -E 'PUSH_SMOKE_(TOKEN|MESSAGE|TIMEOUT)' "$APP_LOG" | head -5 || true
  exit 1
fi

# ---- 9. ecommerce order-lifecycle leg (action pushes, ADR-0037 §2) -------
# The REFERENCE MODEL for other SDKs: the real app UI (Shop → cart →
# checkout → Pay) writes the order through nostos; this script plays the
# vendor advancing paid → shipped → delivered. Each push-table commit to an
# offline account sends an ACTION push via the orders:action template —
# iOS renders the order_status category's buttons from the apns override;
# Android's app renders locally from the data payload. The device re-arms itself between pushes (the test re-pauses on
# every arrival), so each UPDATE needs the device offline again — the sleep
# below covers the 2s coalescer + socket teardown.
ORDER_LOG=/tmp/atlet-push-smoke-order.log
printf "  running atlet order-lifecycle leg (log: $ORDER_LOG)…\n"
prep_device
( cd "$APP_DIR" && mkdir -p build/ios/SourcePackages build/macos/SourcePackages && flutter pub get >/dev/null 2>&1 && \
  flutter test integration_test/order_push_test.dart -d "$DEVICE_ID" \
    $FLUTTER_TEST_EXTRA_ARGS \
    --dart-define=SUPABASE_URL="$SUPABASE_URL" \
    --dart-define=SUPABASE_ANON_KEY="$SUPABASE_ANON_KEY" \
    --dart-define=NOSTOS_SYNC_URL="$SYNC_URL" \
    --dart-define=ATLET_PUSH_PILOT=true ) >"$ORDER_LOG" 2>&1 &
ORDER_PID=$!

ORDER_ID=""
for _ in $(seq 1 600); do
  [ -n "$ORDER_ID" ] && grep -q 'PUSH_SMOKE_ORDER_READY' "$ORDER_LOG" 2>/dev/null && break
  ORDER_ID="$(grep -oE 'PUSH_SMOKE_ORDER=[0-9a-f-]+' "$ORDER_LOG" 2>/dev/null | head -1 | cut -d= -f2)"
  kill -0 "$ORDER_PID" 2>/dev/null || break
  keep_foreground
  sleep 1
done
if [ -z "$ORDER_ID" ] || ! grep -q 'PUSH_SMOKE_ORDER_READY' "$ORDER_LOG" 2>/dev/null; then
  printf "  ${RED}FAIL${RESET}  order leg never reached READY (log: $ORDER_LOG)\n"
  tail -20 "$ORDER_LOG"
  wait "$ORDER_PID" 2>/dev/null
  exit 1
fi
printf "  device checked out: order=%s — playing the vendor…\n" "${ORDER_ID:0:8}"

# The vendor can only advance an order that exists in PG.
for _ in $(seq 1 30); do
  [ "$(psql_exec "SELECT count(*) FROM orders WHERE id='$ORDER_ID';")" = "1" ] && break
  sleep 1
done
[ "$(psql_exec "SELECT count(*) FROM orders WHERE id='$ORDER_ID';")" = "1" ] \
  || { printf "  ${RED}FAIL${RESET}  order $ORDER_ID never landed in PG\n"; exit 1; }

# Advance one lifecycle step and assert BOTH sides: the rail fired
# (nostos_push_sent_total) and the device received it (PUSH_SMOKE_PUSH marker).
order_step() {  # $1 = new status
  psql_exec "UPDATE orders SET status='$1' WHERE id='$ORDER_ID';" >/dev/null
  for _ in $(seq 1 90); do
    grep -q "PUSH_SMOKE_PUSH=$1" "$ORDER_LOG" 2>/dev/null && return 0
    keep_foreground
    sleep 1
  done
  printf "  ${RED}FAIL${RESET}  '$1' push not received within 90s (log: $ORDER_LOG)\n"
  curl -s "http://127.0.0.1:$PORT/metrics" | grep '^nostos_push_' || true
  tail -15 "$ORDER_LOG"
  exit 1
}

order_step shipped
sleep 4   # coalescer window + reconnect teardown — device must be offline again
order_step delivered

wait "$ORDER_PID"
ORDER_STATUS=$?
if [ $ORDER_STATUS -eq 0 ] && grep -q 'PUSH_SMOKE_ORDER_PASS' "$ORDER_LOG"; then
  printf "  ${GREEN}PASS${RESET}  order lifecycle: checkout → shipped → delivered pushes\n"
  exit 0
fi
printf "  ${RED}FAIL${RESET}  order leg (exit=$ORDER_STATUS, log: $ORDER_LOG)\n"
grep -E 'PUSH_SMOKE_ORDER_(MESSAGE|PASS)' "$ORDER_LOG" | head -5 || true
exit 1
