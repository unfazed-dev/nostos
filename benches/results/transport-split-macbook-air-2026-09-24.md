# Bench gate: cairn → nostos rename + transport.rs split — 2026-09-24

Scope: **eval-only** aggregate fan-out (FakeReplicator loopback → real axum/WS server → 1,000 tokio-tungstenite clients).
Not comparable to any full-path (real-PG → client-apply) figure, and not to the 2026-09-02 headline (see Finding 1).

## Trees

| tree | repo @ commit | binary sha256 (first 16) | `__text` bytes |
|---|---|---|---:|
| A cairn | `cairn` @ `074c60f` (detached scratch worktree) | `67d8e04de5b15e6c` | 2,395,400 |
| B nostos main | `nostos/.worktrees/gates` @ `3469b4a` (clean before and after build) | `438455cf5dceddae` | 2,398,080 |
| C transport split | `nostos` @ `785ca68` (detached scratch worktree, see note) | `86d4bacc77caf05e` | 2,398,080 |

Note: `nostos/.worktrees/transport-split` had moved on to `8c7af03` (a one-line doc comment in `transport/session.rs`) when this started.
To build the exact requested commit without touching that worktree, C was built from a detached worktree at `785ca68`.
The two differ by that one comment line, so C is the branch's code.

Build: `CARGO_BUILD_BUILD_DIR=.nostos-scratch/bench-build-<T> cargo build --release -p <cairn|nostos>-bench --bin <…>-bench`,
profile `opt-level=3, lto="fat", codegen-units=1, panic="abort"` (identical in all three `Cargo.toml`s). Binaries copied out to `bench/bin/`
immediately after build and run from there, so later builds in the live worktrees could not clobber them.

## Command (underlying `make bench`, headline config)

```
ulimit -n 1048576
bin/<T>-bench --clients 1000 --events 100000 --out-dir raw/<T><n>
# defaults (identical A/B/C): profile small, buffer 1024, timeout 120 s, rate 0 (unpaced),
# reps 5 + 1 discarded warm-up, order_seed 54241812205. Figure = the bench's own trimmed mean (drop fastest+slowest of 5).
```

Schedule: interleaved `A B C` rounds. Rounds 1–3 were the planned 9 runs. Replacement rounds 4–6 were added because §6.1 marked runs invalid.
They continue until every tree has ≥3 valid runs (cap: 3 extra rounds). Invalid runs are kept and reported here, not re-rolled.

Gates per run (scripts `run.sh`, `run-extra.sh`):
- **Pre-run:** at least 60 s with no `rustc|cargo|clang|ld|ld64|cc|clippy-driver|rustdoc` process. This doubles as a 60 s cool-down between runs.
- **Mid-run:** §6.1 aggregate non-harness CPU ≤150% on ≥95% of about 10 s samples, from `top -l 2` (second block). Harness is the bench binary, `kernel_task` and `top`.
  The Docker VM counts as *other* on native runs.

## Environment

- **Host:** `unfazed-macbook-air.local`, Mac16,13 (MacBook Air 15", **fanless**), Apple M4, 10 cores (4P + 6E), 16 GiB RAM
- **OS:** macOS 27.0 (26A428)
- **Toolchain:** `rustc 1.98.1 (48a229cea 2026-09-01)` (all three trees, `channel = "stable"`)
- **Power:** AC power, battery at 80% and not charging, lowpowermode 0. `pmset -g therm` recorded no thermal or performance warning level (Apple Silicon reports none).
- **fd limit:** `ulimit -n` 1,048,576 (kern.maxfilesperproc 61,440)
- **Load at session start:** load1 8.68 while building, 4.46 at the first run. Start load1 per run was 4.5–13.0, mostly decay from the previous run (see Finding 3).
- **Other load (left alone):**
  - Docker Desktop VM running `cairn-postgres` plus 10 `supabase_*_stack` containers
  - VS Code with 3 Dart language servers and tooling daemons
  - Brave, WindowServer
  - Headroom proxy (python, ~35–42%)
  - Other agents' `git`, `python3.13` and `dart` activity, plus one `clang ×8` burst (during A2) and one `rustc` sighting (22:02:08, A6's final sample, as the run ended)
  - macOS `ANECompilerService` and `mobileassetd`
  - Per-run non-harness CPU mean: 65–120%

## Results — every run, in execution order

| run | order | ops/s (trimmed mean of 5) | 5-rep min–max | reps in exec order (M ops/s) | delivered / attempted | drop % | p50 / p99 max (ms) | other CPU mean / max (%) | viol % | mid-run |
|---|---:|---:|---|---|---|---:|---|---|---:|---|
| A1 | 1 | 1,524,704 | 1,366,639–2,157,924 | 2.16 1.59 1.41 1.37 1.58 | 500,000,000 / 500,000,000 | 0.00 | 0.006 / 0.051 | 98 / 166 | 2.9 | VALID |
| B1 | 2 | 1,453,368 | 1,185,766–1,509,481 | 1.44 1.51 1.19 1.46 1.46 | 500,000,000 / 500,000,000 | 0.00 | 0.007 / 0.078 | 98 / 203 | 5.0 | VALID |
| C1 | 3 | 1,379,144 | 1,226,137–1,484,896 | 1.23 1.38 1.48 1.34 1.42 | 500,000,000 / 500,000,000 | 0.00 | 0.007 / 0.058 | 116 / 192 | 17.5 | invalid |
| A2 | 4 | 1,201,393 | 962,687–1,423,690 | 1.11 1.42 1.34 1.16 0.96 | 500,000,000 / 500,000,000 | 0.00 | 0.007 / 0.061 | 114 / 216 | 11.1 | invalid |
| B2 | 5 | 1,580,272 | 1,477,556–1,818,605 | 1.82 1.67 1.55 1.52 1.48 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.056 | 85 / 148 | 0.0 | VALID |
| C2 | 6 | 1,314,030 | 1,123,158–1,598,806 | 1.22 1.29 1.12 1.43 1.60 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.060 | 108 / 177 | 7.3 | invalid |
| A3 | 7 | 1,502,353 | 1,268,689–1,702,667 | 1.43 1.58 1.27 1.50 1.70 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.056 | 71 / 123 | 0.0 | VALID |
| B3 | 8 | 1,390,793 | 1,159,738–1,679,784 | 1.23 1.16 1.44 1.68 1.50 | 500,000,000 / 500,000,000 | 0.00 | 0.009 / 0.057 | 65 / 119 | 0.0 | VALID |
| C3 | 9 | 1,544,695 | 1,347,686–1,948,521 | 1.95 1.58 1.66 1.40 1.35 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.056 | 75 / 119 | 0.0 | VALID |
| A4 | 10 | 1,312,448 | 1,017,046–1,538,393 | 1.28 1.02 1.18 1.47 1.54 | 500,000,000 / 500,000,000 | 0.00 | 0.007 / 0.059 | 118 / 211 | 11.6 | invalid |
| B4 | 11 | 1,236,046 | 967,781–1,344,733 | 1.24 0.97 1.26 1.21 1.34 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.069 | 120* / 202* | 13.6 | invalid |
| C4 | 12 | 1,489,202 | 1,166,248–1,682,763 | 1.68 1.68 1.62 1.18 1.17 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.059 | 103 / 165 | 10.8 | invalid |
| A5 | 13 | 1,323,067 | 1,113,785–1,573,213 | 1.21 1.11 1.57 1.45 1.31 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.060 | 112 / 207 | 11.9 | invalid |
| B5 | 14 | 1,703,318 | 1,612,097–1,721,332 | 1.69 1.61 1.72 1.71 1.70 | 500,000,000 / 500,000,000 | 0.00 | 0.009 / 0.062 | 80 / 130 | 0.0 | VALID |
| C5 | 15 | 1,706,963 | 1,557,586–2,014,643 | 2.01 1.69 1.78 1.66 1.56 | 500,000,000 / 500,000,000 | 0.00 | 0.009 / 0.059 | 66 / 135 | 0.0 | VALID |
| A6 | 16 | 1,688,636 | 1,408,677–2,392,719 | 2.39 1.93 1.55 1.59 1.41 | 500,000,000 / 500,000,000 | 0.00 | 0.008 / 0.057 | 69 / 131 | 0.0 | VALID |
| B6 | 17 | 1,598,845 | 1,523,000–1,753,416 | 1.61 1.55 1.52 1.64 1.75 | 500,000,000 / 500,000,000 | 0.00 | 0.009 / 0.061 | 78 / 118 | 0.0 | VALID |
| C6 | 18 | 1,726,213 | 1,628,567–2,096,245 | 2.10 1.77 1.70 1.71 1.63 | 500,000,000 / 500,000,000 | 0.00 | 0.010 / 0.061 | 72 / 121 | 0.0 | VALID |

`*` B4: one `top` sample reported `dartvm` at 303,835,578% (a newly spawned process, bogus delta). Mean and max are recomputed without it.
The verdict is unchanged: 5 of 43 samples over 150% (11.6%), so invalid either way.

Every rep in all 18 runs, including the 18 discarded warm-ups (108 reps), delivered 100,000,000 / 100,000,000 with `drop_rate=0.0` and `superseded=0`.
No run timed out and all `throughput_valid=true`. **No drops anywhere.**

## Per-tree summary (mid-run-VALID runs only — the A/B basis)

| tree | valid runs | run figures (ops/s) | **median** | min–max spread (% of median) | drop % | p99 max (ms) |
|---|---|---|---:|---|---:|---:|
| A cairn | A1 A3 A6 | 1,524,704 · 1,502,353 · 1,688,636 | **1,524,704** | 1,502,353–1,688,636 (12.2%) | 0.00 | 0.057 |
| B nostos | B1 B2 B3 B5 B6 | 1,453,368 · 1,580,272 · 1,390,793 · 1,703,318 · 1,598,845 | **1,580,272** | 1,390,793–1,703,318 (19.8%) | 0.00 | 0.078 |
| C split | C3 C5 C6 | 1,544,695 · 1,706,963 · 1,726,213 | **1,706,963** | 1,544,695–1,726,213 (10.6%) | 0.00 | 0.061 |

- **Run-to-run noise:** pooled stdev of the 11 valid run figures is 7.1% (range 1.39M–1.73M). The per-rep stdev inside a tree is 11–19%.
  With n = 3 per arm, the smallest difference this session can resolve at ~2σ is about ±12%. Differences of a few percent cannot be resolved on this host today.
- **B, first 3 valid runs only:** median 1,453,368 (within A's spread too).
- **Paired, fully-valid rounds (3 and 6):**

  | round | B vs A | C vs B |
  |---|---:|---:|
  | 3 | −7.4% | +11.1% |
  | 6 | −5.3% | +8.0% |

  The fixed A→B→C order confounds position with tree, so these pairs are indicative only.
- **All runs including invalid (order-of-magnitude only, §6.1):** medians A 1.41M, B 1.52M, C 1.52M. The invalid runs sit systematically low (1.20–1.49M), which is the contention the gate exists to catch.

## Gates

**Gate 1 (rename, B vs A): PASS.**
- B's median 1,580,272 lies inside A's spread 1,502,353–1,688,636, and A's median lies inside B's spread.
- The Δ of the medians is +3.6%, while the paired fully-valid rounds show B −5% to −7%. The sign flips between views, and both sit inside the ±12% resolution, so this is noise.
- Codegen is not byte-neutral: `__text` is +2,680 B (+0.11%) for B, which is expected from the renamed symbol and string contents.

**Gate 5 (transport split, C vs B): PASS on the noise clause; strictly outside B's spread, on the fast side.**
- C's median 1,706,963 is 3,645 ops/s (0.2%) above B's max of 1,703,318, and B's median lies inside C's spread.
- Δ is +8.0%, about 1.1σ of the measured run-to-run noise. That is within noise, and in any case the direction is faster, not a regression.
- Codegen check: B and C `__text` are the same size (2,398,080 B), and their instruction multisets (mnemonic counts over 599,520 instructions) are **identical**.
  Only the layout order differs (235 diff hunks of permuted instructions), consistent with a pure module move under fat LTO reordering functions.
  Layout-only changes can move a few percent either way. The +8% is **not** a speedup to claim.

**Drops:** 0.00% in all 108 reps. No finding.

## Transport check (step 4)

The headline bench **does** exercise nostos-infra's WS transport:
- `crates/nostos-bench/src/main.rs:46` imports `nostos_infra::transport::{sync_handler, SyncRouterState}` and mounts `sync_handler` on the real axum server that the 1,000 WS clients connect to.
- In C the path is: `transport/handshake.rs` (`sync_handler`) → `transport/session.rs` (`run_session`, the per-frame egress loop) → `transport/dispatch.rs` / `subscribe.rs` (Subscribe).

So the headline covers the split code's hot path, and no separate WS bench was run.

## Findings

1. **The 2026-09-02 headline (2,618,601 ops/s) does not reproduce under today's conditions.** A (cairn itself, same host) gives a valid median of 1.52M (−42%).
   - This is not a rename or split effect: A, B and C all show it.
   - Rep-position means across all 18 runs:
     - warm-up 1.80M
     - rep 1 1.60M
     - reps 2–5 1.46–1.47M (single reps range 0.96M–2.39M)
   - Throughput steps down after about 1–2 min of sustained 10-core load. That fits thermal throttling on the fanless Air, inferred rather than measured (no thermal telemetry).
   - Differences from 09-02:
     - 09-02 ran three single ~40 s passes. §4.1 now runs 5 reps + warm-up, about 6–7 min sustained per invocation.
     - rustc 1.95.0 → 1.98.1 and macOS 26.6.2 → 27.0.
     - Desktop load (VS Code/Dart, Brave, Docker with 11 containers, agents) versus a cleared desktop on 09-02.
   - The published figure should be re-measured under §4.1 on a quiet (ideally actively cooled) host before it is cited again.
2. **Contention from concurrent agents and the desktop invalidated 7 of 18 runs** (C1 A2 C2 A4 B4 C4 A5). Hence the 18-run session (19:55–22:16).
3. **Deviation:** §6.1's START clause (load1 < 8) was not enforced; the 60 s builder-quiet gate was used instead. 5 of the 11 valid runs started at load1 8.3–13.0 (A3 B1 B2 B3 B5), mainly the previous run's decay after a 60 s gap.
   Valid runs that started at load1 < 8:

   | tree | runs | figures |
   |---|---|---|
   | A | A1, A6 | 1.52M, 1.69M |
   | B | B6 | 1.60M |
   | C | C3, C5, C6 | median 1.71M |

   Both gate verdicts are unchanged.

## Artifacts

Raw per-run JSON, mid-run samples and the run scripts stayed on the host and are
not committed; the tables above carry every figure.
