#!/usr/bin/env bash
# scripts/sdk-e2e.sh — run the 10 SDK live-replication E2E slices.
#
# 9 slices (rust, node, tauri, web, capacitor, dotnet, swift, kotlin,
# reactnative) run against the shared no-docker spine
# (nostos-infra/examples/e2e_server) and prove BOTH replication directions:
#   PUSH  — server pushes a row  → SDK applies it → readable on-device
#   ECHO  — SDK write()s a row   → server's echo WriteBack re-emits it
#                                 → SDK applies it → readable on-device
# Flutter is the 10th: it spawns its OWN `cargo run -p nostos-server` (macOS
# desktop) and proves PUSH only (connect→subscribe→watch); its write()/ECHO
# path is covered by the facade unit tests in CI (.github/workflows/ci.yml).
#
# Host slices (rust, node, tauri, web, capacitor) always run. Toolchain- and
# device-dependent slices (dotnet, flutter, swift, kotlin, reactnative) SKIP
# with a reason when their runtime is absent, so the runner is honest on a
# host-only box. Keep this grouping in step with ALL_SLICES below and the
# `want <slice>` guards. See docs/plans/sdk-live-e2e-consolidation.md.
#
# Usage:
#   scripts/sdk-e2e.sh            # run all 9
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
  local name="$1"; local cmd="$2"; local start=$SECONDS; local st
  if bash -c "$cmd" > "/tmp/sdk-e2e-$name.log" 2>&1; then
    st="PASS"
    printf "  ${GREEN}%-13s PASS${RESET}\n" "$name"
  else
    st="FAIL"
    printf "  ${RED}%-13s FAIL${RESET}  (log: /tmp/sdk-e2e-$name.log)\n" "$name"
  fi
  local dur=$((SECONDS - start))
  # One meaningful proof line per slice (varies: PUSH_OK/ECHO_OK for device
  # slices, "All tests passed"/"test result: ok" for host slices). bash-3.2
  # safe (no assoc arrays) — record "name|status|dur|proof" in RESULTS.
  local proof
  proof="$(grep -hoEi '(\[-e2e\]|\[kt-e2e\]|\[rn-e2e\]|\[node-e2e\]|\[cap-e2e\]|\[dotnet-e2e\]) (PUSH_OK|ECHO_OK)|All tests passed!|VERDICT: PUSH_OK=[01] ECHO_OK=[01]|test result: ok\.|PUSH_OK: |ECHO_OK: ' "/tmp/sdk-e2e-$name.log" 2>/dev/null | tail -2 | tr '\n' ' ' | cut -c1-60)"
  RESULTS+=("$name|$st|${dur}s|$proof")
}

skip_slice() { # <name> <reason>
  printf "  ${YELLOW}%-13s SKIP${RESET}  %s\n" "$1" "$2"
  RESULTS+=("$1|SKIP|-|$2")
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
# The addon (nostos_node.node) is gitignored and NOT produced by `cargo build`:
# cargo emits target/release/libnostos_node.{dylib,so}. Without the install step
# below the slice loaded whatever stale *.node a previous manual napi build had
# left in the tree — so it passed locally against a two-month-old binary and
# failed in CI (fresh checkout, MODULE_NOT_FOUND) every time.
want node      && run_slice node      "cd sdk/nostos_node && cargo build --release -q && cp \"\$(ls target/release/libnostos_node.dylib target/release/libnostos_node.so target/release/nostos_node.dll 2>/dev/null | head -1)\" nostos_node.node && node smoke_live.cjs"
want tauri     && run_slice tauri     "cd sdk/nostos_tauri && cargo test -- --nocapture && cd fixture && cargo test -- --nocapture"
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

# Flutter — macOS desktop live integration test (sdk/nostos_flutter/example).
# Restored 2026-08-05 (commit e749c27) after the 2026-07-30 archive. Unlike the
# shared-spine slices it spawns its OWN `cargo run -p nostos-server` at
# 127.0.0.1:8801 and drives connect/subscribe/watch inside a genuine macOS app
# bundle. PUSH-only — asserts the server fans out rows that watch() emits; no
# write()/WriteBack ECHO leg (the SDK write path is covered by the facade unit
# tests in the CI `flutter` job). macOS-gated: `-d macos` needs a Darwin host
# with flutter; SKIP honestly elsewhere (matches swift/kotlin).
#
# The doc-signature guard that used to live in this block moved to the `flutter`
# job in .github/workflows/ci.yml — it caught README/USAGE documenting three
# `NostosDatabase.supabase` parameters that never existed. Do not let it stop
# running.
if want flutter; then
  if [ "$(uname -s)" = "Darwin" ] && command -v flutter >/dev/null 2>&1; then
    # If macOS `open` can't foreground the app (headless/agent/CI without a
    # GUI session), the test times out — an environment limit, not a code
    # defect, so SKIP honestly instead of reporting a false FAIL.
    _flutter_cmd="cd sdk/nostos_flutter/example && flutter test integration_test/nostos_server_test.dart -d macos"
    _flutter_start=$SECONDS
    if bash -c "$_flutter_cmd" > /tmp/sdk-e2e-flutter.log 2>&1; then
      _flutter_dur=$((SECONDS - _flutter_start))
      _flutter_proof="$(grep -hoEi 'All tests passed!|PUSH_OK' /tmp/sdk-e2e-flutter.log 2>/dev/null | tail -1 | cut -c1-60)"
      printf "  ${GREEN}%-13s PASS${RESET}\n" "flutter"
      RESULTS+=("flutter|PASS|${_flutter_dur}s|$_flutter_proof")
    elif grep -q 'Failed to foreground app' /tmp/sdk-e2e-flutter.log 2>/dev/null; then
      skip_slice flutter "(macOS \`open\` can't foreground — no GUI session; environment limit)"
    else
      _flutter_dur=$((SECONDS - _flutter_start))
      printf "  ${RED}%-13s FAIL${RESET}  (log: /tmp/sdk-e2e-flutter.log)\n" "flutter"
      RESULTS+=("flutter|FAIL|${_flutter_dur}s|")
    fi
  else
    skip_slice flutter "(macOS host + flutter required — \`flutter test -d macos\`)"
  fi
fi

# Swift — needs a BOOTED iPhone simulator (xcodebuild + simctl). Checking only
# that simctl runs (i.e. that a simulator is *installed*) turns "nothing to run
# against" into a red FAIL; the Android guards below check for a booted device,
# so match them and SKIP honestly instead.
#
# All three device guards below feed `grep -q` from a HERE-STRING, never a pipe.
# `cmd | grep -q` under `set -o pipefail` reports failure on a successful match
# when cmd's output is long enough: grep exits at the first hit, cmd dies of
# SIGPIPE (141), pipefail propagates it. In a guard that inverts the meaning —
# the device IS booted and the slice SKIPs anyway, which strict mode then counts
# as a failure. `<<<` feeds a file, so nothing can be signalled.
if want swift; then
  if grep -q '(Booted)' <<< "$(xcrun simctl list devices 2>/dev/null || true)"; then
    run_slice swift "cd sdk/nostos_swift/ios-test && ./build.sh"
  else
    skip_slice swift "(no booted iPhone simulator — \`xcrun simctl boot <device>\`)"
  fi
fi

# Kotlin — needs an Android API-34 emulator (nostos_api34 / emulator-5556).
if want kotlin; then
  ADB="${ANDROID_HOME:-$HOME/Library/Android/sdk}/platform-tools/adb"
  if [ -x "$ADB" ] && grep -q 'emulator.*device' <<< "$("$ADB" devices 2>/dev/null || true)"; then
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
  if [ -x "$ADB" ] && grep -q 'emulator.*device' <<< "$("$ADB" devices 2>/dev/null || true)"; then
    run_slice reactnative "cd sdk/nostos_react_native && ./scripts/run-android-e2e.sh"
  else
    skip_slice reactnative "(no booted Android emulator)"
  fi
fi

# ---- per-SDK summary table (one row per SDK, not one collapsed line) ----
echo -e "\n${BOLD}Per-SDK results:${RESET}"
printf "  ${BOLD}%-13s  %-6s  %-6s  %s${RESET}\n" "SDK" "result" "dur" "proof/detail"
for s in "${SLICES[@]}"; do
  st=""; dur=""; proof=""
  for r in "${RESULTS[@]}"; do
    [ "${r%%|*}" = "$s" ] || continue
    rest=${r#*|}; st=${rest%%|*}; rest2=${rest#*|}; dur=${rest2%%|*}; proof=${rest2#*|}
    break
  done
  case "$st" in PASS) col=$GREEN;; FAIL) col=$RED;; *) col=$YELLOW;; esac
  printf "  %-13s  ${col}%-6s${RESET}  %-6s  %s\n" "$s" "${st:--}" "$dur" "$proof"
done
pass=0; fail=0; skip=0
for r in "${RESULTS[@]}"; do
  rest=${r#*|}; st=${rest%%|*}
  case "$st" in
    PASS) pass=$((pass+1));;
    FAIL) fail=$((fail+1));;
    SKIP) skip=$((skip+1));;
  esac
done
echo ""
printf "  ${GREEN}%d passed${RESET}, ${RED}%d failed${RESET}, ${YELLOW}%d skipped${RESET} / %d slices\n" \
  "$pass" "$fail" "$skip" "${#RESULTS[@]}"

# Strict mode (CI): a SKIP means the toolchain we expected wasn't there, which
# on a runner is a broken job, not an honest "no device". Without this, a CI
# job that names its slices still goes green when every one of them skips —
# the same false-pass shape as the NOSTOS_E2E_PG suite self-skipping. Local runs
# leave this unset so device-less boxes stay honest rather than noisy.
if [ "${SDK_E2E_STRICT:-0}" = "1" ] && [ "$skip" -gt 0 ]; then
  printf "  ${RED}strict mode: %d skipped slice(s) count as failures${RESET}\n" "$skip"
  exit $((fail + skip))
fi
exit "$fail"
