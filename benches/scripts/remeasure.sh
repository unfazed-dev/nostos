#!/bin/sh
# Official macOS-native re-measure: 3 x `make bench` at the recorded baseline config
# (clients=1000 events=100000 profile=small buffer=1024) + 2 x 10k soak.
# Committed 2026-09-02 after a reboot wiped the /tmp copies of run1/run2 — keep it in-tree.
#
# Usage:  benches/scripts/remeasure.sh [tag]        (tag defaults to today's date)
# Output: benches/results/remeasure-<tag>/pass{1,2,3}/   bench JSON artifacts
#         benches/results/raw/<tag>/{env.txt,bench-pass*.log,soak-pass*.log}
#         /tmp/nostos-remeasure-<tag>.log             progress; last line REMEASURE_EXIT=<rc>
# Run it detached (nohup … &) and poll the log; a pass takes ~2 min, a soak 60 s.
# Do NOT cite the 10k soak numbers from macOS as throughput — the host caps loopback
# sockets at ~9.2k (ENOBUFS); use benches/scripts/linux-soak.sh for 10k (RESULTS.md 2026-09-02).
set -u
cd "$(dirname "$0")/../.." || exit 90
D=${1:-$(date +%F)}
OUT=benches/results/remeasure-$D
RAW=benches/results/raw/$D
LOG=/tmp/nostos-remeasure-$D.log
exec >>"$LOG" 2>&1
ulimit -n 1048576
mkdir -p "$OUT" "$RAW"
echo "START $(date '+%F %T') load=$(uptime | sed 's/.*averages: //')"
echo "BUILD start $(date '+%T')"
cargo build --release -p nostos-bench --bin nostos-bench --bin nostos-bench-10k >/tmp/nostos-bench-build.log 2>&1 \
  || { echo "BUILD FAILED (see /tmp/nostos-bench-build.log)"; echo REMEASURE_EXIT=91; exit 91; }
[ -x target/release/nostos-bench-10k ] || { echo "no target/release/nostos-bench-10k after build"; echo REMEASURE_EXIT=92; exit 92; }
echo "BUILD ok $(date '+%T')"
{
  echo "date=$D"
  echo "commit=$(git rev-parse --short HEAD) dirty_files=$(git status --porcelain | wc -l | tr -d ' ')"
  echo "rustc=$(rustc --version)"
  echo "host=$(sysctl -n hw.model) ncpu=$(sysctl -n hw.ncpu) macos=$(sw_vers -productVersion)"
  echo "ulimit_n=$(ulimit -n) maxfilesperproc=$(sysctl -n kern.maxfilesperproc)"
  echo "load_at_start=$(uptime | sed 's/.*averages: //')"
} > "$RAW/env.txt"
for p in 1 2 3; do
  echo "PASS $p start $(date '+%T') load=$(uptime | sed 's/.*averages: //')"
  BENCH_CLIENTS=1000 BENCH_EVENTS=100000 BENCH_RESULTS_DIR="$OUT/pass$p" make bench >"$RAW/bench-pass$p.log" 2>&1
  rc=$?
  echo "PASS $p done rc=$rc $(grep -oE 'ops_per_sec=[0-9.]+|drop_rate=[0-9.]+' "$RAW/bench-pass$p.log" | tr '\n' ' ')"
done
for s in 1 2; do
  echo "SOAK $s start $(date '+%T') load=$(uptime | sed 's/.*averages: //')"
  target/release/nostos-bench-10k 10000 5000 60 >"$RAW/soak-pass$s.log" 2>&1
  echo "SOAK $s done rc=$? $(tail -1 "$RAW/soak-pass$s.log" | cut -c1-160)"
  sleep 60   # cool-down: the probe does no teardown; back-to-back soaks measured 2x apart (RESULTS.md 2026-09-02)
done
echo "END $(date '+%F %T')"
echo REMEASURE_EXIT=0
