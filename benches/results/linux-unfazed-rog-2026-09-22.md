# Linux host, 1k tier — `unfazed-rog`, 2026-09-22

**Status:** contended run complete. Quiet run pending (same host, desktop idle).

Its own row, deliberately. Not a revision of any figure in `RESULTS.md`, and not
comparable to them: different CPU, different arch, different OS, different
conditions. `docs/BENCHMARK-METHODOLOGY.md` forbids the cross-host absolute
comparison, and the point of this file is to make the two Linux runs comparable
to *each other*.

## Host

| | |
|---|---|
| CPU | Intel i7-7700HQ, 4 cores / 8 threads, base 2.8 GHz (2017 mobile, 45 W) |
| observed clock | 2899 MHz sustained all-core under load |
| RAM | 23 GB |
| OS | Arch (omarchy), kernel 7.2.5-3 |
| governor | `performance` (set for this run; box ships `powersave`) |
| turbo | enabled (`intel_pstate/no_turbo = 0`) |
| power | AC |
| commit | `59af8b7`, `--release` |

## Arm A — contended (2026-09-22)

The box was an **active workstation** during the run. This is deliberate: a
real-world-contention datapoint, paired against a quiet run on the same host.

Competing load, sampled mid-run: VS Code ×6, `java`, `dart:dartdev`, `python`,
`Hyprland`, a second `claude` session. `nostos-bench` took **636%** of 800%
available CPU, so roughly **a fifth of the machine was spoken for**.

`--clients 1000 --events 100000 --timeout-secs 600`, 3 repetitions:

| rep | ops/sec | drop% | p50 | p99 | delivered | elapsed | valid |
|---:|---:|---:|---:|---:|---:|---:|:--:|
| 1 | 561,338 | 0.92% | 0.01 ms | 0.08 ms | 99,082,748 | 176.5 s | ✅ |
| 2 | **768,098** | 0.53% | 0.02 ms | 0.10 ms | 99,470,462 | 129.5 s | ✅ |
| 3 | 524,392 | 1.04% | 0.01 ms | 0.10 ms | 98,964,046 | 188.7 s | ✅ |

**Median 561,338 ops/sec.** MLPerf-style (drop fastest and slowest, mean the
rest, N=3) gives the same 561,338.

**Spread is 524k–768k, ±22% around the median.** That is the contention, and it
is the single most important thing this file records: a lone run from this host
under these conditions would be worth nothing. Any comparison against the quiet
run has to clear that band before it means anything.

`superseded: 0` across all three, and correctly so — `--distinct-keys 0` gives
every event a fresh row, so ADR-0045's overflow has nothing to conflate.
Conflation needs its own pass with `--distinct-keys` set.

### Drops are real here, and at the 1k tier

0.53% / 0.92% / 1.04% — two of three at or over the **1%** bar that
`docs/BENCHMARK-METHODOLOGY.md` calls the honesty threshold. On Apple Silicon
the same tier sheds nothing at all. Whether that survives a quiet run is the
first question the next run answers; if it does, it is a property of 4 cores,
not of contention.

## Two harness bugs this host found

Neither reproduces on Apple Silicon, and that is exactly why they survived.

**1. A timed-out run reported a number** (fixed, `1449664`). `elapsed_secs`
came back as `120.002` — the `--timeout-secs` default — and the JSON still
rendered `824,882 ops/sec` and `1.01% drops` as measurements. Predicted for the
10k rung in `docs/plans/measuring-conflation-honestly.md`; found corrupting the
**1k** rung. Runs now carry `throughput_valid`, and an invalid run is withheld
from the table and from every aggregate in `RESULTS.md`.

**2. The wait loop could not finish a lossy run** (fixed, `59af8b7`). It waited
for `sum_received() >= events × clients` — the count a *loss-free* run receives.
The router may shed on a full session channel, and a shed event never reaches a
client, so one shed made the target unreachable and the loop spun to the
deadline:

| window | delivered | elapsed | reported |
|---|---|---|---|
| 120 s | 98,987,756 | 120.0 s | 824,882 ops/sec |
| 600 s | 99,427,257 | 600.0 s | **165,712 ops/sec** |

Five times the window, 0.4% more work, a 5× "slower" result — both figures were
work ÷ an arbitrary window. The run now ends on **quiescence** (delivery stops
advancing), with the clock stopped at the last delivery so the quiet grace never
enters `elapsed`.

Both bugs are invisible where nothing sheds. The Mac's 1k figures were measured
at 0.00% drops, so `target` was always met and `elapsed` was honest — those
numbers stand.

## Arm B — quiet (pending)

Same host, same commit, same flags, desktop closed. To be filled in.

Expect the gap to land near the ~20% of CPU contention was taking, not larger:
the 2899 MHz observed under load is a thermal/power ceiling on a 45 W 2017 part,
and closing VS Code does not move that.
