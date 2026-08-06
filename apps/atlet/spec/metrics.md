# Metrics v0 — Core-4 + storage (pilot)

Clock policy: cross-machine intervals anchor to `server_committed_at`
(Postgres now(), sole authority) minus client wall clock corrected by a per-run
offset estimate (5× PostgREST `select now()` round trips; offset = median of
(server_now - client_mid_rtt)). Client-only intervals use the monotonic clock.

1. cold_sync_ms — signIn→init on wiped device until first watchSessions emission
   matching seeded row count. Report rows/sec alongside (seed size recorded).
2. propagation_ms — harness inserts row via PostgREST; value =
   client_wall(remoteVisible) - server_committed_at + clock_offset. N=25, report
   median/p95. (Amended by task-8 review: the original "- clock_offset" form
   double-counts the offset given clock_offset's sign convention above —
   server-ahead is positive, so recovering true delay from a client-clock
   reading requires adding the offset back, not subtracting it again.)
3. write_ack_ms — tMono(serverAcked) - tMono(addSession). N=25, median/p95.
4. queue_drain_ms — setConnected(false), 25 writes, setConnected(true); value =
   last serverAcked tMono - reconnect tMono.
5. db_bytes — engine's local DB file size(s) after cold sync + full drain, same
   checkpoint state (best effort; journal mode recorded).

Every run records: sdk, engine, profile (local|cloud), seed size, app version,
spec version, device model/os, timestamp. Label everywhere:
"Internal evaluation — not a published benchmark".
