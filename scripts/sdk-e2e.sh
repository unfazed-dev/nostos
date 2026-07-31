#!/usr/bin/env bash
# scripts/sdk-e2e.sh — run the 9 SDK live-replication E2E slices against the
# shared no-docker spine (nostos-infra/examples/e2e_server).
#
# Each slice spawns its own spine instance and proves BOTH replication
# directions through the SDK's real public API:
#   PUSH  — server pushes a row  → SDK applies it → readable on-device
#   ECHO  — SDK write()s a row   → server's echo WriteBack re-emits it
#                                 → SDK applies it → readable on-device
#
# Host slices (rust, node, tauri, web, capacitor) always run. Toolchain- and
# device-dependent slices (dotnet, flutter, swift, kotlin, reactnative) SKIP
# with a reason when their runtime is absent, so the runner is honest on a
# host-only box. Keep this grouping in step with ALL_SLICES below and the
# `want <slice>` guards — it said "7 slices / host: rust node tauri web /
# device: flutter swift kotlin" until 2026-07-30, three slices after that
# stopped being true. See docs/plans/sdk-live-e2e-consolidation.md.
#
# Usage:
#   scripts/sdk-e2e.sh            # run all 9
#   scripts/sdk-e2e.sh rust node  # run a subset (names match the slice keys)

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; RED=$'\033[0;31m'
BOLD=$'\033[1m';   RESET=$'\033[0m'

ALL_SLICES=(rust node tauri web capacitor dotnet swift kotlin reactnative)
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

# Flutter — NO LIVE SLICE since 2026-07-30. Operator decision: "no example app
# to live in the SDK", then "archive the tests as well - take it all".
# `sdk/nostos_flutter/{example,test,test_driver}` moved under `archive/`, so
# nostos_server_test.dart — a REAL `cargo run -p nostos-server` driven through the
# SDK's connect/subscribe/watch loop inside a genuine macOS app bundle — has no
# host app left to build against.
#
# `flutter` is OUT of ALL_SLICES: the honest count is 9 live slices, not 10.
# Deliberately not `skip_slice`d — SDK_E2E_STRICT=1 converts a SKIP into a CI
# failure, and leaving it listed would claim PUSH+ECHO coverage that no longer
# exists. Restoring it means a Flutter host app under `fixtures/` (see
# docs/plans/multi-sdk-pomodoro-fixture-matrix.md).
#
# The doc-signature guard that used to ride along here moved to the `flutter`
# job in .github/workflows/ci.yml. It needs no Flutter SDK, and it is the check
# that caught README.md and USAGE.md both documenting three
# `NostosDatabase.supabase` parameters that never existed. Do not let it stop
# running again.
#
# An explicit `sdk-e2e.sh flutter` must fail loudly rather than silently pass:
# a no-op branch reporting success is the exact false-green this harness guards
# against everywhere else.
if want flutter; then
  printf '%s\n' "${RED}FAIL${RESET} flutter — slice archived 2026-07-30; host app now at archive/sdk/nostos_flutter/example." >&2
  printf '%s\n' "       Restore live coverage with a Flutter host app under fixtures/." >&2
  exit 1
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
