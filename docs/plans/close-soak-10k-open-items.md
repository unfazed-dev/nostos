# Close the 10k-soak open items (2026-09-02)

Follows `docs/plans/soak-10k-root-cause-2026-09-02.md` §Still open. Three items;
each shipped only with before/after numbers (CLAUDE.md "measure before optimize").

Advisor consult: skipped (no API key on this host) — recorded via
`consult.sh gate decision --skip`. Decisions rest on measurements + primary sources.

## Item 1 — per-session `event.clone()` in `FanOutService::fan_out`

**Problem.** `EventSink::deliver(&self, event: ReplicationEvent)` takes the event by
value, so the fan-out loop clones it once per matched session: `RowOp` carries two
`String`s (`table`, `pk`) + a `Bytes` payload → 2 heap allocs + 1 refcount bump per
session per event = 20 000 allocs/event at 10k clients.

**Fix.** `EventSink::deliver(&self, event: Arc<ReplicationEvent>)`. The fan-out loop
allocates one `Arc` per event and hands out refcount bumps. `SinkMsg::Event` carries
the `Arc`; the writer task's batch is `Vec<Arc<ReplicationEvent>>` and encodes through
`&*arc` — the wire codec is untouched (no wire change).

**Measure.** `nostos-bench-10k` A/B, baseline = `e33b4c3`, candidate = this change:
- macOS native: 1k and 5k (10k hits the macOS ENOBUFS limit — item 2).
- Linux (Docker): 10k.
Ship if 10k improves materially and 1k does not regress.

**Result (Linux container, `benches/results/raw/2026-09-02-linux/`).** Shipped.
- 10k / 5000 events / 60 s: baseline 3,596 events fanned out, 35.95M/50M delivered,
  28.09% undelivered, 599,219 ops/sec → fixed **5,000 events, 50M/50M, 0.00%,
  854,631 ops/sec, done in 58.5 s**, router `dropped=0`, quorum 10000/10000 both runs.
- 1k, three back-to-back pairs (ops/sec baseline → fixed): 2.15M → 1.98M
  (5000 events, ~2.4 s runs — mostly connect time), 0.93M → 2.15M and
  1.11M → 1.76M (20000 events, steady-state); all 0 drops. Fixed is 1.6–2.3×
  faster on the longer pairs and within 8% on the short one. No regression.
- macOS native 1k/5k A/B: not run — the host was at load 8–9 and the earlier
  same-day native passes were already thrown out as contended; the Linux 1k pairs
  serve as the no-regression check. Native re-measure is item 3.
- (c) per-session re-encode: **not on the critical path** — 10k cleared the 60 s
  budget with 0% drops without it. Left as a named, dated deferral.

**Deferred with reason.** (c) per-session re-encode in the writer task: needs an
encoded-form cache to travel with the event; application layer would have to carry an
opaque slot for it (hexagonal bend). Decide only after the Arc numbers — if 10k on
Linux clears the 60 s budget with <1% drops, (c) is not on the critical path.

## Item 2 — macOS ENOBUFS at ~9.2k loopback sockets

**Fix.** Run the 10k soak on Linux in Docker (`benches/scripts/linux-soak.sh`):
`rust:1.95-bookworm`, `--ulimit nofile=1048576`, `net.ipv4.ip_local_port_range=1024 65535`,
cargo target on a named volume (bind-mounted target dirs are slow under virtiofs),
`RUSTUP_TOOLCHAIN=1.95.0` so the repo's `rust-toolchain.toml` does not pull the
iOS/Android/wasm targets into the container.

**Caveat to record.** Docker Desktop is a VM (10 vCPU / 8 GiB here). Numbers from it are
a *different environment* from the macOS-native 833 307 figure and are reported only
against each other (baseline vs fixed, same container), never against the native
headline (docs/BENCHMARK-METHODOLOGY.md same-env rule).

## Item 3 — official re-measure on the fixed build

1. macOS native: 3 × `make bench` (1k / 100k events / buffer 1024) + 2 × 10k soak
   (the `/tmp/nostos-remeasure3.sh` recipe, now committed as
   `benches/scripts/remeasure.sh` so a reboot cannot lose it again).
2. Linux container: 2 × 10k soak.
3. Update `benches/results/RESULTS.md` (new dated section), `docs/ROADMAP.md`
   (10k status line), `docs/BENCHMARK-METHODOLOGY.md` (Linux-container environment
   rule), and the root-cause plan §Still open.
4. Headline (README / CLAUDE.md) changes only if the 1k native median moves outside the
   3-pass spread of the 2026-09-02 re-measure.

## Exit criteria

- `make ci` green.
- Every number in the docs has a raw log under `benches/results/raw/<date>/`.
- Nothing in §Still open of the root-cause plan remains without either a fix or a
  named, dated reason.
