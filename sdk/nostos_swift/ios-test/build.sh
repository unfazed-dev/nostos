#!/usr/bin/env bash
# NostosSmoke — build + deploy + run the iOS-sim LIVE replication round-trip.
#
# Spawns the shared spine (target/debug/examples/e2e_server) host-side,
# discovers its port from the `NOSTOS_E2E_PORT=` stdout line, injects it into
# the sim app via `SIMCTL_CHILD_NOSTOS_E2E_PORT`, runs the app on the iPhone
# simulator, and gates exit 0 on the `[swift-e2e] SUCCESS` line.
#
# The iPhone simulator shares the host's localhost, so the app reaches the
# spine at ws://127.0.0.1:<port>/sync — no 10.0.2.2 remap needed (that's
# Android-emulator-only). This is the port-plumbing shape Kotlin will copy
# (with `10.0.2.2` instead of `127.0.0.1`).
#
# ponytail: debug build of the Rust staticlib is fine for a test (not a
# shipped binary). The harness links libnostos_swift.a DIRECTLY rather than via
# the xcframework (Tier 1 builds the xcframework separately as a deliverable).
#
# Usage: ./build.sh
# Exit codes: 0 = [swift-e2e] SUCCESS captured; non-zero = see $LOG / $SPINE_LOG.

set -euo pipefail

# Derived, not hardcoded: this script previously pinned absolute paths to one
# machine, so the slice could only ever run there — a silent break for CI (A6
# runs sdk-e2e) and for any other checkout.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"           # sdk/nostos_swift
REPO_ROOT="$(cd "$ROOT/../.." && pwd)"    # repo root
cd "$ROOT"

# xcodegen looks up `$USER` to populate DEVELOPMENT_TEAM / path defaults; some
# sandboxed shells strip it. Fall back to `id -un` (getuid) so generation is
# reproducible from any environment (CI, agent sandbox, interactive terminal).
: "${USER:=$(id -un)}"
: "${LOGNAME:=$(id -un)}"
export USER LOGNAME

# Default to whichever simulator is ACTUALLY booted. `scripts/sdk-e2e.sh` gates
# this slice on "any (Booted) device", so a hardcoded default made the guard and
# the action disagree: guard green, then `simctl install` dies with
# "Unable to lookup in current state: Shutdown" — reported as a swift FAIL when
# nothing was wrong with the SDK. Pin a specific device with NOSTOS_SIM_UDID.
# Filtered to iPhone/iPad deliberately: `simctl list devices booted` also lists
# booted watchOS/tvOS/visionOS sims, and an unfiltered `head -1` could hand a
# Watch UDID to `xcodebuild -sdk iphonesimulator` — reintroducing the very
# guard-vs-action mismatch this block exists to remove (`sdk-e2e.sh:117` greps
# `(Booted)` across all device types).
SIM_UDID="${NOSTOS_SIM_UDID:-$(xcrun simctl list devices booted 2>/dev/null \
  | grep -E '^[[:space:]]+(iPhone|iPad)' \
  | grep -oE '\([0-9A-Fa-f-]{36}\)' | head -1 | tr -d '()')}"
if [ -z "$SIM_UDID" ]; then
  echo "no booted iPhone/iPad simulator — boot one (\`xcrun simctl boot <device>\`) or set NOSTOS_SIM_UDID" >&2
  exit 1
fi
BUNDLE_ID="run.nostos.smoke"
LOG="/tmp/nostos_swift_e2e_launch.log"
SPINE_LOG="/tmp/nostos_swift_e2e_spine.log"
SPINE_PORT=""

# ---- spine lifecycle ----
SPINE_PID=""
cleanup() {
    if [[ -n "$SPINE_PID" ]] && kill -0 "$SPINE_PID" 2>/dev/null; then
        kill "$SPINE_PID" 2>/dev/null || true
        wait "$SPINE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Build the spine if missing (mirrors the Rust E2E template's lazy build).
SPINE_BIN="$REPO_ROOT/target/debug/examples/e2e_server"
if [[ ! -x "$SPINE_BIN" ]]; then
    echo "[harness] 0/7 cargo build -p nostos-infra --examples e2e_server"
    (cd "$REPO_ROOT" && cargo build -p nostos-infra --example e2e_server)
fi
[[ -x "$SPINE_BIN" ]] || { echo "BUILD FAILED: spine binary not at $SPINE_BIN"; exit 1; }

echo "[harness] 1/7 spawn spine (e2e_server)"
rm -f "$SPINE_LOG"
"$SPINE_BIN" > "$SPINE_LOG" 2>&1 &
SPINE_PID=$!

# Discover the port from the NOSTOS_E2E_PORT= stdout line (timeout 10s).
PORT_DEADLINE=$(( $(date +%s) + 10 ))
while [[ $(date +%s) -lt $PORT_DEADLINE ]]; do
    if ! kill -0 "$SPINE_PID" 2>/dev/null; then
        echo "SPINE DIED at startup — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1
    fi
    if grep -q '^NOSTOS_E2E_READY$' "$SPINE_LOG" 2>/dev/null; then
        SPINE_PORT=$(grep '^NOSTOS_E2E_PORT=' "$SPINE_LOG" | tail -1 | cut -d= -f2 | tr -d '[:space:]')
        [[ -n "$SPINE_PORT" ]] && break
    fi
    sleep 0.2
done
[[ -n "$SPINE_PORT" ]] || { echo "SPINE never announced a port — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1; }
echo "[harness] spine on port $SPINE_PORT (pid $SPINE_PID)"

echo "[harness] 2/7 cargo build --target aarch64-apple-ios-sim"
cargo build --target aarch64-apple-ios-sim

echo "[harness] 3/7 xcodegen generate"
cd ios-test
xcodegen generate 2>&1 | tail -10

echo "[harness] 4/7 xcodebuild -sdk iphonesimulator -destination id=$SIM_UDID"
xcodebuild \
  -project NostosSmoke.xcodeproj \
  -scheme NostosSmoke \
  -sdk iphonesimulator \
  -destination "id=$SIM_UDID" \
  -configuration Debug \
  -derivedDataPath build \
  CODE_SIGN_IDENTITY="" \
  CODE_SIGNING_REQUIRED=NO \
  CODE_SIGNING_ALLOWED=NO \
  build > /tmp/nostos_swift_e2e_xcodebuild.log 2>&1 || { echo "BUILD FAILED — see /tmp/nostos_swift_e2e_xcodebuild.log"; tail -40 /tmp/nostos_swift_e2e_xcodebuild.log; exit 1; }

APP_PATH="build/Build/Products/Debug-iphonesimulator/NostosSmoke.app"
echo "[harness] built: $APP_PATH"

echo "[harness] 5/7 simctl uninstall (clean state)"
xcrun simctl uninstall "$SIM_UDID" "$BUNDLE_ID" 2>/dev/null || true

echo "[harness] 6/7 simctl install"
xcrun simctl install "$SIM_UDID" "$APP_PATH"

echo "[harness] 7/7 simctl launch --console-pty (NOSTOS_E2E_PORT=$SPINE_PORT)"
# --console-pty pipes the app's stdout/stderr straight to this terminal.
# SIMCTL_CHILD_<NAME> is simctl's documented prefix for injecting env vars
# into the launched app's ProcessInfo.processInfo.environment.
rm -f "$LOG"
SIMCTL_CHILD_NOSTOS_E2E_PORT="$SPINE_PORT" \
  xcrun simctl launch --console-pty "$SIM_UDID" "$BUNDLE_ID" > "$LOG" 2>&1 || true

echo "[harness] ----- captured sim stdout (raw) -----"
cat "$LOG" || echo "(no output)"
echo "[harness] ----- end -----"

if grep -q '\[swift-e2e\] SUCCESS' "$LOG"; then
    echo "[harness] VERDICT: live round-trip ran on iPhone sim (PUSH + ECHO both directions)"
    exit 0
else
    echo "[harness] VERDICT: [swift-e2e] SUCCESS line NOT captured — see $LOG (spine: $SPINE_LOG)"
    exit 2
fi
