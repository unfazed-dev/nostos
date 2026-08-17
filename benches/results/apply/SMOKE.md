# Client-apply leg — SMOKE fragments (2026-08-17)

**Stage:** N real SyncClients applying FakeReplicator events through the real
/sync WS transport on loopback (rusqlite writes, durable checkpoints, acks).
**This is its own stage, never comparable to the 833,307 ops/sec eval-only
fan-out headline** (different stage, different units — docs/BENCHMARK-METHODOLOGY.md).

**These are SMOKE numbers** — seconds-long runs on a non-quiet machine
(load ~5.5) to prove the leg works end-to-end. Production quiet-window
numbers land in RESULTS.md as a hand-curated MEASURED section.

## Environment

Apple M4 (10 cores), macOS arm64, rustc 1.95.0 (59807616e 2026-04-14),
release profile, 2026-08-17, load avg ~5.5 (NOT a quiet window).

## Commands

    cargo run --release -p nostos-client --example apply_bench -- --clients 8 --events 2000 --out-dir benches/results/apply
    cargo run --release -p nostos-client --example apply_bench -- --clients 8 --events 2000 --on-disk --out-dir benches/results/apply

## Orchestrator-verified runs (re-run by the integrator, not the author)

| profile | events × clients | rows_applied | drops | coarse ops/sec | drain_lag_ms (poll-25ms) | checkpoints |
|---|---|---|---|---|---|---|
| :memory: | 2000 × 8 | 16,000 | 0 (0.00%) | ~105,000 | ~83 | all at final LSN 0/4E17 |
| on-disk (tmpfs-free tempdir files) | 2000 × 8 | 16,000 | 0 (0.00%) | ~35,200 | ~397 | all at final LSN (incl. reopen-from-disk readback) |

The authoring agent's independent smoke runs (run-memory-smoke.json,
run-ondisk-smoke.json) reproduced within ±2% on ops/sec. run.json is the
integrator's on-disk verify run. Router cross-check: matched=delivered=16,000,
dropped=0, faulted=0 on every run.

## Honest readings

- On-disk binds at ~3× below :memory: — SQLite fsync/disk is the apply-stage
  ceiling on this profile, exactly the bound the design sweep predicted.
- drain_lag is coarse (25 ms poll granularity + wall-clock) — a gauge, not an SLA.
- 8 clients is smoke scale; high-N behavior (blocking-pool contention risk
  flagged in the design sweep) is what the production quiet-window run answers.
