# ADR-0040: Bounded-sink loss windows and slow-client policy

- **Status:** Proposed (pending operator ratification)
- **Date:** 2026-08-24
- **Evidence:** `benches/results/RESULTS.md` §"Real-PG → client-apply end-to-end"
  (2026-08-24); harness
  `crates/nostos-client/tests/e2e_pg_apply_throughput.rs`
- **References:** ADR-0009 (ack-driven resume, snapshot-reconcile fallback),
  ADR-0025 (op-log, opt-in writer), ADR-0018 (tenant enforcement)

## Context

The fan-out contract is drop-on-full: a per-session sink that cannot accept an
event sheds it and counts it (`Metrics.dropped`), never blocking the fan-out
loop. For a *connected* fresh client there is no replay source (the op-log is
opt-in and replay triggers on reconnect/resume only), so within one session a
shed event is lost until disconnect. Three concrete loss windows are now
MEASURED, not hypothetical:

1. **Startup burst vs default buffer.** Production default
   `NOSTOS_SESSION_BUFFER=1024`. An unpaced 20k-row load (40×500-row
   transactions) shed **5,988 events (29.9%)** — `matched=20000,
   delivered=14012`. The client applied 100% of what was delivered; loss is
   purely arrival-rate vs sink depth during the burst.
2. **First-connect snapshot flood.** A fresh subscriber receives a targeted
   snapshot of every published row. On a table with ~28k leftover rows the
   session matched **69,009** events across repeated reconcile waves and never
   converged below continuous shedding.
3. **Any mid-session hiccup** longer than the client's drain deficit has the
   same shape as (1): shed-and-counted, invisible to the client until it
   notices missing rows by other means.

The bounded-buffer contract itself is correct and stays (ADR-0009's honesty:
sheds are counted, never silent). What is missing is a *policy* for the
connected client that loses part of a stream.

## Options

1. **Client-initiated re-subscribe on gap detection.** Client tracks a cheap
   continuity signal (per-table applied LSN monotonicity or periodic
   server-reported `delivered/matched` counters over the wire); on gap,
   drop local state for the table and re-subscribe, reusing the existing
   reconnect/snapshot-reconcile machinery unchanged.
2. **Server-driven resnapshot.** Server detects a session's shed counter
   advancing past a threshold and pushes a snapshot boundary + fresh snapshot
   down the same session. No client change; server does bookkeeping.
3. **Op-log replay for live sessions.** Attach the op-log writer by default
   and let mid-session gaps be back-filled from `cairn_oplog` without any
   resnapshot. Heaviest: changes the op-log's opt-in posture (ADR-0025).
4. **Buffer sizing guidance only.** Document raising
   `NOSTOS_SESSION_BUFFER` (32,768 demonstrated zero-loss on the 20k burst)
   and leave semantics untouched. Mitigates bursts; does nothing for the
   snapshot flood on large tables.

## Decision

Proposed: **Option 1**, with Option 4 documented as immediate operator
guidance. Option 1 reuses proven machinery (snapshot-reconcile already exists
for reconnect), keeps all shedding server-side-countable, and adds no server
state. Detection signal to be pinned at implementation: LSN-gap check on the
client against each frame's checkpoint run.

## Consequences

- Fresh-client large-table sync becomes eventually-correct instead of
  silently-partial (the storm observed in evidence converges instead of
  looping).
- Re-subscribe storms need a hysteresis bound (one re-subscribe per N seconds
  per table) to avoid the observed non-convergent loop becoming a client
  behavior.
- Drop counters remain the honesty surface; the policy converts them from
  "lost" to "reconcile trigger".
- If ratified, implementation lands behind a feature flag with the e2e harness
  extended to assert eventual correctness at buffer 1024.
