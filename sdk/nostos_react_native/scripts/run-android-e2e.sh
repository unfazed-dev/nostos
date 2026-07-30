#!/usr/bin/env bash
# run-android-e2e.sh — React-Native Android TurboModule live-replication E2E
# orchestrator (the Wave-B SHOULD tier).
#
# Adapted from sdk/nostos_kotlin/scripts/run-live-e2e.sh. Wires the on-device
# `live_connect_push_echo_roundTrip` instrumented test (driving NostosTurboModule)
# to a host-side spine (`target/debug/examples/e2e_server`) by:
#   0. Ensuring the nostos_api34 emulator (emulator-5556) is booted.
#   1. Cross-compiling libnostos_kotlin.so + regenerating UniFFI Kotlin sources +
#      stripping + copying them into android/src/main/{jniLibs,kotlin-uniffi}/
#      (delegates to scripts/build-android.sh).
#   2. Spawning the spine host-side, capturing its NOSTOS_E2E_PORT.
#   3. Running `./gradlew connectedDebugAndroidTest -PnostosPort=<port>` —
#      gradle threads the port into the test apk via
#      testInstrumentationRunnerArguments["nostosPort"], and the test reads it
#      back via InstrumentationRegistry.getArguments(). The emulator reaches
#      the host at ws://10.0.2.2:<port>/sync (host-loopback alias).
#   4. Killing the spine on EXIT/INT/TERM.
#   5. Capturing [rn-e2e] PUSH_OK + ECHO_OK from logcat + failures=0 from the
#      test XML — the parity bar with nostos_kotlin's run-live-e2e.sh.
#
# Usage: ./scripts/run-android-e2e.sh
# Exit codes: 0 = [rn-e2e] PUSH_OK + ECHO_OK captured + test XML failures=0;
#             non-zero = see $HARNESS_LOG / $SPINE_LOG / $GRADLE_LOG.
set -euo pipefail

NOSTOS_RN="/Volumes/developer_ssd/Developer/nostos/sdk/nostos_react_native"
REPO_ROOT="/Volumes/developer_ssd/Developer/nostos"
cd "$NOSTOS_RN"

AVD="${NOSTOS_AVD:-nostos_api34}"
EMU_SERIAL="${NOSTOS_EMU_SERIAL:-emulator-5556}"
ADB="${ADB:-$HOME/Library/Android/sdk/platform-tools/adb}"
EMU_BIN="${EMU_BIN:-$HOME/Library/Android/sdk/emulator/emulator}"

SPINE_BIN="$REPO_ROOT/target/debug/examples/e2e_server"
SPINE_LOG="/tmp/nostos_rn_e2e_spine.log"
GRADLE_LOG="/tmp/nostos_rn_e2e_gradle.log"
EMU_BOOT_LOG="/tmp/nostos_rn_emu_boot.log"
HARNESS_LOG="/tmp/nostos_rn_e2e_harness.log"
SPINE_PID=""

cleanup() {
    if [[ -n "$SPINE_PID" ]] && kill -0 "$SPINE_PID" 2>/dev/null; then
        kill "$SPINE_PID" 2>/dev/null || true
        wait "$SPINE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# -------- 0. ensure emulator -----------------------------------------------
echo "[rn-harness] 0/5 ensure emulator ($AVD @ $EMU_SERIAL)"
if ! "$ADB" devices | grep -q "^$EMU_SERIAL\b.*device$"; then
    echo "[rn-harness]   booting $AVD headless (port $(echo "$EMU_SERIAL" | sed 's/[^0-9]//g'))"
    "$EMU_BIN" -avd "$AVD" -no-window -no-audio -no-boot-anim -gpu swiftshader_indirect \
        -port "$(echo "$EMU_SERIAL" | sed 's/[^0-9]//g')" > "$EMU_BOOT_LOG" 2>&1 &
    "$ADB" -s "$EMU_SERIAL" wait-for-device 2>/dev/null || true
    BOOT_DEADLINE=$(( $(date +%s) + 120 ))
    while [[ $(date +%s) -lt $BOOT_DEADLINE ]]; do
        bc=$("$ADB" -s "$EMU_SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r\n ')
        [[ "$bc" == "1" ]] && break
        sleep 2
    done
    bc=$("$ADB" -s "$EMU_SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r\n ')
    [[ "$bc" == "1" ]] || { echo "[rn-harness] EMULATOR BOOT FAILED — see $EMU_BOOT_LOG"; exit 1; }
    echo "[rn-harness]   $EMU_SERIAL booted (sys.boot_completed=1)"
else
    echo "[rn-harness]   $EMU_SERIAL already online"
fi

# -------- 1. build .so + UniFFI Kotlin sources -----------------------------
echo "[rn-harness] 1/5 build .so + UniFFI Kotlin sources (scripts/build-android.sh)"
"$NOSTOS_RN/scripts/build-android.sh"

# -------- 2. spawn spine + discover port -----------------------------------
# Build the spine if missing (mirrors the Kotlin + Swift harnesses' lazy build).
if [[ ! -x "$SPINE_BIN" ]]; then
    echo "[rn-harness]   building spine (cargo build -p nostos-infra --example e2e_server)"
    (cd "$REPO_ROOT" && cargo build -p nostos-infra --example e2e_server)
fi
[[ -x "$SPINE_BIN" ]] || { echo "[rn-harness] SPINE BUILD FAILED: $SPINE_BIN missing"; exit 1; }

echo "[rn-harness] 2/5 spawn spine (e2e_server)"
rm -f "$SPINE_LOG"
"$SPINE_BIN" > "$SPINE_LOG" 2>&1 &
SPINE_PID=$!

# Discover the port from the NOSTOS_E2E_PORT= stdout line (timeout 10s).
SPINE_PORT=""
PORT_DEADLINE=$(( $(date +%s) + 10 ))
while [[ $(date +%s) -lt $PORT_DEADLINE ]]; do
    if ! kill -0 "$SPINE_PID" 2>/dev/null; then
        echo "[rn-harness] SPINE DIED at startup — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1
    fi
    if grep -q '^NOSTOS_E2E_READY$' "$SPINE_LOG" 2>/dev/null; then
        SPINE_PORT=$(grep '^NOSTOS_E2E_PORT=' "$SPINE_LOG" | tail -1 | cut -d= -f2 | tr -d '[:space:]')
        [[ -n "$SPINE_PORT" ]] && break
    fi
    sleep 0.2
done
[[ -n "$SPINE_PORT" ]] || { echo "[rn-harness] SPINE never announced a port — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1; }
echo "[rn-harness]   spine on port $SPINE_PORT (pid $SPINE_PID)"

# -------- 3. gradlew connectedDebugAndroidTest -----------------------------
echo "[rn-harness] 3/5 gradlew connectedDebugAndroidTest -PnostosPort=$SPINE_PORT (on $EMU_SERIAL)"
# Spool logcat across the run so the verdict grep can find the [rn-e2e] lines
# even if gradle's own stdout truncates them. Enlarge the ring buffer first: a
# flaky "PUSH_OK=0 ECHO_OK=1" split was traced to the small default buffer
# dropping the EARLIER [rn-e2e] PUSH_OK line (logged before ECHO_OK) on a busy
# emu — the instrumented test itself PASSED (XML failures=0); only the
# proof-line capture flaked. `-G 8M` + a full `-d` dump (no `-t` tail window)
# makes capture deterministic. See docs/plans/sdk-parity-final-three.md.
"$ADB" -s "$EMU_SERIAL" logcat -G 8M 2>/dev/null || true
"$ADB" -s "$EMU_SERIAL" logcat -c 2>/dev/null || true
cd "$NOSTOS_RN/android"
# shellcheck disable=SC2086
ANDROID_SERIAL="$EMU_SERIAL" ./gradlew connectedDebugAndroidTest -PnostosPort="$SPINE_PORT" --console=plain > "$GRADLE_LOG" 2>&1 || {
    echo "[rn-harness] GRADLE FAILED — tail of $GRADLE_LOG:"; tail -80 "$GRADLE_LOG"; exit 1
}

# -------- 4. capture [rn-e2e] proof lines + test XML -----------------------
echo "[rn-harness] 4/5 capture [rn-e2e] proof lines + test XML"
LOGCAT_DUMP=$("$ADB" -s "$EMU_SERIAL" logcat -d 2>/dev/null || true)
PUSH_OK=0; ECHO_OK=0
# NOT `echo "$LOGCAT_DUMP" | grep -q …` — under `set -o pipefail` that reports
# failure on a SUCCESSFUL match once the dump is big: `grep -q` exits at the
# first hit, `echo` dies of SIGPIPE (141), pipefail propagates it, and
# `&& PUSH_OK=1` never runs. It cost the kotlin slice a red FAIL at 558868b
# with `[kt-e2e] PUSH_OK` sitting in the captured dump; this file had the
# identical shape and was passing only on dump-size luck.
[[ "$LOGCAT_DUMP" == *'[rn-e2e] PUSH_OK'* ]] && PUSH_OK=1
[[ "$LOGCAT_DUMP" == *'[rn-e2e] ECHO_OK'* ]] && ECHO_OK=1

XML_GLOB="$NOSTOS_RN/android/build/outputs/androidTest-results/connected/**/*.xml"
XML_FAIL=$(python3 - "$XML_GLOB" <<'PY' 2>/dev/null || echo "?"
import glob, sys, xml.etree.ElementTree as ET
files = glob.glob(sys.argv[1], recursive=True)
for f in files:
    r = ET.parse(f).getroot()
    print(r.get('failures'))
PY
)

# -------- 5. verdict -------------------------------------------------------
echo "[rn-harness] 5/5 VERDICT: PUSH_OK=$PUSH_OK ECHO_OK=$ECHO_OK xml_failures=$XML_FAIL"
if [[ "$PUSH_OK" == "1" && "$ECHO_OK" == "1" && "$XML_FAIL" == "0" ]]; then
    echo "[rn-harness] SUCCESS: RN TurboModule live round-trip ran on $EMU_SERIAL (PUSH + ECHO both directions)"
    exit 0
else
    echo "[rn-harness] FAILURE: see $GRADLE_LOG + logcat (-t 2000)"
    exit 2
fi
