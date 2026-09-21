#!/usr/bin/env bash
# Diagnostic for the 100k fan-out cliff (RESULTS.md § "Re-run with progress-based
# quorum": 50k = 0.12 s/event, 100k = 1.22 s/event, zero drops). Two tiers, each
# with a VM-level sampler alongside the probe's own `[diag]` progress line, so
# the slow tier can be attributed to one of:
#   kernel TCP memory pressure  -> sockstat `TCP: mem` climbs, TcpExt
#                                  TCPMemoryPressures / PruneCalled / RcvCollapsed advance
#   VM memory reclaim / swap    -> MemAvailable -> 0, SwapFree falls, PSI memory
#                                  `full` climbs, probe `swap_mib` > 0
#   CPU saturation (kernel)     -> /proc/stat sys jiffies dominate, PSI cpu `some` high
#   app-level loop cost         -> none of the above; rate uniform from t=0
# Mirrors benches/scripts/linux-soak.sh but uses its OWN container name and
# volumes so it never fights the ladder scripts' build. Container-side logic
# lives in fanout-100k-diag-inner.sh.
#
# Headroom rule (docs/BENCHMARK-METHODOLOGY.md): a BUILD may run under any
# host load; a MEASUREMENT only starts when host load1 < 8 (0.8 × 10 cores),
# no other nostos-linux-* container is up, and /tmp/nostos-bench.lock is free.
# Each tier is its own docker run with host load1 logged before and after.
#
# Usage: benches/scripts/fanout-100k-diag.sh <src-dir> <out-dir> [tier-spec ...]
#   tier-spec = clients,events,window,ack,listeners (default: the two runs below)
set -u
SRC=$1; OUT=$2; shift 2
IMAGE=${NOSTOS_LINUX_IMAGE:-rust:1.98-bookworm}
LOCK=/tmp/nostos-bench.lock
NAME=nostos-linux-fanout-100k
LOG=$OUT/linux-fanout-diag.log
mkdir -p "$OUT"

host_load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
other_container_up() { docker ps --format '{{.Names}}' | grep -q '^nostos-linux-'; }
lock_held() { [ -f "$LOCK" ] && kill -0 "$(cat "$LOCK" 2>/dev/null)" 2>/dev/null; }

in_container() { # args: inner-script args...
  docker run --rm --name "$NAME" \
    -v "$SRC":/src \
    -v nostos-linux-target-fanout:/target \
    -v nostos-linux-cargo-registry-fanout:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target \
    -e RUSTUP_TOOLCHAIN=1.98.0 \
    -e CARGO_INCREMENTAL=0 \
    -e TAG=fanout-diag \
    --ulimit nofile=1048576:1048576 \
    --sysctl net.ipv4.ip_local_port_range="1024 65535" \
    --sysctl net.ipv4.tcp_tw_reuse=1 \
    -w /src "$IMAGE" sh benches/scripts/fanout-100k-diag-inner.sh "$@"
}

# Phase A — build (load-tolerant, no lock).
in_container build >>"$LOG" 2>&1 || { echo "BUILD_EXIT=$?" >>"$LOG"; exit 91; }

# Phase B — measurements, each behind the headroom gate.
tier() { # clients events window ack listeners
  waited=0
  while lock_held || other_container_up || awk -v l="$(host_load1)" 'BEGIN{exit !(l>=8)}'; do
    if [ "$waited" -ge 1800 ]; then
      echo "GATE_TIMEOUT tier=$1 load1=$(host_load1) lock=$(cat "$LOCK" 2>/dev/null) containers=$(docker ps --format '{{.Names}}' | tr '\n' ,)" >>"$LOG"
      return 90
    fi
    sleep 60; waited=$((waited + 60))
  done
  echo $$ >"$LOCK"
  echo "HOST tier=$1 start $(date +%T) load1=$(host_load1) waited=${waited}s" >>"$LOG"
  in_container tier "$1" "$2" "$3" "$4" "$5" >>"$LOG" 2>&1
  rc=$?
  echo "HOST tier=$1 end $(date +%T) load1=$(host_load1) rc=$rc" >>"$LOG"
  rm -f "$LOCK"
  sleep 30  # let the VM drain 100k+ sockets before anyone else measures
  return $rc
}
trap 'rm -f "$LOCK"; docker rm -f "$NAME" >/dev/null 2>&1' EXIT

if [ $# -eq 0 ]; then
  # 100k first (the slow tier, exact original shape: ack=1, 2 listeners) with a
  # short window — the progress line gives the rate without needing completion.
  set -- 100000,500,300,1,2 50000,500,120,1,1
fi
for spec in "$@"; do
  IFS=, read -r c e w a l <<<"$spec"
  tier "$c" "$e" "$w" "$a" "$l"
done
echo "LINUX_DIAG_EXIT=0" >>"$LOG"
