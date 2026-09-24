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
#   MID-RUN check — aggregate non-harness CPU <= 150% (1.5 of 10 cores) on at
#                   least 95% of the 10 s samples. NOT load1: the 10-vCPU harness
#                   VM alone adds ~4-5 while fanning out, so load1 < 8 for a whole
#                   run is unreachable by design. load1 is recorded alongside but
#                   never invalidates on its own.
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
#
# `com.apple.Virt` is not optional — Docker Desktop on Apple Silicon runs the VM
# under Apple's Virtualization.framework, so the process burning 574% during a
# fan-out is named `com.apple.Virtualization...`, NOT `com.docker.*`. A first cut
# of this list omitted it and killed a perfectly good tier 26 s in.
# `kernel_task` is harness too: at 100k sockets it is doing the harness's I/O.
# The cost is that thermal throttling no longer shows up here — the VM's own
# `[sys]` PSI and idle counters are where to look for that.
#
# Names are matched as PREFIXES because macOS `top` truncates COMMAND to the
# column width (`com.apple.Virtualization.VirtualMachine` prints as
# `com.apple.Virtua`), so a pattern longer than ~16 chars can never match.
HARNESS_RE='^(com\.docker|com\.apple\.Virt|docker|Docker|qemu|vpnkit|hyperkit|kernel_task|top|sysctl)'

# Mid-run thresholds, recalibrated 2026-09-21 — see docs/BENCHMARK-METHODOLOGY.md
# § 6.1 for the derivation. The rule these replace ("no single non-harness
# process above 20% CPU") could not be met by any machine with a screen on:
# macOS `top` reports 100% = ONE core, so 20% is 2% of this 10-core host, while
# the harness VM itself legitimately runs at 574%. It also measured the wrong
# thing — ten processes at 15% starve the VM more than one at 30%, and the old
# rule caught only the second. Aggregate is what steals cores, so aggregate is
# what is measured.
OTHER_LIMIT=150   # % of one core, summed over all non-harness processes
VIOL_PCT_LIMIT=5  # % of a tier's samples allowed to exceed it

# stdin: `top -l 2 -stats cpu,command` output. stdout: "<total> <worst> <cmd>" —
# aggregate non-harness CPU, plus the single worst offender for diagnosis. The
# first block must be discarded: macOS `top` reports a since-launch average
# there, not an instantaneous one, which is why `ps -o %cpu` cannot do this job.
top_other_total() {
  awk -v re="$HARNESS_RE" '
    /^%CPU/ { c = 1; tot = 0; best = 0; who = "-"; next }
    c && NF >= 2 {
      cmd = $2; for (i = 3; i <= NF; i++) cmd = cmd " " $i
      if (cmd !~ re) { tot += $1 + 0; if ($1 + 0 > best) { best = $1 + 0; who = cmd } }
    }
    END { printf "%.1f %.1f %s\n", tot, best, who }'
}

# $1 = a host-sampler log. A tier is INVALID when more than VIOL_PCT_LIMIT of its
# samples exceed OTHER_LIMIT — proportional, because the harm from contention is
# time-integrated: one 10 s blip in a 300 s tier is 3% of the run, not a reason
# to discard it. `other_mean` is reported so future analysis can regress
# throughput on a continuous covariate instead of on end-of-run load1, which is
# partly *caused* by throughput. n == 0 is invalid: a tier with no contention
# evidence is not a measurement.
midrun_verdict() {
  awk -v lim="$OTHER_LIMIT" -v pctlim="$VIOL_PCT_LIMIT" '
    /^\[host\]/ && /other_total=/ {
      n++
      split($0, a, "other_total="); split(a[2], b, " "); t = b[1] + 0
      sum += t; if (t > mx) mx = t; if (t > lim) v++
      split($0, c, "load1=");       split(c[2], d, " "); if (d[1] + 0 > l1) l1 = d[1] + 0
    }
    END {
      pct = n ? 100 * v / n : 100
      printf "samples=%d viol=%d viol_pct=%.1f other_mean=%.0f other_max=%.0f load1_max=%.2f valid=%s",
        n, v + 0, pct, n ? sum / n : 0, mx, l1, (n > 0 && pct <= pctlim) ? "yes" : "no"
    }' "$1"
}

if [ "${1:-}" = "--self-test" ]; then
  # 574 Apple-VZ + 412 docker + kernel_task are harness; 31.7 + 22.0 + 5.0 are not.
  got=$(printf '%s\n' \
    'Processes: 700 total, 2 running' '%CPU COMMAND' '99.0 WindowServer' '90.0 com.docker.backe' \
    'PhysMem: 15G used (2622M wired), 148M unused.' 'Load Avg: 2.34, 3.45, 4.56' \
    '%CPU COMMAND' '574.0 com.apple.Virtua' '412.3 com.docker.backe' '25.5 kernel_task' \
    '31.7 Code Helper (Ren' '22.0 Brave Browser' '5.0 Spotlight' '5.3 top' \
    | top_other_total)
  [ "$got" = "58.7 31.7 Code Helper (Ren" ] \
    || { echo "self-test FAILED: top_other_total gave '$got', want '58.7 31.7 Code Helper (Ren'"; exit 1; }
  echo "self-test ok: top_other_total '$got' (harness + first block ignored, rest summed)"

  fix=$(mktemp)
  # Row 2 is 2026-09-02 attempt 2 (WindowServer 40 + VS Code 61 + Google 39 +
  # ProtonVPN 29 + node 24 + secd 30) — the one run we have a human INVALID
  # verdict for. Rows 1/3 are this desktop idling. Any recalibration must still
  # reject row 2 and still accept rows 1 and 3.
  printf '%s\n' \
    '[host] 22:34:26 tier=100000 load1=4.39 other_total=81.0 other_max=30.0 cmd=WindowServer' \
    '[host] 22:34:37 tier=100000 load1=11.20 other_total=223.0 other_max=61.0 cmd=Code Helper' \
    '[host] 22:34:48 tier=100000 load1=9.10 other_total=75.4 other_max=28.1 cmd=WindowServer' >"$fix"
  got=$(midrun_verdict "$fix"); rm -f "$fix"
  # 1 of 3 samples = 33% > 5% => invalid. load1 hit 11.20 and that alone must not decide.
  [ "$got" = "samples=3 viol=1 viol_pct=33.3 other_mean=126 other_max=223 load1_max=11.20 valid=no" ] \
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
    -e RUSTUP_TOOLCHAIN=1.98.1 \
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
    read -r tot best who <<<"$(top -l 2 -n 25 -stats cpu,command -o cpu 2>/dev/null | top_other_total)"
    echo "[host] $ts tier=$1 load1=$l1 other_total=$tot other_max=$best cmd=$who"
    if awk -v t="$tot" -v lim="$OTHER_LIMIT" 'BEGIN{exit !(t > lim)}'; then
      streak=$((streak + 1))
    else
      streak=0
    fi
    if [ "$streak" -ge 3 ]; then
      echo "[host] $ts CONTENDED tier=$1 — 3 consecutive samples over ${OTHER_LIMIT}% aggregate non-harness CPU, killing the run"
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
  mark=$(wc -l <"$LOG")
  host_sampler "$1" >"$samp" 2>&1 & hs=$!
  echo "HOST tier=$1 start $(date +%T) load1=$(host_load1) waited=${waited}s samples=$(basename "$samp")" >>"$LOG"
  in_container tier "$1" "$2" "$3" "$4" "$5" >>"$LOG" 2>&1
  rc=$?
  kill "$hs" 2>/dev/null; wait "$hs" 2>/dev/null
  verdict=$(midrun_verdict "$samp")
  [ -f "$OUT/.contended" ] && rc=89
  # The other half of "when does a number count" (§ 5): the headline figure is
  # the highest throughput at <1% drops, and a throughput with a high drop rate
  # is meaningless. That was written down and unenforced too — every tier of the
  # two 2026-09-21 ack-coalescing runs dropped between 7.9% and 95.7%, and their
  # ops/sec went into an A/B anyway. A tier that drops is still diagnostically
  # useful; it is just not a throughput measurement, and now says so.
  drop=$(tail -n +$((mark + 1)) "$LOG" | awk -F: '/^ *drop% *:/ { gsub(/ /, "", $2); d = $2 } END { print (d == "" ? "?" : d) }')
  tput=no
  case "$verdict" in *valid=yes*) awk -v d="$drop" 'BEGIN{exit !(d != "?" && d < 1)}' && tput=yes ;; esac
  echo "HOST tier=$1 end $(date +%T) load1=$(host_load1) rc=$rc MIDRUN $verdict drop_pct=$drop throughput_valid=$tput" >>"$LOG"
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
