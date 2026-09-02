#!/bin/sh
# Runs INSIDE the Linux container (see fanout-100k-diag.sh). Two modes:
#   build                                   -> build nostos-bench-10k into /target/bin-$TAG
#   tier <clients> <events> <window> <ack> <listeners>
#         -> run one probe tier with a VM-level [sys] sampler every 5 s
set -u
TAG=${TAG:-fanout-diag}
BIN=/target/bin-$TAG/nostos-bench-10k

case "$1" in
build)
  echo "BUILD start $(date +%T) tag=$TAG"
  cargo build --release --locked -p nostos-bench --bin nostos-bench-10k 2>&1 | tail -3 \
    || { echo BUILD_FAILED; exit 91; }
  mkdir -p "$(dirname "$BIN")" && cp /target/release/nostos-bench-10k "$BIN" || exit 92
  echo "BUILD ok $(date +%T)"
  ;;
tier)
  shift
  [ -x "$BIN" ] || { echo "NO_BINARY $BIN"; exit 93; }
  echo "env: $(uname -srm) nproc=$(nproc) nofile=$(ulimit -n) mem=$(awk '/MemTotal/{print $2}' /proc/meminfo)kB swap=$(awk '/SwapTotal/{print $2}' /proc/meminfo)kB tcp_rmem=$(tr '\t' - </proc/sys/net/ipv4/tcp_rmem) tcp_wmem=$(tr '\t' - </proc/sys/net/ipv4/tcp_wmem)"
  # VM-level sampler. sockstat alloc/mem are VM-global; TcpExt is this netns;
  # PSI, meminfo and /proc/stat are VM-global. /proc/stat is cumulative
  # jiffies (user nice sys idle iowait irq softirq) — diff offline.
  sampler() {
    while :; do
      ss=$(awk '/^TCP:/{print "tcp_alloc="$9" tcp_mem_pages="$11}' /proc/net/sockstat)
      ext=$(awk '/^TcpExt:/{ if (!h) { h=$0; n=NF; next } v=$0 }
        END { split(h,H); split(v,V);
          for (i=2;i<=n;i++) if (H[i] ~ /^(PruneCalled|RcvPruned|TCPRcvCollapsed|TCPMemoryPressures|TCPMemoryPressuresChrono|TCPBacklogDrop|TCPAbortOnMemory|TCPRcvQDrop|TCPZeroWindowDrop)$/) printf "%s=%s ", H[i], V[i] }' /proc/net/netstat)
      mem=$(awk '/^(MemAvailable|SwapFree|Slab|Dirty):/{printf "%s%dM ", $1, $2/1024}' /proc/meminfo)
      psi="psi_mem=$(awk 'NR==2{print $3}' /proc/pressure/memory 2>/dev/null) psi_cpu=$(awk 'NR==1{print $3}' /proc/pressure/cpu 2>/dev/null)"
      cpu=$(awk 'NR==1{print "cpu_u="$2" n="$3" s="$4" i="$5" io="$6" irq="$7" sirq="$8}' /proc/stat)
      echo "[sys] $(date +%T) $ss $ext$mem$psi load=$(cut -d' ' -f1-3 /proc/loadavg | tr ' ' ,) $cpu"
      sleep 5
    done
  }
  sampler & SP=$!
  echo "SOAK start $(date +%T) clients=$1 events=$2 window=$3 ack=$4 listeners=$5"
  "$BIN" "$1" "$2" "$3" "$4" "$5"
  echo "SOAK rc=$? $(date +%T)"
  kill "$SP" 2>/dev/null; wait "$SP" 2>/dev/null
  ;;
*)
  echo "usage: $0 build | tier <clients> <events> <window> <ack> <listeners>"; exit 2 ;;
esac
