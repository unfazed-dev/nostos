#!/usr/bin/env bash
# Userspace IPv6-egress fix for reaching Supabase's IPv6-only direct Postgres
# host (db.<project-ref>.supabase.co:5432) from an IPv4-only / broken-IPv6
# macOS box — WITHOUT sudo, a system extension, or disturbing an existing
# full-tunnel VPN (e.g. a corp VPN holding the default route).
#
# Why this exists: Supabase free-tier direct connections are IPv6-only, and
# logical replication CANNOT go through the pooler (Supabase + PowerSync docs
# both confirm — direct connection required). Many dev networks hand the
# machine a global IPv6 address but don't actually route it ("no route to
# host"). This script gives the box real IPv6 egress via Cloudflare WARP,
# running entirely in userspace.
#
# How: `wgcf` mints a free Cloudflare WARP WireGuard profile; `wireproxy` runs
# it in userspace (gvisor netstack — no root, no macOS system extension, no
# utun contention) and exposes a plain local TCP port that forwards through
# WARP -> Cloudflare edge -> IPv6 -> Supabase. The Postgres replication
# protocol is just TCP, so it rides through transparently.
#
# Proven 2026-07-12 against a real project: nostos's PgReplicator connected
# through 127.0.0.1:<port>, created a logical slot, streamed the pgoutput
# snapshot, delivered live inserts, and LSN-resumed (e2e_pg_replication 3/3,
# e2e_pg_snapshot 2/2). Supabase PG 17.6, wal_level=logical.
#
# Usage:
#   SUPABASE_REF=abc123... ./scripts/warp-ipv6-egress.sh up     # start tunnel
#   NOSTOS_PG_URL=postgresql://postgres:<pw>@127.0.0.1:${LOCAL_PORT}/postgres?sslmode=disable
#   ./scripts/warp-ipv6-egress.sh down                           # stop tunnel
#
# nostos note: nostos's PgReplicator connects with NoTls today, and Supabase's
# direct host accepts plaintext, so use sslmode=disable (or omit sslmode) in
# NOSTOS_PG_URL — sslmode=require would break the NoTls connector.
#
# ponytail: literal IPv6 Target pins the current Supabase host IP. If Supabase
# rotates it, re-resolve `dig +short AAAA db.<ref>.supabase.co` (or run `up`
# again — resolve_target re-resolves each time) and restart.
set -euo pipefail

REF="${SUPABASE_REF:-ltamqsxxumtusyxswezi}"
LOCAL_PORT="${LOCAL_PORT:-15433}"   # 15433 avoids clashing with nostos's local docker PG on 5433
WORKDIR="${WORKDIR:-$HOME/.nostos/warp}"
GOBIN="${GOBIN:-$HOME/.g/go/bin}"
CONF="$WORKDIR/nostos-warp.conf"
PIDFILE="$WORKDIR/wireproxy.pid"

resolve_target() {
  local ip
  ip="$(dscacheutil -q host -a name "db.${REF}.supabase.co" 2>/dev/null \
        | awk '/ipv6_address/{print $2; exit}')"
  echo "[${ip:-2406:da1c:4c7:f801::9907}]:5432"
}

ensure_tools() {
  command -v wgcf >/dev/null || brew install wgcf
  [ -x "$GOBIN/wireproxy" ] || \
    GOBIN="$GOBIN" go install github.com/windtf/wireproxy/cmd/wireproxy@latest
}

ensure_profile() {
  mkdir -p "$WORKDIR"; cd "$WORKDIR"
  [ -f wgcf-account.toml ] || wgcf register --accept-tos
  [ -f wgcf-profile.conf ] || wgcf generate
}

write_conf() {
  local target; target="$(resolve_target)"
  python3 - "$WORKDIR/wgcf-profile.conf" "$CONF" "$LOCAL_PORT" "$target" <<'PY'
import re, sys
src, dst, port, target = sys.argv[1:5]
c = open(src).read()
def g(k):
    m = re.search(rf'^{k}\s*=\s*(.+)$', c, re.M)
    return m.group(1).strip() if m else ''
# NOTE: Address MUST stay comma-separated on ONE line — wireproxy drops the
# IPv6 interface address if Address is split across two lines (=> "no route
# to host" on ALL v6, even though the WG handshake succeeds).
open(dst, 'w').write(f"""[Interface]
Address = {g('Address')}
PrivateKey = {g('PrivateKey')}
DNS = 1.1.1.1
MTU = 1280

[Peer]
PublicKey = {g('PublicKey')}
Endpoint = {g('Endpoint')}
AllowedIPs = 0.0.0.0/0, ::/0
PersistentKeepalive = 25

[Socks5]
BindAddress = 127.0.0.1:25344

[TCPClientTunnel]
BindAddress = 127.0.0.1:{port}
Target = {target}
""")
print(f"wrote {dst}: 127.0.0.1:{port} -> {target}")
PY
}

up() {
  ensure_tools; ensure_profile; write_conf
  nohup "$GOBIN/wireproxy" -c "$CONF" >"$WORKDIR/warp.log" 2>&1 &
  echo $! > "$PIDFILE"
  echo "wireproxy up (pid $(cat "$PIDFILE")); tunnel on 127.0.0.1:${LOCAL_PORT}"
  echo "  NOSTOS_PG_URL=postgresql://postgres:<pw>@127.0.0.1:${LOCAL_PORT}/postgres?sslmode=disable"
  echo "  (SOCKS5 for ad-hoc v6 testing on 127.0.0.1:25344)"
}

down() {
  # Match by binary path, not conf-path: the running process's cmdline may use
  # a relative or absolute conf path; only one wireproxy runs at a time here.
  pkill -f 'bin/wireproxy' 2>/dev/null || true
  rm -f "$PIDFILE"
  echo "wireproxy down"
}

case "${1:-up}" in
  up) up ;;
  down) down ;;
  *) echo "usage: $0 [up|down]  (env: SUPABASE_REF, LOCAL_PORT, WORKDIR, GOBIN)"; exit 1 ;;
esac
