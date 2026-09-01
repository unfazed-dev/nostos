#!/usr/bin/env bash
# ADR-0041 D5 field leg — DESKTOP half (the phone-on-cellular half is the
# owner's; runbook: docs/plans/adr-0041-d5-field-leg.md).
#
# Boots nostos-server in iroh transport mode (fake replicator, so rows keep
# flowing), scrapes the QR-native dial URL, health-checks it, then proves
# RESUME-NOT-RESTART with two iroh_dial_check probes around an offline gap:
# the second checkpoint must EXCEED the first.
#
# Optional env:
#   NOSTOS_IROH_RELAY_URL  self-hosted relay (iroh-relay --dev) — recommended,
#                         drops the n0-fleet variable
#   D5_GAP_SECS           offline-gap length between probes (default 20)
set -euo pipefail

BIND="${D5_BIND:-127.0.0.1:8199}"
GAP="${D5_GAP_SECS:-20}"
DIR="/tmp/nostos-d5"
mkdir -p "$DIR"
LOG="$DIR/server.log"

echo '[d5] starting nostos-server (transport=iroh, replicator=fake)...'
env NOSTOS_BIND="$BIND" NOSTOS_REPLICATOR=fake \
    ${NOSTOS_IROH_RELAY_URL:+NOSTOS_IROH_RELAY_URL="$NOSTOS_IROH_RELAY_URL"} \
    cargo run -q -p nostos-server -- --transport iroh >"$LOG" 2>&1 &
SRV=$!
trap 'kill "$SRV" 2>/dev/null || true' EXIT

URL=''
for _ in $(seq 1 180); do
  URL=$(grep -o 'iroh://[^ "]*' "$LOG" | head -1 || true)
  [ -n "$URL" ] && break
  sleep 1
done
if [ -z "$URL" ]; then echo "FAIL: no iroh:// dial URL — server log:"; tail -5 "$LOG"; exit 1; fi
echo "[d5] dial url: $URL"
echo "$URL" > "$DIR/dial_url.txt"

for _ in $(seq 1 30); do
  python3 -c "import urllib.request;urllib.request.urlopen('http://$BIND/healthz',timeout=2)" 2>/dev/null && break
  sleep 1
done
echo '[d5] healthz ok'

probe() {
  cargo run -q -p nostos-client --features iroh --example iroh_dial_check -- "$URL" >"$DIR/probe$1.log" 2>&1 \
    && grep 'checkpoint LSN' "$DIR/probe$1.log" | sed -n "s/.*checkpoint LSN \([0-9]*\).*/\1/p" \
    || { echo "FAIL: probe $1 — see $DIR/probe$1.log"; exit 1; }
}

CP1=$(probe 1)
echo "[d5] probe 1 checkpoint: $CP1"
echo "[d5] offline gap: ${GAP}s (server keeps emitting)..."
sleep "$GAP"
CP2=$(probe 2)
echo "[d5] probe 2 checkpoint: $CP2"

echo "[d5] checkpoint 1=$CP1 2=$CP2"
if [ "$CP2" -gt "$CP1" ]; then
  echo 'PASS: second dial resumed from the durable checkpoint (advanced, not replayed).'
  echo 'Next: the phone leg — docs/plans/adr-0041-d5-field-leg.md Part 2.'
  exit 0
fi
echo 'FAIL: checkpoint did not advance — resume semantics bug, not a network artifact.'
exit 1