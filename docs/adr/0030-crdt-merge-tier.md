# ADR-0030: CRDT merge tier — add-wins set + server-serialized counter

**Status:** Proposed — Decision 1 (counter is NOT a CRDT) overturns the workstream's original
"implement CRDT for the counter" framing and awaits operator ratification.
**Date:** 2026-07-31. **References:** ADR-0004 (conflict tiers), ADR-0014 (merge, Phase-4 debt),
ADR-0013 (server-authoritative write-back), [`RESULTS.md`](../../benches/results/RESULTS.md),
[`BENCHMARK-METHODOLOGY.md`](../BENCHMARK-METHODOLOGY.md), the [plan](../plans/multi-sdk-pomodoro-fixture-matrix.md).

## Context

ADR-0004 declares three conflict tiers; ADR-0014 ships only (a) last-write-wins and defers (b)
CRDT-per-field and (c) custom merge to "Phase 4" (`nostos-core/src/lib.rs:30`). Verified 2026-07-31
against source: the wire carries **no causal metadata** — `WireFrame = {lsn, op, table, pk, payload,
txn_id}` (`crates/nostos-infra/src/wire.rs:20-30`); `txn_id` is a Postgres transaction grouping id
(`crates/nostos-domain/src/events.rs:136`, `pg.rs:19`), NOT causal/replica metadata. The merge is a
blind per-row LWW upsert gated only by `applied_lsn <= excluded.applied_lsn`
(`crates/nostos-client/src/sqlite.rs:561-569`). Grep for hlc / version_vector / replica_id across
shipping code returns zero hits.

## Decision — the "implement CRDT" framing was partly wrong

Split by data shape:

1. **Counter → NOT a CRDT.** nostos is server-authoritative: every write funnels through
   `WriteBack → Postgres` (ADR-0013), server-gated by `NOSTOS_WRITE_TABLES`. The lost-update
   ("two concurrent +1 → +1") exists only because today the client read-modify-writes the full
   value. An `op: "increment"` write whose payload is `{field, delta}`, which `WriteBack` translates
   to `UPDATE ... SET val = val + ?`, lets **Postgres serialize** both increments → +2. Zero
   wire-metadata cost. A PN-counter's per-replica split is warranted only for peer-to-peer client
   merge, which nostos has no path for.
2. **Add-wins OR-set → genuine CRDT** (community tags / presence). Concurrent add "x" + remove "x"
   cannot be serialized correctly by LWW-by-commit-order. Shape per element: `{h: <add-hlc>,
   d: <remove-hlc|null>}`; merge keeps iff add-hlc > remove-hlc (concurrent → add wins).
3. **Timer → stays LWW** (a register; CRDT would be wrong).
4. **Causal metadata = HLC, not version vector.** A version vector is one entry per replica per op;
   nostos's model implies 1k–10k clients → O(10³–10⁴) entries per frame → kilobytes of JSON on a small
   row → hot-path death. HLC is O(1), ~16–26 B/op, minted by the server at write-back commit (no
   client clock sync).

## Wire + benchmark impact (the load-bearing claim)

- The **833,307 ops/sec @ 1k clients @ 0.00% drops** hot path is server→client fan-out
  (`FakeReplicator → router → WS`), measured by `nostos-bench`; it does not apply rows or run
   write-back.
- The counter delta-op lives on **client→server Write** (off the measured path) and replicates back
  as an ordinary row update — same frame size. Impact: none.
- OR-set metadata embeds per-element HLC **inside the existing opaque payload blob**; `WireFrame` is
  unchanged, so serde serialization (the measured cost) is byte-identical; only payload bytes grow
  (~24 B/element). Impact on 833k@1k with today's small-row workload: ~0.
- **Regression risk materializes ONLY if top-level wire fields are added or the bench payload is
  fattened — both avoidable.** Adding either is itself a decision that must clear this gate.

## Benchmark gate (D7 — binding)

Before/after `make bench --clients 1000`, clean `--release`, 3× median, same machine / toolchain /
`ulimit -n`. Record BENCHMARK-METHODOLOGY §6 env; **`NOSTOS_FAKE_EPS=0` and `NOSTOS_FAKE_KEYS=0`**
(§4.3: they do not apply to bench and must stay 0 = unpaced, or they cap the headline).
**Revert threshold: >3% ops/sec regression at 1k (i.e. <808k vs RESULTS.md's 833,307) OR any drop% >
0.00%.** 3% is conservative (the 208× moat tolerates it before rounding toward 200×); any non-zero
drop directly falsifies the "0.00% drops" headline and is non-negotiable.

## JSON-debuggability

Not pressured. Per-element HLC inside the JSON payload stays human-readable (`{"x":{"h":…}}`). No
binary framing; "the wire stays human-debuggable JSON until a measurement says otherwise" (CLAUDE.md)
— that measurement has not arrived.

## Alternatives

Full PN-counter (rejected — no P2P path); version vector (rejected — O(n) blowup at 1k–10k clients);
server-LWW-only for sets (rejected — concurrent add/remove mis-serialized); full Loro-style doc CRDT
(rejected in ADR-0004).
