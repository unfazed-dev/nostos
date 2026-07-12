#!/usr/bin/env bash
# scripts/sdk-e2e.sh — run the 7 SDK live-replication E2E slices against the
# shared no-docker spine (nostos-infra/examples/e2e_server).
#
# Each slice spawns its own spine instance and proves BOTH replication
# directions through the SDK's real public API:
#   PUSH  — server pushes a row  → SDK applies it → readable on-device
#   ECHO  — SDK write()s a row   → server's echo WriteBack re-emits it
#                                 → SDK applies it → readable on-device
#
# Host slices (rust, node, tauri, web) always run. Device-dependent slices
# (flutter, swift, kotlin) SKIP with a reason when their runtime is absent, so
# the runner is honest on a host-only box. See
# docs/plans/sdk-live-e2e-consolidation.md.
#
# Usage:
#   scripts/sdk-e2e.sh            # run all 7
#   scripts/sdk-e2e.sh rust node  # run a subset (names match the slice keys)

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; RED=$'\033[0;31m'
BOLD=$'\033[1m';   RESET=$'\033[0m'

ALL_SLICES=(rust node tauri web capacitor dotnet flutter swift kotlin reactnative)
if [ "$#" -gt 0 ]; then
  SLICES=("$@")
else
  SLICES=("${ALL_SLICES[@]}")
fi

declare -a RESULTS=()

run_slice() { # <name> <command-string>
  local name="$1"; local cmd="$2"
  if bash -c "$cmd" > "/tmp/sdk-e2e-$name.log" 2>&1; then
    printf "  ${GREEN}%-8s PASS${RESET}\n" "$name"
    RESULTS+=("$name|PASS")
  else
    printf "  ${RED}%-8s FAIL${RESET}  (log: /tmp/sdk-e2e-$name.log)\n" "$name"
    RESULTS+=("$name|FAIL")
  fi
}

skip_slice() { # <name> <reason>
  printf "  ${YELLOW}%-8s SKIP${RESET}  %s\n" "$1" "$2"
  RESULTS+=("$1|SKIP")
}

want() { # <name> — 0 if this slice is selected
  local n="$1"
  for s in "${SLICES[@]}"; do [ "$s" = "$n" ] && return 0; done
  return 1
}

# Pre-build the spine once so each slice's spawn finds it (slices also self-build
# if absent, but this avoids per-slice rebuild races).
echo -e "${BOLD}building the shared spine…${RESET}"
cargo build -q -p nostos-infra --examples 2>&1 | tail -1
echo -e "${BOLD}SDK live-E2E slices:${RESET}"

want rust      && run_slice rust      "cargo test -q -p nostos-client --test e2e_live_replication -- --nocapture"
want node      && run_slice node      "cd sdk/nostos_node && cargo build --release -q && node smoke_live.cjs"
want tauri     && run_slice tauri     "cd sdk/nostos_tauri && cargo test -- --nocapture"
want web       && run_slice web       "cd sdk/nostos_web && npx playwright test --config=playwright.config.cjs"
want capacitor && run_slice capacitor "cd sdk/nostos_capacitor && npm install --no-audit --no-fund && npm run build && cd example-app && npm install --no-audit --no-fund && npx playwright test --config=playwright.config.cjs"
# dotnet — C# binding live-E2E against the shared spine (PUSH+ECHO). Loads the
# host libnostos_dotnet.dylib over the UniFFI-CS surface via the dotnet/smoke
# console app (the C# mirror of sdk/nostos_node/smoke_live.cjs). Requires `dotnet`
# (brew install --cask dotnet-sdk); SKIPs honestly when absent.
if want dotnet; then
  if command -v dotnet >/dev/null 2>&1 || [ -x "$HOME/.dotnet/dotnet" ]; then
    run_slice dotnet "cd sdk/nostos_dotnet && ./scripts/run-dotnet-e2e.sh"
  else
    skip_slice dotnet "(dotnet not installed — dot.net/v1/dotnet-install.sh | bash, or brew install --cask dotnet-sdk)"
  fi
fi

# Flutter — packaging + live-sync E2E. nostos_server_test.dart spins up a REAL
# `cargo run -p nostos-server` (NOSTOS_REPLICATOR=fake, NOSTOS_SYNC_AUTH=none — no
# Postgres, no docker, no cloud; the same no-DB spine pattern the rust/node/web
# slices use) and drives the Flutter SDK's connect/subscribe/watch loop inside a
# genuine app bundle. `-d macos` because the test binds the server on 127.0.0.1
# (host loopback) — only a host/desktop target reaches it directly (an emulator
# would need 10.0.2.2). A real Supabase-CLOUD-backed live test is a separate
# follow-up (needs the cloud project ref — see docs/plans/flutter-supabase-plug-and-play-launch.md).
if want flutter; then
  if command -v flutter >/dev/null 2>&1; then
    run_slice flutter "cd sdk/nostos_flutter/example && flutter test integration_test/nostos_server_test.dart -d macos"
  else
    skip_slice flutter "(flutter not on PATH)"
  fi
fi

# Swift — needs the iPhone simulator (xcodebuild + simctl).
if want swift; then
  if xcrun simctl list devices >/dev/null 2>&1; then
    run_slice swift "cd sdk/nostos_swift/ios-test && ./build.sh"
  else
    skip_slice swift "(no Xcode / iPhone simulator)"
  fi
fi

# Kotlin — needs an Android API-34 emulator (nostos_api34 / emulator-5556).
if want kotlin; then
  ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
  if [ -x "$ADB" ] && "$ADB" devices 2>/dev/null | grep -q 'emulator.*device'; then
    run_slice kotlin "cd sdk/nostos_kotlin && ./scripts/run-live-e2e.sh"
  else
    skip_slice kotlin "(no booted Android emulator)"
  fi
fi

# React Native — Android Kotlin TurboModule (reuses nostos_kotlin's .so + UniFFI
# bindings). Needs a booted Android emulator; run-android-e2e.sh builds the .so,
# spawns the spine, and runs the instrumented PUSH+ECHO round-trip. iOS TurboModule
# is a fast-follow (nostos_swift is sim-proven, so the pieces exist).
if want reactnative; then
  ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
  if [ -x "$ADB" ] && "$ADB" devices 2>/dev/null | grep -q 'emulator.*device'; then
    run_slice reactnative "cd sdk/nostos_react_native && ./scripts/run-android-e2e.sh"
  else
    skip_slice reactnative "(no booted Android emulator)"
  fi
fi

# ---- summary ----
echo -e "\n${BOLD}Summary:${RESET}"
pass=0; fail=0; skip=0
for r in "${RESULTS[@]}"; do
  case "${r##*|}" in
    PASS) pass=$((pass+1));;
    FAIL) fail=$((fail+1));;
    SKIP) skip=$((skip+1));;
  esac
done
ran=${#RESULTS[@]}
printf "  ${GREEN}%d passed${RESET}, %d failed, ${YELLOW}%d skipped${RESET} / %d slices\n" \
  "$pass" "$fail" "$skip" "$ran"
exit "$fail"
