#!/bin/sh
# Re-run of the Linux 30k–100k tiers only, after the probe's quorum wait became
# progress-based (docs/plans/scale-ladder-20k-100k.md § Follow-up). The first
# ladder (2026-09-02, commit 4bf9a0d) hit the old max(30s, 1s/1k) quorum cap at
# every tier ≥30k and booked late subscribers' early events as drops. 20k was
# clean twice and is not repeated; Phase A (macOS) is not repeated.
#
# Usage: nohup benches/scripts/scale-ladder-rerun.sh [date-tag] &  → log /tmp/nostos-ladder2.log
# Output: benches/results/raw/<tag>-ladder-rerun/{env.txt,linux-*.log}
set -u
cd "$(dirname "$0")/../.." || exit 90
D=${1:-$(date +%F)}
RAW=benches/results/raw/$D-ladder-rerun
LOG=/tmp/nostos-ladder2.log
exec >>"$LOG" 2>&1
ulimit -n 1048576
mkdir -p "$RAW"
load() { uptime | sed 's/.*averages: //'; }
echo "START $(date '+%F %T') load=$(load)"
{
  echo "date=$D"
  echo "commit=$(git rev-parse --short HEAD) dirty_files=$(git status --porcelain | wc -l | tr -d ' ')"
  echo "rustc=$(rustc --version)"
  echo "host=$(sysctl -n hw.model) ncpu=$(sysctl -n hw.ncpu) macos=$(sw_vers -productVersion)"
  echo "docker=$(docker info --format '{{.NCPU}}cpu/{{.MemTotal}}B' 2>/dev/null)"
  echo "load_at_start=$(load)"
} > "$RAW/env.txt"

tier() { # clients window listeners label
  echo "B: TIER $1 start $(date '+%T') load=$(load) window=$2 listeners=$3"
  benches/scripts/linux-soak.sh "$PWD" ladder-rerun "$RAW/linux-$4.log" "$1" 5000 "$2" 1 "$3"
  echo "B: TIER $1 done $(grep -oE 'LINUX_SOAK_EXIT=[0-9]+|drop% *: *[0-9.]+|ops/sec *: *[0-9]+|ops/sec \(finish\) *: *[0-9a-z/]+|elapsed_to_finish *: *[0-9.]+s|elapsed_to_finish *: *n/a|completed=[a-z]+|peak_rss_mib=[0-9a-z/]+|quorum after [0-9.]+s|complete=[a-z]+|late_subscribers=[0-9]+' "$RAW/linux-$4.log" | tr '\n' ' ')"
  sleep 30
}
tier 30000  400  1 30k
tier 40000  500  1 40k
tier 50000  600  1 50k
tier 100000 1200 2 100k

# Second pass at the largest tier with <1% drops. Keyed on drop% alone:
# `completed=true` means delivered == clients × events, which is unreachable
# once any subscriber arrived after fan-out started (its pre-subscribe events
# are never matched), so at ≥30k it is always false. The probe's
# `elapsed_to_finish` line carries the honest finish time instead.
best=""
for spec in "30000 400 1 30k" "40000 500 1 40k" "50000 600 1 50k" "100000 1200 2 100k"; do
  set -- $spec
  f="$RAW/linux-$4.log"
  drop=$(grep -oE 'drop% *: *[0-9.]+' "$f" | grep -oE '[0-9.]+$')
  if [ -n "$drop" ] && awk -v d="$drop" 'BEGIN{exit !(d<1)}'; then
    best=$1; bestw=$2; bestl=$3; bestlabel=$4
  fi
done
if [ -n "$best" ]; then
  tier "$best" "$bestw" "$bestl" "$bestlabel-pass2"
else
  echo "B: no tier reached <1% drops — no second pass"
fi
echo "END $(date '+%F %T') load=$(load)"
echo LADDER_EXIT=0
