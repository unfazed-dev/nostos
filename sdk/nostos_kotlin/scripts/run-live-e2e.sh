#!/usr/bin/env bash
# run-live-e2e.sh — Kotlin live-replication E2E orchestrator.
#
 # Wires the on-device `live_connect_push_echo_roundTrip` instrumented test to a
 # host-side spine (`target/debug/examples/e2e_server`) by:
 #   0. Ensuring the nostos_api34 emulator (emulator-5556) is booted.
 #   1. Cross-compiling libnostos_kotlin.so for aarch64-linux-android (NDK).
 #   2. Regenerating UniFFI Kotlin sources from the host .dylib.
 #   3. Stripping + copying the .so into android/src/main/jniLibs/arm64-v8a/.
 #   4. Spawning the spine host-side, capturing its NOSTOS_E2E_PORT.
 #   5. Running `./gradlew connectedDebugAndroidTest -PnostosPort=<port>` —
 #      gradle threads the port into the test apk via
 #      testInstrumentationRunnerArguments["nostosPort"], and the test reads it
 #      back via InstrumentationRegistry.getArguments(). The test reaches the
 #      spine at ws://10.0.2.2:<port>/sync (the emulator's documented
 #      host-loopback alias).
 #   6. Killing the spine on EXIT/INT/TERM.
#
# Port-plumbing choice (reported for the runner): option (a)
# `testInstrumentationRunnerArguments` fed from a `-PnostosPort=` gradle
# property — the cleanest gradle-native path. The test self-skips via
# `assumeTrue` when `nostosPort` is unset ("0") so the offline test still runs
# green without this orchestrator.
#
# Usage: ./scripts/run-live-e2e.sh
# Exit codes: 0 = [kt-e2e] PUSH_OK + ECHO_OK captured + test XML failures=0;
#             non-zero = see $HARNESS_LOG / $SPINE_LOG / $GRADLE_LOG.

set -euo pipefail

ROOT="/Volumes/developer_ssd/Developer/nostos/sdk/nostos_kotlin"
REPO_ROOT="/Volumes/developer_ssd/Developer/nostos"
cd "$ROOT"

AVD="${NOSTOS_AVD:-nostos_api34}"
EMU_SERIAL="${NOSTOS_EMU_SERIAL:-emulator-5556}"
NDK_VERSION="${NOSTOS_NDK_VERSION:-28.2.13676358}"
NDK="$HOME/Library/Android/sdk/ndk/$NDK_VERSION"
ADB="${ADB:-$HOME/Library/Android/sdk/platform-tools/adb}"
EMU_BIN="${EMU_BIN:-$HOME/Library/Android/sdk/emulator/emulator}"

SPINE_BIN="$REPO_ROOT/target/debug/examples/e2e_server"
SPINE_LOG="/tmp/nostos_kotlin_e2e_spine.log"
GRADLE_LOG="/tmp/nostos_kotlin_e2e_gradle.log"
SPINE_PID=""

cleanup() {
    if [[ -n "$SPINE_PID" ]] && kill -0 "$SPINE_PID" 2>/dev/null; then
        kill "$SPINE_PID" 2>/dev/null || true
        wait "$SPINE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

echo "[harness] 0/6 ensure emulator ($AVD @ $EMU_SERIAL)"
if ! "$ADB" devices | grep -q "^$EMU_SERIAL\b.*device$"; then
    echo "[harness]   booting $AVD headless (port $(echo "$EMU_SERIAL" | tr -d 'a-z_'))"
    "$EMU_BIN" -avd "$AVD" -no-window -no-audio -no-boot-anim -gpu swiftshader_indirect \
        -port "$(echo "$EMU_SERIAL" | sed 's/[^0-9]//g')" > /tmp/nostos_kotlin_emu_boot.log 2>&1 &
    EMU_BOOT_PID=$!
    # Wait for the device to be visible + fully booted (sys.boot_completed).
    "$ADB" -s "$EMU_SERIAL" wait-for-device 2>/dev/null || true
    BOOT_DEADLINE=$(( $(date +%s) + 120 ))
    while [[ $(date +%s) -lt $BOOT_DEADLINE ]]; do
        bc=$("$ADB" -s "$EMU_SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r\n ')
        [[ "$bc" == "1" ]] && break
        sleep 2
    done
    bc=$("$ADB" -s "$EMU_SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r\n ')
    [[ "$bc" == "1" ]] || { echo "[harness] EMULATOR BOOT FAILED — see /tmp/nostos_kotlin_emu_boot.log"; exit 1; }
    echo "[harness]   $EMU_SERIAL booted (sys.boot_completed=1)"
else
    echo "[harness]   $EMU_SERIAL already online"
fi

echo "[harness] 1/6 cargo build --target aarch64-linux-android"
export ANDROID_NDK_HOME="$NDK"
export CC_aarch64_linux_android="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android24-clang"
export AR_aarch64_linux_android="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
cargo build --target aarch64-linux-android

echo "[harness] 2/6 uniffi-bindgen generate (kotlin sources)"
cargo build --lib  # host .dylib for bindgen symbol read
uniffi-bindgen generate --library target/debug/libnostos_kotlin.dylib --language kotlin --out-dir kotlin-sources

echo "[harness] 3/6 strip + copy .so → jniLibs/arm64-v8a/"
mkdir -p android/src/main/jniLibs/arm64-v8a
"$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-strip" --strip-debug \
    -o android/src/main/jniLibs/arm64-v8a/libnostos_kotlin.so \
    target/aarch64-linux-android/debug/libnostos_kotlin.so
ls -la android/src/main/jniLibs/arm64-v8a/libnostos_kotlin.so

# Build the spine if missing (mirrors the Swift harness's lazy build).
if [[ ! -x "$SPINE_BIN" ]]; then
    echo "[harness]   building spine (cargo build -p nostos-infra --example e2e_server)"
    (cd "$REPO_ROOT" && cargo build -p nostos-infra --example e2e_server)
fi
[[ -x "$SPINE_BIN" ]] || { echo "[harness] SPINE BUILD FAILED: $SPINE_BIN missing"; exit 1; }

echo "[harness] 4/6 spawn spine (e2e_server)"
rm -f "$SPINE_LOG"
"$SPINE_BIN" > "$SPINE_LOG" 2>&1 &
SPINE_PID=$!

# Discover the port from the NOSTOS_E2E_PORT= stdout line (timeout 10s).
SPINE_PORT=""
PORT_DEADLINE=$(( $(date +%s) + 10 ))
while [[ $(date +%s) -lt $PORT_DEADLINE ]]; do
    if ! kill -0 "$SPINE_PID" 2>/dev/null; then
        echo "[harness] SPINE DIED at startup — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1
    fi
    if grep -q '^NOSTOS_E2E_READY$' "$SPINE_LOG" 2>/dev/null; then
        SPINE_PORT=$(grep '^NOSTOS_E2E_PORT=' "$SPINE_LOG" | tail -1 | cut -d= -f2 | tr -d '[:space:]')
        [[ -n "$SPINE_PORT" ]] && break
    fi
    sleep 0.2
done
[[ -n "$SPINE_PORT" ]] || { echo "[harness] SPINE never announced a port — see $SPINE_LOG"; tail -40 "$SPINE_LOG"; exit 1; }
echo "[harness]   spine on port $SPINE_PORT (pid $SPINE_PID)"

echo "[harness] 5/6 gradlew connectedDebugAndroidTest -PnostosPort=$SPINE_PORT (on $EMU_SERIAL)"
cd android
# ANDROID_SERIAL scopes AGP's connectedDebugAndroidTest to $EMU_SERIAL only —
# without it, AGP fans out across EVERY connected device and the build fails if
# any sibling emulator (e.g. probe_arm64 @ API 37 / 16KB pages) trips a
# device-specific JNA wall unrelated to this slice.
# Enlarge + clear the logcat ring buffer so the run's proof lines (PUSH_OK is
# logged before ECHO_OK) can't scroll out the small default buffer on a busy
# emu — the same capture flake that intermittently returned PUSH_OK=0/ECHO_OK=1
# in the RN harness while the instrumented test itself passed (XML failures=0).
# Fix verified there 2026-07-13; the verdict dump below uses a full -d (no -t).
"$ADB" -s "$EMU_SERIAL" logcat -G 8M 2>/dev/null || true
"$ADB" -s "$EMU_SERIAL" logcat -c 2>/dev/null || true
ANDROID_SERIAL="$EMU_SERIAL" ./gradlew connectedDebugAndroidTest -PnostosPort="$SPINE_PORT" --console=plain > "$GRADLE_LOG" 2>&1 || {
    echo "[harness] GRADLE FAILED — tail of $GRADLE_LOG:"; tail -60 "$GRADLE_LOG"; exit 1
}

echo "[harness] 6/6 capture [kt-e2e] proof lines + test XML"
# Spool logcat from the test run, then grep for the proof lines.
"$ADB" -s "$EMU_SERIAL" logcat -d -t 800 | grep '\[kt-e2e\]' || true
echo "[harness] ----- test XML (failures count) -----"
XML_GLOB="build/outputs/androidTest-results/connected/**/*.xml"
# shellcheck disable=SC2086
python3 - "$XML_GLOB" <<'PY' || true
import glob, sys, xml.etree.ElementTree as ET
files = glob.glob(sys.argv[1], recursive=True)
if not files:
    print("[harness] NO test XML found — did the test run?"); sys.exit(2)
for f in files:
    r = ET.parse(f).getroot()
    print(f"[harness] {f}: tests={r.get('tests')} failures={r.get('failures')} errors={r.get('errors')}")
PY

# Verdict: PUSH_OK + ECHO_OK must both be in logcat, AND failures=0 in XML.
LOGCAT_DUMP=$("$ADB" -s "$EMU_SERIAL" logcat -d 2>/dev/null || true)
PUSH_OK=0; ECHO_OK=0
echo "$LOGCAT_DUMP" | grep -q '\[kt-e2e\] PUSH_OK' && PUSH_OK=1
echo "$LOGCAT_DUMP" | grep -q '\[kt-e2e\] ECHO_OK' && ECHO_OK=1
XML_FAIL=$(python3 - "$XML_GLOB" <<'PY' 2>/dev/null || echo "?"
import glob, sys, xml.etree.ElementTree as ET
files = glob.glob(sys.argv[1], recursive=True)
for f in files:
    r = ET.parse(f).getroot()
    print(r.get('failures'))
PY
)

echo "[harness] VERDICT: PUSH_OK=$PUSH_OK ECHO_OK=$ECHO_OK xml_failures=$XML_FAIL"
if [[ "$PUSH_OK" == "1" && "$ECHO_OK" == "1" && "$XML_FAIL" == "0" ]]; then
    echo "[harness] SUCCESS: live round-trip ran on $EMU_SERIAL (PUSH + ECHO both directions)"
    exit 0
else
    echo "[harness] FAILURE: see $GRADLE_LOG + logcat (-t 2000)"
    exit 2
fi
