# Scale ladder: 20k → 100k clients + macOS 10k re-check (2026-09-02)

Operator ask: check the macOS 10k re-measure (soak 1 = 64.73% drops, soak 2 = 0.00%),
then measure 20k / 30k / 40k / 50k / 100k clients as soaks on macOS and Linux.

## Hard limits found before running anything (primary sources: `sysctl`, harness source)

| limit | macOS 26.6.2 (this Mac) | Linux container (`rust:1.95-bookworm`, Docker Desktop 10 vCPU / 8 GiB) |
|---|---|---|
| loopback sockets | ~9.2k → `ENOBUFS` (mbuf clusters, `kern.ipc.nmbclusters=131072`, boot-arg only) | none hit at 10k |
| ephemeral ports per (src, dst ip:port) | `net.inet.ip.portrange` 49152–65535 = **16,384** | `ip_local_port_range` 1024–65535 = **64,511** |
| FDs per process (harness is in-process: 2 FDs per client) | `kern.maxfilesperproc=61440` → **≤30k clients** even if sockets allowed | `--ulimit nofile=1048576` |
| listen backlog | `somaxconn=128` | see run log (`somaxconn`) |

Consequences:
- **macOS cannot do 20k+.** Three independent walls (ENOBUFS at ~9.2k, 16k ports, 61k FDs).
  One 20k attempt is run *only* to record the failure mode; 30k–100k are not attempted.
- **Linux 100k needs >1 destination address**: the probe binds a single `127.0.0.1:0`
  listener, so all clients share one 4-tuple space capped at 64,511. Fix: the probe
  accepts a 5th argument `listeners` (default 1) and binds `127.0.0.1..127.0.0.N:0`
  on the same axum app; clients round-robin over the URLs. Linux routes all of
  127/8 to `lo` (verified by binding 127.0.0.2/127.0.0.5 in the container).
- **Memory**: 100k sessions × per-session buffer 1024 (bounded channel, not
  preallocated) in an 8 GiB VM — record RSS at end of each tier from `/proc/self/status`.

## Tier shape (same for every tier so tiers are comparable)

`nostos-bench-10k <clients> 5000 <window> 1 <listeners>` — 5000 events per client (the
10k soak's shape), window 600 s (the probe exits early once everything is delivered;
the window is a ceiling, not the elapsed time). Recorded per tier: connect/subscribe
quorum time, delivered / attempted, drop%, router dropped, not-reached-in-window,
elapsed, ops/sec, RSS. One pass per tier, then a second pass at the largest tier that
finished with <1% drops.

Listeners: 20k–50k → 1 (fits 64,511); 100k → 2.

Advisor consult (2026-09-02, plain shell): two changes adopted — (a) the window is
per-tier, not a flat 600 s: at the 10k rate (854k ops/s) 500M deliveries alone need
~585 s, so windows are 300/400/500/600/1200 s and the probe now prints
`completed=true|false` plus peak RSS so a truncated tier is reported as
"X/N delivered in W s", never as a rate; (b) the macOS 20k attempt is skipped — the
wall is at ~9.2k regardless of the requested count, so it would reproduce the
existing ENOBUFS evidence; the sysctl snapshot above is the record. Not adopted:
median-of-3 per tier (100k alone is ~10–20 min; one pass per tier, second pass at
the top clean tier, stated as such).

## macOS 10k re-check

Question: was soak 1's 64.73% a cold-start artefact (it started 0 s after bench pass 3)
or is macOS 10k inherently flaky? Experiment from an idle host: 3 × `10000 5000 60`
with 60 s gaps, then one bench pass immediately followed by a 4th soak. If soaks 1–3
are clean and soak 4 is not, the answer is "run soaks from idle" (script change:
sleep before the first soak). If any idle soak fails, macOS 10k is inherently
unstable at this socket count and stays not-claimed.

## Sequence (all serial — macOS and the Docker VM share the same 10 cores)

1. Probe change (`listeners` arg) + `cargo clippy` + `make ci` later with the results.
2. macOS: 10k re-check (≈6 min), 20k attempt (seconds).
3. Linux: build tag `ladder`, tiers 20k, 30k, 40k, 50k, 100k (×2 listeners), second pass
   at the top clean tier.
4. RESULTS.md new section (native and container in separate tables, never compared),
   ROADMAP 10k line, this plan's Results section, commit.

## Results — run 2026-09-02 19:21–20:26, commit `4bf9a0d`, `LADDER_EXIT=0`

Full tables in `benches/results/RESULTS.md` § "Scale ladder 20k–100k"; raw logs
`benches/results/raw/2026-09-02-ladder/`.

**Status: measured; ≥30k tiers are harness-capped and must be re-run.**

- Linux 20k: clean and reproduced — 100M/100M, 0.00% drops, 988,044 / 967,548 ops/sec
  (two passes), 912 / 884 MiB RSS, ~101 s each.
- Linux 30k / 40k / 50k / 100k: **no server-side limit found** — router dropped = 0,
  connect_failed = 0, no ENOBUFS, all clients connected by window end, 100k held
  100,000 subscribed sessions at 4,034 MiB RSS. But every tier hit the probe's
  subscribe-quorum cap (`max(30 s, clients/1000 s)`, `probe_10k.rs:170`) with 4–16% of
  clients still connecting (container connect rate ≈ 840–960 conn/s), and `attempted`
  = `clients × events` counts the events fanned out before those clients subscribed as
  drops. Reconciled exactly: `matched == delivered` at every tier; shortfall = late
  clients' missed early events. The 0.62% / 2.05% / 4.33% / 0.11% figures are that
  artefact, and the ops/sec at those tiers is delivered ÷ window (lower bound only).
- macOS 10k re-check answered the question: **not a cold-start artefact.** 2 of 4 soaks
  clean (2.16M / 2.17M ops/sec, 0%), 2 stalled (~80% drops, fan-out loop ~13× slower,
  router shed at the bounded buffers) with idle 2 failing 90 s after a clean idle run.
  macOS 10k stays not-claimed. 1k bench pass in the same run: 2,687,461 ops/sec, 0%.

**Follow-up (open):** make the quorum wait scale with the observed connect rate (or
compute `attempted` from subscribed clients), then re-run 30k–100k un-capped. Until
then no ≥30k rate is quoted anywhere.

Not done from step 4: no ROADMAP edit — it has no dedicated 10k line to update.
