# ADR-0030: CRDT merge tier — add-wins set + server-serialized counter

**Status:** Ratified 2026-07-31 + in-flight. Decision 1 (counter delta-op) **SHIPPED** (commit
`5730ed8` — `WriteOp::Increment` + `PgWriteBack::increment` `UPDATE SET col = col + ?`, tenant-guarded;
off the measured bench path). Decision 2 (OR-set CRDT) **algebra SHIPPED** (commit `75e65bd` —
`nostos-domain::crdt`: `Hlc` + add-wins `merge_or_set_payloads`, 13 tests green); apply-path +
client-HLC + server-merge **integration SHIPPED** (slices 2/3/4, commits
`28df948`/`317b4d1`/`45fdc70`); both real-PG e2e green (`increment`, `or_set_writeback`).
**WS3 engine COMPLETE.** **Decision 4 RELAXED** per operator
ratification: HLCs are minted by BOTH client (optimistic local OR-set edits) and server (write-back
commit) — "server-only" would have made the CRDT decorative (Decision 2 addendum). Benchmark gate (D7)
still binding on the integration.
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

   **Addendum (2026-07-31, post-implementation-design):** In nostos's server-authoritative +
   per-row-LSN-gated-LWW model, the HLC-merge is REDUNDANT for server-delivered frames — the server
   serializes, so the latest-LSN frame already carries the converged set and LSN-LWW lands it. The
   CRDT merge does work LSN-LWW cannot in exactly ONE case: an **optimistic local edit** (client adds
   tag X offline) meeting a **server frame** (another user added tag Y) — the pending-replay's
   full-row upsert would clobber Y; only an element-wise HLC merge converges to {X,Y}. That case
   requires the CLIENT to mint the HLC for its optimistic edit (so X is comparable to Y). Hence
   Decision 4's relaxation. The server-side `WriteBack` must ALSO merge element-wise (not clobber)
   when applying the flushed client payload, or a client add loses other clients' elements server-side.
3. **Timer → stays LWW** (a register; CRDT would be wrong).
4. **Causal metadata = HLC, not version vector.** A version vector is one entry per replica per op;
   nostos's model implies 1k–10k clients → O(10³–10⁴) entries per frame → kilobytes of JSON on a small
   row → hot-path death. HLC is O(1), ~16–26 B/op. **Minted by BOTH client and server** (relaxed
   2026-07-31 from "server-only"): the client mints for optimistic local OR-set edits, the server at
   write-back commit. HLC needs no clock sync (Lamport-style — wall + monotone logical counter per
   process), so client minting is sound; the original "server-only, no client clock" framing would
   have made the CRDT decorative (Decision 2 addendum).

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
- **VERIFIED 2026-07-31 (post-implementation):** `WireFrame` is byte-unchanged (HLCs live in the
  opaque payload blob), AND `nostos-bench` provably never imports the CRDT/apply/write code —
  `grep -rE 'apply_batch|Storage|Outbox|or_set|merge_or_set|WriteBack' crates/nostos-bench/src`
  returns only `fs::write` (result-file I/O in `report.rs`). The bench is pure fan-out
  (`FakeReplicator → FanOutService → raw WS clients counting frames`); its clients are NOT
  `SyncClient`s applying rows. So the CRDT (client apply + HLC) and the Increment op (write path)
  are in code the bench never executes — the gate is satisfied by construction; the empirical run
  only confirms 0 drops.

## Benchmark gate (D7 — binding)

Before/after `make bench --clients 1000`, clean `--release`, 3× median, same machine / toolchain /
`ulimit -n`. Record BENCHMARK-METHODOLOGY §6 env; **`NOSTOS_FAKE_EPS=0` and `NOSTOS_FAKE_KEYS=0`**
(§4.3: they do not apply to bench and must stay 0 = unpaced, or they cap the headline).
**Revert threshold: >3% ops/sec regression at 1k (i.e. <808k vs RESULTS.md's 833,307) OR any drop% >
0.00%.** 3% is conservative (the 208× moat tolerates it before rounding toward 200×); any non-zero
drop directly falsifies the "0.00% drops" headline and is non-negotiable.

**VERDICT 2026-07-31 — PASS.** Empirical before/after on the same dev machine, `--clients 1000
--events 50000`, `NOSTOS_FAKE_EPS=0 NOSTOS_FAKE_KEYS=0`, 3 runs each:

| | runs (ops/sec) | median | drop% |
|---|---|---|---|
| before (`9738a26`, pre-CRDT) | 686 463 / 699 668 / 675 639 | ~686k | 0.00% |
| after (CRDT, `45fdc70`) | 718 644 / 676 809 / 638 222 | ~677k | 0.00% |

Median delta −1.3% — well inside the ~10% run-to-run machine noise (the ranges overlap heavily;
after-run-1 actually exceeded every before-run). 0.00% drops both sides (50M/50M delivered each run).
Both numbers are this dev box's fan-out baseline — ~82% of RESULTS.md's 833,307 (recorded on the
project's bench machine), a machine gap not a regression. The off-path proof above (nostos-bench never
imports the CRDT/apply/write code) is the load-bearing reason before≈after; the measurement confirms
0 drops + no regression within noise. **Gate cleared; no revert.**

## JSON-debuggability

Not pressured. Per-element HLC inside the JSON payload stays human-readable (`{"x":{"h":…}}`). No
binary framing; "the wire stays human-debuggable JSON until a measurement says otherwise" (CLAUDE.md)
— that measurement has not arrived.

## Alternatives

Full PN-counter (rejected — no P2P path); version vector (rejected — O(n) blowup at 1k–10k clients);
server-LWW-only for sets (rejected — concurrent add/remove mis-serialized); full Loro-style doc CRDT
(rejected in ADR-0004).

## Implementation status & slices (2026-07-31)

Operator ratified 2026-07-31 to build the **meaningful local-first CRDT** (not the decorative
server-only variant, not defer). Slices:

- **✅ Piece 1 — counter delta-op** (commit `5730ed8`): `WriteOp::Increment` + `WriteBack::increment`
  port + `PgWriteBack` `UPDATE SET col = col + ?` (tenant CTE + EXISTS) + `NoWriteBack` +
  `dispatch_write` arm + 4 mock adapters + 3 local-apply no-op arms. clippy-clean both feature
  configs; workspace suite 441 passed. **VERIFIED 2026-07-31:** live-PG e2e
  `increment_serializes_concurrent_deltas_server_side` PASSES against real Postgres.
- **✅ Piece 2 slice 1 — CRDT algebra** (commit `75e65bd`): `nostos-domain::crdt` — `Hlc` (mint/max,
  const-fn manual compare) + add-wins `merge_or_set_payloads` + `present_elements`; 13 tests (monotone
  mint, commutative/idempotent merge, add-wins, re-add-after-remove, tombstones). Zero moat risk
  (pure domain, off all paths).
- **✅ Piece 2 slice 2 — storage apply-merge** (commit `28df948`): per-table OR-set strategy on
  `SqliteStorage`/`InMemoryStorage` via an internal `or_set_tables: HashSet<String>` (NOT a Storage
  trait change); `apply_batch` + `apply_local` + pending-replay MERGE for OR-set rows via
  `merge_or_set_or_lww` (LWW fallback on parse error).
- **✅ Piece 2 slice 3 — server element-merge** (commit `317b4d1`): `PgWriteBack` MERGES a flushed
  OR-set upsert into a configured JSONB column (read-modify-write via `merge_or_set_or_lww`) instead
  of clobbering — else a client add loses other clients' elements server-side. The "which column holds
  the set" deferral is resolved **fixture-agnostically + config-driven**: `PgWriteBack::with_or_set_columns(HashMap<table,
  column>)` names the column, so the fixture decides at wiring time. No-tenant merge only (the pomodoro
  community row is the shared, unscoped case); tenant + OR-set falls through to the clobber path
  (tenant-scoped shared sets remain fixture co-design). Reused `Upsert` — no new WriteOp/wire op.
  **VERIFIED:** `or_set_writeback_merges_concurrent_client_adds_server_side` PASSES against real
  Postgres (two concurrent adds converge to {alice, bob}, not a clobber).
- **✅ Piece 2 slice 4 — client HLC + optimistic edit** (commit `45fdc70`): `SyncClient` holds HLC
  state; `or_set_add(table, pk, element)` / `_remove` mint a client HLC, build the element payload,
  enqueue, and apply optimistically (slice 2's merge).
- **✅ Slice 5 — D7 bench gate** (commit `7835af3`): before/after `make bench --clients 1000` ×3
  median, `NOSTOS_FAKE_EPS=0 NOSTOS_FAKE_KEYS=0` — **PASS** (0.00% drops both sides, −1.3% within noise;
  off-path proof load-bearing). The pomodoro community shell modeled as a single-row OR-set remains —
  that is fixture work, post-engine.

**WS3 engine COMPLETE 2026-07-31:** all four CRDT slices + the counter delta-op shipped; `increment`
and OR-set-merge e2e both green against real Postgres; D7 bench gate passed; `WireFrame`
byte-unchanged. Remaining = the fixture that exercises it, not engine work.
