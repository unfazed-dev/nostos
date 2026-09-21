#!/bin/sh
# Build nostos-bench-10k inside a Linux container and run the 10k soak there.
#
# Why: macOS exhausts mbuf clusters (ENOBUFS, os error 55) at ~9.2k loopback
# sockets, so a clean 10k number is impossible natively on a Mac
# (docs/plans/soak-10k-root-cause-2026-09-02.md §Still open item 2). Linux has
# no such limit once nofile + the ephemeral-port range are raised.
#
# Usage: benches/scripts/linux-soak.sh <src-dir> <tag> <out-log> [clients events window ack]
#   src-dir  repo checkout to build (a worktree is fine)
#   tag      short label; the binary is cached under this name in the target volume
#   out-log  file that receives the probe's stderr report
#
# Environment (Docker Desktop on this Mac: 10 vCPU / 8 GiB — a VM, NOT the host;
# never compare these numbers with macOS-native ones, only with each other).
set -u
SRC=$1; TAG=$2; OUT=$3; shift 3
CLIENTS=${1:-10000}; EVENTS=${2:-5000}; WINDOW=${3:-60}; ACK=${4:-1}; LISTENERS=${5:-1}
IMAGE=${NOSTOS_LINUX_IMAGE:-rust:1.98-bookworm}

docker run --rm --name "nostos-linux-soak-$TAG" \
  -v "$SRC":/src \
  -v nostos-linux-target:/target \
  -v nostos-linux-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target \
  -e RUSTUP_TOOLCHAIN=1.98.0 \
  -e CARGO_INCREMENTAL=0 \
  --ulimit nofile=1048576:1048576 \
  --sysctl net.ipv4.ip_local_port_range="1024 65535" \
  --sysctl net.ipv4.tcp_tw_reuse=1 \
  -w /src "$IMAGE" sh -c '
    set -u
    TAG='"$TAG"'
    BIN=/target/bin-$TAG/nostos-bench-10k
    if [ ! -x "$BIN" ]; then
      echo "BUILD start $(date +%T) tag=$TAG"
      cargo build --release --locked -p nostos-bench --bin nostos-bench-10k 2>&1 | tail -3 \
        || { echo BUILD_FAILED; exit 91; }
      mkdir -p "$(dirname "$BIN")" && cp /target/release/nostos-bench-10k "$BIN" || exit 92
      echo "BUILD ok $(date +%T)"
    fi
    echo "env: $(uname -srm) nproc=$(nproc) nofile=$(ulimit -n) ports=$(cat /proc/sys/net/ipv4/ip_local_port_range | tr "\t" -) mem=$(awk "/MemTotal/{print \$2}" /proc/meminfo)kB rustc=$(rustc --version)"
    echo "SOAK start $(date +%T) clients='"$CLIENTS"' events='"$EVENTS"' window='"$WINDOW"' ack='"$ACK"' listeners='"$LISTENERS"' somaxconn=$(cat /proc/sys/net/core/somaxconn)"
    "$BIN" '"$CLIENTS"' '"$EVENTS"' '"$WINDOW"' '"$ACK"' '"$LISTENERS"'
    echo "SOAK rc=$? $(date +%T)"
  ' >"$OUT" 2>&1
echo "LINUX_SOAK_EXIT=$?" >>"$OUT"
