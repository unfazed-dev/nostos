#!/usr/bin/env bash
# Watch what the device actually believes, and say so one line at a time.
#
# The device's own SQLite is the honest source: it is what the UI renders from,
# it holds the outbox, and it records why a write was refused. Polling it needs
# no keys and cannot lie about what the user can see. The simulator log
# supplies the one thing SQLite cannot — whether a push actually arrived.
#
# Emits a line only when something CHANGES, so it is safe to leave armed:
#   cart 3 -> 4                         a cart write rendered locally
#   order 9f2c… paid -> shipped         a status change arrived from the server
#   DEAD-LETTER cart_items/9f2c… 42501… a write the server refused, with its error
#   PUSH <text>                         FCM woke the app
#
# ponytail: polling, not triggers. 2s is under human reaction time and the file
# is local; swap to `fswatch` on the -wal if this ever costs anything.
#
# usage: atlet_watch.sh [udid] [bundle-id]
set -uo pipefail

UDID="${1:-0FD740AA-3A37-4365-BC25-7B61603B22C2}"
BUNDLE="${2:-internal.atlet.atlet}"
POLL="${ATLET_WATCH_POLL:-2}"

container=$(xcrun simctl get_app_container "$UDID" "$BUNDLE" data 2>/dev/null)
if [ -z "$container" ]; then
  echo "WATCH-ERROR app $BUNDLE not installed on $UDID"
  exit 1
fi
# Re-resolved every poll, not cached: `simctl install` can hand the app a NEW
# data container, and a path captured at startup then points at a directory
# nothing writes to any more — the watch goes silent, which reads exactly like
# "nothing happened" (caught 2026-09-23, three times in one session).
db_path() {
  local c
  c=$(xcrun simctl get_app_container "$UDID" "$BUNDLE" data 2>/dev/null) || return 1
  [ -n "$c" ] || return 1
  echo "$c/Documents/cairn_direct.sqlite"
}
echo "WATCH-UP polling cairn_direct.sqlite every ${POLL}s + push log"

work=$(mktemp -d)
# By PID, never `kill 0`: this script is meant to be run from another shell,
# and a process-group kill takes that shell down with it.
tail_pid=""
cleanup() { [ -n "$tail_pid" ] && kill "$tail_pid" 2>/dev/null; rm -rf "$work"; }
trap cleanup EXIT INT TERM

# Push receipts: the app logs through Flutter's debugPrint, which lands in the
# unified log as the Runner process. Anchored on the app's own markers and on
# actual delivery, NOT on "FIRMessaging" — Firebase logs a paragraph of
# swizzling chatter at every launch and none of it is a notification. The
# second grep drops what the first still lets through. (And nothing anchors on
# a bare number, so a timestamp reading `.401` cannot masquerade as a 401.)
xcrun simctl spawn "$UDID" log stream --style compact \
  --predicate 'process == "Runner"' 2>/dev/null \
  | grep --line-buffered -E "push pilot|doorbell|order banner|onMessage|didReceiveRemoteNotification|Received remote notification" \
  | grep --line-buffered -vE "proxy enabled|I-FCM001000|swizzl" \
  > "$work/push.log" &
tail_pid=$!
PUSH_LINES=0

snap() {
  # One sqlite3 call, one line per fact, read straight from the live file.
  # NOT a copy: WAL admits concurrent readers, and copying the db and its -wal
  # as two separate steps races the app's own checkpoint — the pair then
  # belongs to two different generations, sqlite3 refuses to open it, and the
  # watch goes SILENT rather than noisy, which reads exactly like "nothing
  # happened" (caught 2026-09-23: seven minutes of missed events, twice).
  local db
  db=$(db_path) || return 1
  sqlite3 "$db" <<'SQL' 2>/dev/null
.timeout 2000
.mode list
.separator |
select 'count','cart',count(*) from cart_items;
select 'count','orders',count(*) from orders;
select 'order',id,status from orders;
select 'dlq',id,table_name||'/'||pk||' '||coalesce(substr(last_error,1,160),'?') from cairn_outbox where dlq=1;
select 'queued','n',count(*) from cairn_outbox where dlq=0;
SQL
}

prev=""
stalled=0
while true; do
  cur=$(snap)
  # Say so when the db stops being readable. An unreadable db and a quiet app
  # look identical from here, and only one of them is good news.
  if [ -z "$cur" ]; then
    [ "$stalled" = 0 ] && echo "WATCH-STALL cannot read the db"
    stalled=1
  elif [ "$stalled" = 1 ]; then
    echo "WATCH-OK db readable again"
    stalled=0
  fi
  if [ -n "$cur" ] && [ "$cur" != "$prev" ]; then
    if [ -z "$prev" ]; then
      # First read is the baseline, not news — report it once, compactly.
      c=$(echo "$cur" | awk -F'|' '$1=="count"{printf "%s=%s ",$2,$3}')
      d=$(echo "$cur" | grep -c '^dlq|')
      echo "BASELINE ${c}dead-letters=$d"
    else
      # comm needs sorted input; the ids make each line unique either way.
      diff <(echo "$prev") <(echo "$cur") | grep '^>' | sed 's/^> //' | while IFS='|' read -r kind a b; do
        case "$kind" in
          count)
            was=$(echo "$prev" | awk -F'|' -v k="$a" '$1=="count"&&$2==k{print $3}')
            [ "$was" != "$b" ] && echo "$a $was -> $b"
            ;;
          order)
            was=$(echo "$prev" | awk -F'|' -v k="$a" '$1=="order"&&$2==k{print $3}')
            if [ -z "$was" ]; then echo "ORDER NEW ${a:0:8}… status=$b"
            else echo "ORDER ${a:0:8}… $was -> $b"; fi
            ;;
          dlq)  echo "DEAD-LETTER $b" ;;
          queued) [ "$b" != "0" ] && echo "outbox queued=$b (unsent)" ;;
        esac
      done
    fi
    prev="$cur"
  fi

  # Drain whatever the push tail collected since the last pass.
  if [ -s "$work/push.log" ]; then
    total=$(wc -l < "$work/push.log" | tr -d ' ')
    if [ "$total" -gt "$PUSH_LINES" ]; then
      tail -n +$((PUSH_LINES + 1)) "$work/push.log" \
        | sed -E 's/.*Runner\[[0-9]+:[0-9a-f]+\] //' | cut -c1-200 \
        | while read -r l; do [ -n "$l" ] && echo "PUSH $l"; done
      PUSH_LINES=$total
    fi
  fi

  sleep "$POLL"
done
