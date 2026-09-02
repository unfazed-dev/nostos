#!/usr/bin/env bash
# Diagnostic for the 100k fan-out cliff (RESULTS.md § "Re-run with progress-based
# quorum": 50k = 0.12 s/event, 100k = 1.22 s/event, zero drops). One container
# run, two tiers, with a VM-level sampler alongside the probe's own `[diag]`
# progress line, so the slow tier can be attributed to one of:
#   kernel TCP memory pressure  -> sockstat `TCP: mem` climbs, TcpExt
#                                  TCPMemoryPressures / PruneCalled / RcvCollapsed advance
#   VM memory reclaim / swap    -> MemAvailable -> 0, SwapFree falls, PSI memory
#                                  `full` climbs, probe `swap_mib` > 0
#   CPU saturation (kernel)     -> /proc/stat sys jiffies dominate, PSI cpu `some` high
#   app-level loop cost         -> none of the above; rate uniform from t=0
# Mirrors benches/scripts/linux-soak.sh but uses its OWN container name and
# volumes so it never fights the ladder scripts' build.
#
# Usage: benches/scripts/fanout-100k-diag.sh <src-dir> <out-dir>
# Bench runs are exclusive on this host: honours /tmp/nostos-bench.lock.
set -u
SRC=$1; OUT=$2
IMAGE=${NOSTOS_LINUX_IMAGE:-rust:1.95-bookworm}
TAG=fanout-diag
mkdir -p "$OUT"

LOCK=/tmp/nostos-bench.lock
waited=0
while [ -f "$LOCK" ] && kill -0 "$(cat "$LOCK" 2>/dev/null)" 2>/dev/null; do
  if [ "$waited" -ge 1800 ]; then echo "LOCK_TIMEOUT holder=$(cat "$LOCK")"; exit 90; fi
  sleep 60; waited=$((waited + 60))
done
echo $$ >"$LOCK"
trap 'rm -f "$LOCK"; docker rm -f nostos-linux-fanout-100k >/dev/null 2>&1' EXIT

docker run --rm --name nostos-linux-fanout-100k \
  -v "$SRC":/src \
  -v nostos-linux-target-fanout:/target \
  -v nostos-linux-cargo-registry-fanout:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  -e RUSTUP_TOOLCHAIN=1.95.0 \
  -e CARGO_INCREMENTAL=0 \
  --ulimit nofile=1048576:1048576 \
  --sysctl net.ipv4.ip_local_port_range="1024 65535" \
  --sysctl net.ipv4.tcp_tw_reuse=1 \
  -w /src "$IMAGE" sh -c '
    set -u
    TAG='"$TAG"'
    BIN=/target/bin-$TAG/nostos-bench-10k
    echo "BUILD start $(date +%T) tag=$TAG"
    cargo build --release --locked -p nostos-bench --bin nostos-bench-10k 2>&1 | tail -3 \
      || { echo BUILD_FAILED; exit 91; }
    mkdir -p "$(dirname "$BIN")" && cp /target/release/nostos-bench-10k "$BIN" || exit 92
    echo "BUILD ok $(date +%T)"
    echo "env: $(uname -srm) nproc=$(nproc) nofile=$(ulimit -n) mem=$(awk "/MemTotal/{print \$2}" /proc/meminfo)kB swap=$(awk "/SwapTotal/{print \$2}" /proc/meminfo)kB tcp_rmem=$(tr "\t" - </proc/sys/net/ipv4/tcp_rmem) tcp_wmem=$(tr "\t" - </proc/sys/net/ipv4/tcp_wmem)"
    # VM-level sampler: sockstat mem/alloc are VM-global, TcpExt is this netns,
    # PSI + meminfo + /proc/stat are VM-global. /proc/stat is cumulative
    # jiffies (user nice sys idle iowait irq softirq) — diff offline.
    sampler() {
      while :; do
        ss=$(awk "/^TCP:/{print \"tcp_alloc=\"\$9\" tcp_mem_pages=\"\$11}" /proc/net/sockstat)
        ext=$(awk "/^TcpExt:/{ if (!h) { h=\$0; n=NF; next } v=\$0 } END { split(h,H); split(v,V); for (i=2;i<=n;i++) if (H[i] ~ /^(PruneCalled|RcvPruned|TCPRcvCollapsed|TCPMemoryPressures|TCPMemoryPressuresChrono|TCPBacklogDrop|TCPAbortOnMemory|TCPRcvQDrop|TCPZeroWindowDrop)\$/) printf \"%s=%s \", H[i], V[i] }" /proc/net/netstat)
        mem=$(awk "/^(MemAvailable|SwapFree|Slab|Dirty):/{printf \"%s%dM \", \$1, \$2/1024}" /proc/meminfo)
        psi="psi_mem=$(awk "NR==2{print \$3}" /proc/pressure/memory 2>/dev/null) psi_cpu=$(awk "NR==1{print \$3}" /proc/pressure/cpu 2>/dev/null)"
        cpu=$(awk "NR==1{print \"cpu_u=\"\$2\" n=\"\$3\" s=\"\$4\" i=\"\$5\" io=\"\$6\" irq=\"\$7\" sirq=\"\$8}" /proc/stat)
        echo "[sys] $(date +%T) $ss $ext$mem$psi load=$(cut -d" " -f1-3 /proc/loadavg | tr " " ,) $cpu"
        sleep 5
      done
    }
    run_tier() {
      sampler & SP=$!
      echo "SOAK start $(date +%T) clients=$1 events=$2 window=$3 ack=$4 listeners=$5"
      "$BIN" "$1" "$2" "$3" "$4" "$5"
      echo "SOAK rc=$? $(date +%T)"
      kill $SP 2>/dev/null; wait $SP 2>/dev/null
      sleep 20  # let 100k+ sockets drain before the next tier
    }
    # 100k first (the slow tier, exact original shape: ack=1, 2 listeners) with a
    # short window — the progress line gives the rate without needing completion.
    run_tier 100000 500 300 1 2
    # 50k control with the same sampler, original shape (1 listener).
    run_tier 50000 500 120 1 1
  ' >"$OUT/linux-fanout-diag.log" 2>&1
echo "LINUX_DIAG_EXIT=$?" >>"$OUT/linux-fanout-diag.log"
