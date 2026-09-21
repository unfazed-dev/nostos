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
# Headroom rule (docs/BENCHMARK-METHODOLOGY.md § 6.1):
#   START gate    — a BUILD may run under any host load; a MEASUREMENT only
#                   starts when host load1 < 8 (0.8 x 10 cores), no other
#                   nostos-linux-* container is up, and /tmp/nostos-bench.lock
#                   is free.
#   MID-RUN check — no non-harness process > 20% CPU on any 10 s sample. NOT
#                   load1: the 10-vCPU harness VM alone adds ~4-5 while fanning
#                   out, so load1 < 8 for a whole run is unreachable by design.
#                   load1 is recorded alongside but never invalidates on its own.
#
# The mid-run half was written down on 2026-09-02, enforced by hand, and then
# not enforced at all: the ack-coalescing replication (2026-09-21) passed the
# start gate and ran tiers at load1 34.21 and 22.64, and those tiers went into
# an A/B. Both halves are mechanical now. Every tier ends with a
# `MIDRUN samples=.. violations=.. load1_max=.. valid=yes|no` verdict, and the
# run's exit line carries the worst tier rc instead of a hardcoded 0.
#
# Usage: benches/scripts/fanout-100k-diag.sh <src-dir> <out-dir> [tier-spec ...]
#   tier-spec = clients,events,window,ack,listeners (default: the two runs below)
#          or: benches/scripts/fanout-100k-diag.sh --self-test
set -u

# Processes that ARE the harness: the Docker Desktop VM doing the fan-out is
# expected at 400%+, and so is the sampler itself. Everything else is contention.
HARNESS_RE='^(com\.docker|docker|Docker|qemu|vpnkit|hyperkit|top|sysctl)'

# stdin: `top -l 2 -stats cpu,command` output. stdout: "<cpu> <command>" for the
# busiest NON-harness process in the LAST sample block. The first block must be
# discarded — macOS `top` reports a since-launch average there, not an
# instantaneous one, which is why `ps -o %cpu` cannot be used for this at all.
top_worst_other() {
  awk -v re="$HARNESS_RE" '
    /^%CPU/ { c = 1; best = 0; who = "-"; next }
    c && NF >= 2 {
      cmd = $2; for (i = 3; i <= NF; i++) cmd = cmd " " $i
      if (cmd !~ re && $1 + 0 > best) { best = $1 + 0; who = cmd }
    }
    END { printf "%.1f %s\n", best, who }'
}

# $1 = a host-sampler log. Applies the rule as written: ANY sample with a
# non-harness process over 20% invalidates the tier. load1 is reported but never
# decides — the harness VM alone adds ~4-5. n == 0 is invalid too: a tier with no
# contention evidence is not a measurement.
midrun_verdict() {
  awk '/^\[host\]/ && /top_other=/ {
      n++
      split($0, a, "top_other="); split(a[2], b, " "); if (b[1] + 0 > 20) v++
      split($0, c, "load1=");     split(c[2], d, " "); if (d[1] + 0 > mx) mx = d[1] + 0
    }
    END { printf "samples=%d violations=%d load1_max=%.2f valid=%s", n, v + 0, mx, (n > 0 && v + 0 == 0) ? "yes" : "no" }' "$1"
}

if [ "${1:-}" = "--self-test" ]; then
  got=$(printf '%s\n' \
    'Processes: 700 total, 2 running' '%CPU COMMAND' '99.0 WindowServer' '90.0 com.docker.backe' \
    'PhysMem: 15G used (2622M wired), 148M unused.' 'Load Avg: 2.34, 3.45, 4.56' \
    '%CPU COMMAND' '412.3 com.docker.backe' '31.7 Code Helper (Ren' '22.0 Brave Browser' '5.3 top' \
    | top_worst_other)
  [ "$got" = "31.7 Code Helper (Ren" ] \
    || { echo "self-test FAILED: top_worst_other gave '$got', want '31.7 Code Helper (Ren'"; exit 1; }
  echo "self-test ok: top_worst_other picked '$got' (harness + first block ignored)"

  fix=$(mktemp)
  printf '%s\n' \
    '[host] 22:34:26 tier=100000 load1=4.39 top_other=12.0 Brave Browser' \
    '[host] 22:34:37 tier=100000 load1=11.20 top_other=38.5 WindowServer' \
    '[host] 22:34:48 tier=100000 load1=9.10 top_other=3.2 mds' >"$fix"
  got=$(midrun_verdict "$fix"); rm -f "$fix"
  # load1 hit 11.20 and that alone must NOT invalidate; the 38.5% sample must.
  [ "$got" = "samples=3 violations=1 load1_max=11.20 valid=no" ] \
    || { echo "self-test FAILED: midrun_verdict gave '$got'"; exit 1; }
  echo "self-test ok: midrun_verdict '$got'"
  exit 0
fi

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
    -e NOSTOS_PROBE_SKIP_DECODE="${NOSTOS_PROBE_SKIP_DECODE:-}" \
    --ulimit nofile=1048576:1048576 \
    --sysctl net.ipv4.ip_local_port_range="1024 65535" \
    --sysctl net.ipv4.tcp_tw_reuse=1 \
    -w /src "$IMAGE" sh benches/scripts/fanout-100k-diag-inner.sh "$@"
}

# Phase A — build (load-tolerant, no lock).
in_container build >>"$LOG" 2>&1 || { echo "BUILD_EXIT=$?" >>"$LOG"; exit 91; }

# Phase B — measurements, each behind the headroom gate.

# Samples host contention every ~10 s for the life of a tier. A tier is INVALID
# on a single violating sample (that is the rule as written), but is only KILLED
# after 3 consecutive ones: `top` samples are noisy and a lone Spotlight spike is
# not worth discarding 20 minutes of fan-out over — it is worth flagging.
host_sampler() { # $1 = tier label
  streak=0
  while :; do
    ts=$(date +%T); l1=$(host_load1)
    worst=$(top -l 2 -n 15 -stats cpu,command -o cpu 2>/dev/null | top_worst_other)
    echo "[host] $ts tier=$1 load1=$l1 top_other=$worst"
    if awk -v w="${worst%% *}" 'BEGIN{exit !(w > 20)}'; then
      streak=$((streak + 1))
    else
      streak=0
    fi
    if [ "$streak" -ge 3 ]; then
      echo "[host] $ts CONTENDED tier=$1 — 3 consecutive samples over 20% non-harness CPU, killing the run"
      : >"$OUT/.contended"
      docker rm -f "$NAME" >/dev/null 2>&1
      return
    fi
    sleep 10
  done
}

run_tier() { # clients events window ack listeners -> rc (89 = killed for contention)
  rm -f "$OUT/.contended"
  samp=$OUT/host-cpu-tier$1-$(date +%H%M%S).log
  host_sampler "$1" >"$samp" 2>&1 & hs=$!
  echo "HOST tier=$1 start $(date +%T) load1=$(host_load1) waited=${waited}s samples=$(basename "$samp")" >>"$LOG"
  in_container tier "$1" "$2" "$3" "$4" "$5" >>"$LOG" 2>&1
  rc=$?
  kill "$hs" 2>/dev/null; wait "$hs" 2>/dev/null
  verdict=$(midrun_verdict "$samp")
  [ -f "$OUT/.contended" ] && rc=89
  echo "HOST tier=$1 end $(date +%T) load1=$(host_load1) rc=$rc MIDRUN $verdict" >>"$LOG"
  return $rc
}

tier() { # clients events window ack listeners
  attempt=1
  while :; do
    waited=0
    while lock_held || other_container_up || awk -v l="$(host_load1)" 'BEGIN{exit !(l>=8)}'; do
      if [ "$waited" -ge 1800 ]; then
        echo "GATE_TIMEOUT tier=$1 load1=$(host_load1) lock=$(cat "$LOCK" 2>/dev/null) containers=$(docker ps --format '{{.Names}}' | tr '\n' ,)" >>"$LOG"
        return 90
      fi
      sleep 60; waited=$((waited + 60))
    done
    echo $$ >"$LOCK"
    run_tier "$@"; rc=$?
    rm -f "$LOCK"
    sleep 30  # let the VM drain 100k+ sockets before anyone else measures
    [ -f "$OUT/.contended" ] || return $rc
    # One re-arm, matching the 2026-09-02 precedent ("no third re-arm"): a retry
    # loop under sustained desktop load would spin all night for nothing.
    if [ "$attempt" -ge 2 ]; then
      echo "CONTENDED_GIVEUP tier=$1 after 2 attempts" >>"$LOG"
      return 89
    fi
    attempt=2
    echo "RE-ARM tier=$1 attempt=2 after host contention" >>"$LOG"
  done
}
trap 'rm -f "$LOCK"; docker rm -f "$NAME" >/dev/null 2>&1' EXIT

if [ $# -eq 0 ]; then
  # 100k first (the slow tier, exact original shape: ack=1, 2 listeners) with a
  # short window — the progress line gives the rate without needing completion.
  set -- 100000,500,300,1,2 50000,500,120,1,1
fi
worst=0
for spec in "$@"; do
  IFS=, read -r c e w a l <<<"$spec"
  tier "$c" "$e" "$w" "$a" "$l"
  rc=$?
  [ "$rc" -gt "$worst" ] && worst=$rc
done
# Was hardcoded to 0, so a gate timeout or a contended tier still reported a
# clean run — and downstream analysis read LINUX_DIAG_EXIT=0 as "these numbers
# are usable". It carries the worst tier rc now. 89 = contention, 90 = gate.
echo "LINUX_DIAG_EXIT=$worst" >>"$LOG"
exit "$worst"
