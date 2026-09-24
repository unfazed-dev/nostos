# Linux host, 1k–10k ladder — `unfazed-rog`, 2026-09-22

**Status:** both arms complete. Arm A contended (1k only), Arm B quiet
(1k/5k/10k under a delivery budget).

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
| commit | Arm A `6151a68`, Arm B `107f937`, both `--release` |

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

## Arm B — quiet (2026-09-22)

Same host, desktop closed, `nostos-bench` alone on the machine. Load average
**2.29** at launch (decaying from a `cargo build`) and **7.74** at the end,
which is the benchmark's own 600%-of-800% footprint rather than anything
competing with it.

`--clients 1000,5000,10000 --deliveries 100000000 --reps 3 --warmup-reps 1
--timeout-secs 900`, order seed `54241812205`. Unpaced. 46m23s wall clock for
12 runs (9 measured, 3 warm-ups discarded).

**The 1k tier's flags are identical to Arm A's** — a delivery budget of 100M
frames at 1,000 clients *is* 100,000 events — so the 1k row below is a true
paired arm, not an approximation.

| clients | events/run | ops/sec (trimmed mean of 3) | spread (min–max) | drop% (max) | p50 | p99 | reps |
|---:|---:|---:|---:|---:|---:|---:|:--:|
| 1,000 | 100,000 | **819,565** | 808,494–821,254 | 0.85% | 0.02 ms | 0.09 ms | 3/3 |
| 5,000 | 20,000 | **702,698** | 687,983–704,643 | 10.73% | 0.01 ms | 0.04 ms | 3/3 |
| 10,000 | 10,000 | **226,989** | 226,677–241,906 | 12.64% | 0.00 ms | 0.01 ms | 3/3 |

Every repetition, in execution order (the order `series.svg` plots):

| run | clients | ops/sec | drop% | delivered |
|---:|---:|---:|---:|---:|
| 1 | 5,000 | 704,643 | 1.66% | 98,343,709 |
| 2 | 1,000 | 819,565 | 0.24% | 99,764,638 |
| 3 | 5,000 | 687,983 | 10.73% | 89,266,544 |
| 4 | 10,000 | 226,677 | 2.87% | 97,130,333 |
| 5 | 5,000 | 702,698 | 3.80% | 96,200,465 |
| 6 | 1,000 | 821,254 | 0.85% | 99,149,340 |
| 7 | 10,000 | 226,989 | 2.66% | 97,336,097 |
| 8 | 10,000 | 241,906 | 12.64% | 87,364,922 |
| 9 | 1,000 | 808,494 | 0.75% | 99,249,780 |

`superseded: 0` throughout, same reason as Arm A. **9/9 valid** — no tier hit
the timeout, which is itself the first result below.

### 1. Contention was costing 32%, and the file's own prediction was wrong

1k, same flags, contended → quiet: **561,338 → 819,565 ops/sec, +46%.**

This file previously predicted *"expect the gap to land near the ~20% of CPU
contention was taking, not larger."* It landed at **46%**, so the prediction
under-called it by more than a factor of two. CPU share is not the whole story:
the competing processes were also evicting cache and fragmenting the scheduler,
and neither shows up in a `%CPU` column.

**The spread is the bigger result.** ±22% contended, **±0.8% quiet**
(808,494–821,254 across three reps). Contention did not merely depress the
number, it destroyed the *precision*, which is what made the Arm A figures
unusable for answering any question except "how bad is contention".

### 2. The delivery budget is what made the 10k rung measurable

The previous ladder at a fixed `--events 100000` gave the 10k rung 1B frames to
move and it hit the 1,800 s cap, reporting `TIMED OUT` and nothing else. Under a
100M-frame budget the same tier finishes in ~370–440 s, three times running, at
a **±3% spread**. That is the first real 10k number from this host.

A fixed event count charges the widest tier twice — more sessions per event
*and* more events — so the ladder varies work and width together. Holding
frames constant leaves width as the only variable, which is the whole reason
`--deliveries` exists (`107f937`).

### 3. Equal-work comparison refutes the O(N)-scan hypothesis

`crates/nostos-bench/src/main.rs` blames the 10k regime on `FanOutService::run`
doing a per-event `slowest_session` + `min_acked_lsn` walk over every session —
O(N) per event. The budget lets that be tested directly, because it equalises
the scan work:

| tier | events × sessions | frames moved | ops/sec |
|---|---:|---:|---:|
| 5,000 | 20,000 × 5,000 = **10⁸** | 96,200,465 | 702,698 |
| 10,000 | 10,000 × 10,000 = **10⁸** | 97,336,097 | 226,989 |

**Identical scan work, near-identical frame counts, and 10k is still 3.1×
slower.** So the per-event walk is not the ceiling. The only variable left is
the number of *concurrent* sessions: 10,000 tokio tasks and 10,000 sockets on
4 cores — scheduling and per-session memory, not per-event CPU.

That reframes the open "why does the wide tier collapse" question away from an
algorithmic fix and toward a concurrency one. It does **not** license a claim
about Nostos's server limit: this is still the in-process harness with the
generator co-located, per the second-host requirement in
`docs/plans/measuring-conflation-honestly.md`.

### 4. Throughput got precise; drop rate got noisy

Per-tier drop across the three reps:

| tier | rep drops | ratio |
|---|---|---:|
| 1,000 | 0.24% / 0.85% / 0.75% | 3.5× |
| 5,000 | 1.66% / 3.80% / 10.73% | 6.5× |
| 10,000 | 2.66% / 2.87% / 12.64% | 4.8× |

ops/sec now varies by ~1% within a tier while drop rate varies by up to 6.5×.
The table reports the **max**, which is the conservative choice, but a single
drop figure per tier hides that range and should not be quoted without it.

Two reasons to distrust these drop rates specifically:

- **Shorter runs amplify the connection ramp.** Loss concentrated in the first
  moments of a run — sessions still subscribing while events are already
  flowing — is a fixed absolute cost divided by a smaller denominator. The 5k
  tier read 0.29% over 100k events and 1.66–10.73% over 20k. Nothing got worse;
  the same early loss is a larger fraction of a shorter run.
- **Drop rate is therefore not comparable across `--events` values**, which
  makes it the one figure in this file that must not be compared to the
  previous ladder.

Throughput does not have this problem: it is a rate, and the budget holds the
work constant.

### 5. The series plot says the machine held still

`series.svg` (execution order, not table order) draws three flat lines across
all nine runs — no step, no drift, no tier changing level partway through. That
is the plot earning its keep: it licenses trusting these numbers rather than
suspecting a mid-session thermal or contention change, which is exactly the
doubt the Arm A spread could not resolve.

### What the numbers mean in workload terms

`ops/sec` is deliveries, i.e. `events/sec × clients`. Dividing back out gives
the row-change rate the router actually ingested:

| tier | ops/sec | row-changes/sec into the router | per device |
|---:|---:|---:|---:|
| 1,000 | 819,565 | 820 | 820/sec |
| 5,000 | 702,698 | 141 | 141/sec |
| 10,000 | 226,989 | 23 | 23/sec |

The harness gives **every** client **every** event, so this is maximum fan-out.
A real deployment scoped by tenant (`NOSTOS_TENANT_COLUMN`) fans each row to one
tenant's devices, so the same hardware carries far more total devices at a far
lower fan-out factor.

## Two harness bugs this host found

Neither reproduces on Apple Silicon, and that is exactly why they survived.

**1. A timed-out run reported a number** (fixed, `49eff57`). `elapsed_secs`
came back as `120.002` — the `--timeout-secs` default — and the JSON still
rendered `824,882 ops/sec` and `1.01% drops` as measurements. Predicted for the
10k rung in `docs/plans/measuring-conflation-honestly.md`; found corrupting the
**1k** rung. Runs now carry `throughput_valid`, and an invalid run is withheld
from the table and from every aggregate in `RESULTS.md`.

**2. The wait loop could not finish a lossy run** (fixed, `6151a68`). It waited
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

## Still open on this host

- **Conflation has not been measured here.** Every run above is
  `--distinct-keys 0`, so `superseded: 0` is correct and ADR-0045's overflow has
  had nothing to conflate. Needs its own pass.
- **`--rate` has not been exercised.** Every figure above is unpaced, so they
  answer *"where does it fall over"*, not *"does it hold rate R under 1% loss"*.
  The 5k tier's 141 row-changes/sec is the obvious starting R.
- **The 10k concurrency hypothesis is untested.** Finding 3 narrows the suspect
  to session count; confirming it needs a profile, not another ladder.
