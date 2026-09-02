# Throughput re-measure 2026-09-02 — CONTENDED, NOT CITABLE

**Status: aborted after pass 2 (pass 3 killed at start; soaks never ran). Do
not cite any number from this directory as a throughput figure.** The 833,307 ops/sec / 0.00% drop baseline in
`benches/results/RESULTS.md` stands unchanged.

## What was run

- Commit `5c7ca8b` (working tree: 1 dirty file, docs only).
- Config byte-identical to the baseline: `clients=1000 events=100000
  profile=small buffer=1024`, `--release` (lto=fat, codegen-units=1),
  `rustc 1.95.0 (59807616e 2026-04-14)`, Mac16,13, 10 cores, macOS 26.6.2.
- Script: `/tmp/nostos-remeasure.sh` — 3 bench passes + 2 × 10k soak, each
  under `caffeinate -i`. Raw logs in `benches/results/raw/2026-09-02/`.

## Results (both invalid)

| pass | ops/sec | drop% | delivered | load at start | load at end |
|---|---|---|---|---|---|
| 1 | 706,412 | **15.23%** | 84,770,165 | 11.32 | 16.43 |
| 2 | 654,772 | **21.43%** | 78,572,826 | 15.82 | 24.22 |

Pass 2 also tripped the bench's "fan-out didn't finish within 3min grace"
abort path. Drops and load both worsen pass-over-pass — the contention was
growing, not settling.

`docs/BENCHMARK-METHODOLOGY.md` flags >1% drops as not honest throughput.
These drop rates are a scheduler-starvation artifact, not a system property:
the bench is CPU-bound fan-out and the host ran at load 16–24 on 10 cores.

## Why the host was contended

Snapshot at abort (2026-09-02T10:19:40+1000, load 17.13 / 17.99 / 17.28):

- `Code Helper (Plugin)` pid 49568 — 93% CPU during the run, 168% after the
  bench was killed. This is the dominant load.
- Two other `claude --resume` sessions at 33% / 21%.

The four orphan context-mode node procs (47955 47973 73569 73604) from the
previous blocker had already been killed before this run; they were not the
cause here.

## Decision

Aborted during pass 3 (pass 3 and both soaks not run). Further passes under
identical contention cannot disambiguate "host starved" from "real regression";
only a quiet-host rerun can. Advisor consult agreed (confidence 0.85).

## To get a valid number

1. Quiet the host: quit VS Code (or at least the extension host — pid 49568),
   pause other agent sessions, confirm `uptime` 1-min load < 10.
2. Re-run `/tmp/nostos-remeasure.sh` (or `make bench BENCH_CLIENTS=1000
   BENCH_EVENTS=100000` ×3 + `target/release/nostos-bench-10k 10000 5000 60` ×2).
3. Accept a pass only if drop% ≤ 1% per the methodology.

## Follow-up (not done)

- The baseline's host load was never recorded, so a quiet rerun may land
  somewhere other than 833k and would then be a re-baseline, not a confirmation.
- Add a pre-flight gate to `make bench` / the bench binary: record 1-min load
  in `Environment` and refuse (or loudly warn) when it exceeds core count.
