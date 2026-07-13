#!/usr/bin/env bash
# scripts/run-dotnet-e2e.sh — .NET (C#) live-E2E against the shared spine.
#
# The C# mirror of sdk/nostos_node/smoke_live.cjs: spawns the shared no-docker
# spine (nostos-infra/examples/e2e_server), waits for NOSTOS_E2E_READY, runs the
# C# smoke (dotnet/smoke) against it, and asserts PUSH_OK + ECHO_OK through the
# UniFFI-CS binding over libnostos_dotnet.dylib. Tears the spine down on exit.
#
# Requires `dotnet` on PATH (brew install --cask dotnet-sdk → the .NET 10 SDK,
# which builds + runs the net8.0 TFM via RollForward=Major). scripts/sdk-e2e.sh
# guards this with `command -v dotnet` and SKIPs honestly when it is absent.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DOTNET_DIR="$ROOT/sdk/nostos_dotnet"

# Resolve `dotnet`: prefer PATH, fall back to the no-sudo user-local install
# (dotnet-install.sh → ~/.dotnet). The brew `dotnet-sdk` cask's .pkg installer
# needs sudo; the user-local tarball install does not — honor either here.
if ! command -v dotnet >/dev/null 2>&1; then
  if [ -x "$HOME/.dotnet/dotnet" ]; then
    export DOTNET_ROOT="$HOME/.dotnet"
    export PATH="$DOTNET_ROOT:$PATH"
  else
    echo "[dotnet-e2e] FAIL: dotnet not found (not on PATH, no ~/.dotnet/dotnet)"
    exit 1
  fi
fi

# 1. Build the host cdylib (release) — Smoke.csproj copies it next to the
#    managed assembly so DllImport("nostos_dotnet") resolves from app base.
echo "[dotnet-e2e] building libnostos_dotnet.dylib (host, release)…"
(cd "$DOTNET_DIR" && cargo build --release -q) || { echo "[dotnet-e2e] FAIL: cargo build"; exit 1; }

# 2. Build the spine example if absent (mirrors the node smoke).
SPINE="$ROOT/target/debug/examples/e2e_server"
if [ ! -x "$SPINE" ]; then
  echo "[dotnet-e2e] building spine (nostos-infra example)…"
  (cd "$ROOT" && cargo build -q -p nostos-infra --example e2e_server) || { echo "[dotnet-e2e] FAIL: spine build"; exit 1; }
fi

# 3. Spawn the spine; discover its port via stdout lines (30s ready timeout).
SPINE_LOG="$(mktemp -t nostos-dotnet-e2e-spine)"
"$SPINE" >"$SPINE_LOG" 2>&1 &
SPINE_PID=$!
cleanup() { kill -TERM "$SPINE_PID" >/dev/null 2>&1 || true; rm -f "$SPINE_LOG"; }
trap cleanup EXIT

echo "[dotnet-e2e] launching spine…"
SPINE_PORT=""
ready_deadline=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -lt "$ready_deadline" ]; do
  if ! kill -0 "$SPINE_PID" 2>/dev/null; then
    echo "[dotnet-e2e] FAIL: spine exited early. log:"; cat "$SPINE_LOG"; exit 1
  fi
  p="$(grep -o 'NOSTOS_E2E_PORT=[0-9]*' "$SPINE_LOG" | head -1 | cut -d= -f2)"
  if grep -q '^NOSTOS_E2E_READY$' "$SPINE_LOG" && [ -n "$p" ]; then
    SPINE_PORT="$p"; break
  fi
  sleep 0.3
done
if [ -z "$SPINE_PORT" ]; then
  echo "[dotnet-e2e] FAIL: spine never reached NOSTOS_E2E_READY within 30s. log:"; cat "$SPINE_LOG"; exit 1
fi
echo "[dotnet-e2e] spine ready on port $SPINE_PORT"

# 4. Run the C# smoke. Belt-and-suspenders: expose the dylib dir via DYLD too
#    (brew dotnet lives in /opt/homebrew — not SIP-protected, so DYLD_* is
#    honored). The csproj already copies the dylib next to the assembly.
export NOSTOS_E2E_PORT="$SPINE_PORT"
export DYLD_LIBRARY_PATH="$DOTNET_DIR/target/release${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"

cd "$DOTNET_DIR/dotnet/smoke"
if dotnet run --project Smoke.csproj -c Release; then
  echo "[dotnet-e2e] VERDICT: PUSH_OK=1 ECHO_OK=1 (smoke exited 0)"
  exit 0
else
  rc=$?
  echo "[dotnet-e2e] VERDICT: smoke exited $rc"
  exit "$rc"
fi
