# Throughput re-measure 2026-09-02, run 2 — CONTENDED, NOT CITABLE

**Status: aborted after pass 1. Do not cite any number from this directory as a
throughput figure.** The 833,307 ops/sec / 0.00% drop baseline in
`benches/results/RESULTS.md` stands unchanged. Second attempt of the day; the
first is recorded in `../remeasure-2026-09-02/CONTENDED.md`.

## What was run

- Commit `cc00d04` (working tree: 1 dirty file, docs only).
- Config byte-identical to the baseline: `clients=1000 events=100000
  profile=small buffer=1024`, `--release`, `rustc 1.95.0 (59807616e 2026-04-14)`,
  Mac16,13, 10 cores, macOS 26.6.2.
- Launched at 1-min load **5.16** — the quietest the host had been all day.
  Load was back to 17.4 within three minutes of the bench starting.

## Result (invalid)

| pass | ops/sec | drop% | delivered | load at start | load at end |
|---|---|---|---|---|---|
| 1 | 700,366 | **15.95%** | 84,045,313 | 5.16 | 15.35 |

Pass 1 also tripped the bench's "fan-out didn't finish within 3min grace"
abort path. `docs/BENCHMARK-METHODOLOGY.md` flags >1% drops as not honest
throughput.

Note the consistency across the two contended runs (706k / 15.2%, 655k / 21.4%,
700k / 16.0%): the bench behaves the same way under the same starvation. That
is evidence the drops are a host artifact, not a flaky bench — and none of it
is evidence about the server.

## Why the host was contended

- VS Code `Code Helper` pid 5260 — 231% CPU at the 3-minute mark, 74–80%
  thereafter. This is a different helper process from run 1 (pid 49568); the
  editor spawns a new one. The dominant load both times.
- Other agent sessions (`claude --resume`, an npx `node`), `simctl`, `replayd`
  at 20–30% each.

## Decision

Killed the script during pass 2 (pass 2–3 and both soaks not run). Same
reasoning as run 1: more passes under this load cannot produce a citable
number.

## To get a valid number

1. **Quit VS Code entirely** — not just close windows. Its helper process is
   the load source and it respawns. Pause other agent sessions.
2. Confirm `uptime` 1-min load < 5 and that it *stays* there for a minute.
3. Run `sh /tmp/nostos-remeasure2.sh` (after bumping `D=` to a fresh suffix so
   this directory is not overwritten).
4. Accept a pass only if drop% ≤ 1%.
