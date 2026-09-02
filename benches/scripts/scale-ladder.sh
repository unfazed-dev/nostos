#!/bin/sh
# Scale ladder + macOS 10k re-check — docs/plans/scale-ladder-20k-100k.md.
#
# Phase A (macOS native): 3 x 10k soak from an idle host with 60 s gaps, then one
#   1k bench pass immediately followed by a 4th soak (was soak 1's 64.73% on
#   2026-09-02 a cold-start artefact, or is macOS 10k inherently unstable?).
#   macOS cannot go above ~9.2k sockets (ENOBUFS) / 16k ephemeral ports / 61k FDs
#   per process, so no 20k+ tier is attempted natively (sysctl snapshot in env.txt).
# Phase B (Linux, Docker): tiers 20k 30k 40k 50k 100k via linux-soak.sh, 5000
#   events each, per-tier window, 100k on 2 loopback listener addresses; then a
#   second pass at the largest tier that completed with <1% drops.
#
# Usage: nohup benches/scripts/scale-ladder.sh [date-tag] &   → log /tmp/nostos-ladder.log
# Output: benches/results/raw/<tag>-ladder/{env.txt,mac-*.log,linux-*.log}
set -u
cd "$(dirname "$0")/../.." || exit 90
D=${1:-$(date +%F)}
RAW=benches/results/raw/$D-ladder
BENCH_OUT=benches/results/ladder-$D
LOG=/tmp/nostos-ladder.log
exec >>"$LOG" 2>&1
ulimit -n 1048576
mkdir -p "$RAW" "$BENCH_OUT"
load() { uptime | sed 's/.*averages: //'; }
echo "START $(date '+%F %T') load=$(load)"

echo "BUILD start $(date '+%T')"
cargo build --release -p nostos-bench --bin nostos-bench --bin nostos-bench-10k >/tmp/nostos-bench-build.log 2>&1 \
  || { echo "BUILD FAILED (see /tmp/nostos-bench-build.log)"; echo LADDER_EXIT=91; exit 91; }
echo "BUILD ok $(date '+%T')"
{
  echo "date=$D"
  echo "commit=$(git rev-parse --short HEAD) dirty_files=$(git status --porcelain | wc -l | tr -d ' ')"
  echo "rustc=$(rustc --version)"
  echo "host=$(sysctl -n hw.model) ncpu=$(sysctl -n hw.ncpu) macos=$(sw_vers -productVersion)"
  echo "ulimit_n=$(ulimit -n) maxfilesperproc=$(sysctl -n kern.maxfilesperproc) nmbclusters=$(sysctl -n kern.ipc.nmbclusters) somaxconn=$(sysctl -n kern.ipc.somaxconn)"
  echo "portrange=$(sysctl -n net.inet.ip.portrange.first)-$(sysctl -n net.inet.ip.portrange.last)"
  echo "docker=$(docker info --format '{{.NCPU}}cpu/{{.MemTotal}}B' 2>/dev/null)"
  echo "load_at_start=$(load)"
} > "$RAW/env.txt"

# ---------- Phase A: macOS 10k re-check ----------
echo "A: idle cool-down 60s"; sleep 60
for s in 1 2 3; do
  echo "A: SOAK idle$s start $(date '+%T') load=$(load)"
  target/release/nostos-bench-10k 10000 5000 60 >"$RAW/mac-soak-idle$s.log" 2>&1
  echo "A: SOAK idle$s done rc=$? $(grep -oE 'drop% *: *[0-9.]+|ops/sec *: *[0-9]+|completed=[a-z]+' "$RAW/mac-soak-idle$s.log" | tr '\n' ' ')"
  sleep 60
done
echo "A: BENCH pass start $(date '+%T') load=$(load)"
BENCH_CLIENTS=1000 BENCH_EVENTS=100000 BENCH_RESULTS_DIR="$BENCH_OUT/mac-bench" make bench >"$RAW/mac-bench.log" 2>&1
echo "A: BENCH pass done rc=$? $(grep -oE 'ops_per_sec=[0-9.]+|drop_rate=[0-9.]+' "$RAW/mac-bench.log" | tr '\n' ' ')"
echo "A: SOAK afterbench start $(date '+%T') load=$(load)"
target/release/nostos-bench-10k 10000 5000 60 >"$RAW/mac-soak-afterbench.log" 2>&1
echo "A: SOAK afterbench done rc=$? $(grep -oE 'drop% *: *[0-9.]+|ops/sec *: *[0-9]+|completed=[a-z]+' "$RAW/mac-soak-afterbench.log" | tr '\n' ' ')"

# ---------- Phase B: Linux ladder ----------
tier() { # clients window listeners label
  echo "B: TIER $1 start $(date '+%T') load=$(load) window=$2 listeners=$3"
  benches/scripts/linux-soak.sh "$PWD" ladder "$RAW/linux-$4.log" "$1" 5000 "$2" 1 "$3"
  echo "B: TIER $1 done $(grep -oE 'LINUX_SOAK_EXIT=[0-9]+|drop% *: *[0-9.]+|ops/sec *: *[0-9]+|completed=[a-z]+|peak_rss_mib=[0-9a-z/]+|quorum after [0-9.]+s' "$RAW/linux-$4.log" | tr '\n' ' ')"
  sleep 30
}
tier 20000  300  1 20k
tier 30000  400  1 30k
tier 40000  500  1 40k
tier 50000  600  1 50k
tier 100000 1200 2 100k

# Second pass at the largest tier that completed with <1% drops.
best=""; bestw=""; bestl=""
for spec in "20000 300 1 20k" "30000 400 1 30k" "40000 500 1 40k" "50000 600 1 50k" "100000 1200 2 100k"; do
  set -- $spec
  f="$RAW/linux-$4.log"
  drop=$(grep -oE 'drop% *: *[0-9.]+' "$f" | grep -oE '[0-9.]+$')
  if grep -q 'completed=true' "$f" && [ -n "$drop" ] && awk -v d="$drop" 'BEGIN{exit !(d<1)}'; then
    best=$1; bestw=$2; bestl=$3; bestlabel=$4
  fi
done
if [ -n "$best" ]; then
  tier "$best" "$bestw" "$bestl" "$bestlabel-pass2"
else
  echo "B: no tier completed with <1% drops — no second pass"
fi
echo "END $(date '+%F %T') load=$(load)"
echo LADDER_EXIT=0
