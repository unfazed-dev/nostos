---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "The sink's queue discipline is observable on the wire: clients stop seeing intermediate row values, so any consumer built against per-event delivery (counting frames, animating each change, auditing intermediates) breaks. Reverting after SDKs ship means a wire-contract regression across Rust/Dart/TS clients, not a server-side revert."
  surprising_without_context: true
  surprise_reason: "A future reader finds a fan-out server that deliberately discards events it successfully produced, inside a project whose headline claim is '0.00% drops'. Without ADR-0030's addendum and the LSN-gated client upsert in hand, this reads as data loss rather than as work the client was already doing one hop later."
  result_of_real_tradeoff: true
  rejected_alternatives: "Raise DEFAULT_SESSION_BUFFER to 16-32k (measured zero-loss in ADR-0040 but buys time, not a bound); disconnect slow clients at a high watermark (correct for event-shaped data, wrong for state-shaped); block fan-out on a full sink (one slow client stalls every client); make the op-log default-on and replay mid-session (reverses ADR-0025's opt-in posture and adds a Postgres write per commit)."
  all_three_true: true
status: proposed
---

# ADR-0045: Per-key conflating session sinks

- **Status:** Proposed (2026-09-22). **Implemented and reverted the same day** — the
  design is sound and its invariants are test-pinned, but the first implementation cost
  **39% of the 1k headline** and the gate below is binding. Work preserved on the local
  branch `adr-0045-conflating-sink` (`6044dbc`); see "Implementation attempt".
- **Date:** 2026-09-22
- **References:** ADR-0009 (ack-driven resume, one checkpoint per socket), ADR-0025
  (op-log backfill; `(table_name, pk)` compaction index), ADR-0030 (CRDT merge tier —
  the addendum is load-bearing here), ADR-0039 (sync streams), ADR-0040 (bounded-sink
  loss windows), `docs/BENCHMARK-METHODOLOGY.md` §5–6.1,
  `benches/results/RESULTS.md` (drop-rate ladder, 2026-09-21).

## Context

The per-session sink is a bounded tokio mpsc, `DEFAULT_SESSION_BUFFER = 1024`, with
`try_send` and drop-on-full (`crates/nostos-infra/src/router.rs`). A session that
cannot drain sheds events and counts them (`Metrics.dropped`) — honest, never silent
OOM, never blocking fan-out.

The cost of that contract is now measured, twice:

- ADR-0040: an unpaced 20k-row load at buffer 1024 shed **5,988 events (29.9%)**.
- The drop-rate ladder (2026-09-21): **50,000 clients is the last reproducibly sub-1%
  rung**. Aggregate throughput is flat from 10k to 80k (430k–675k deliveries/sec, no
  trend) while drops climb 0 → 8%. What degrades with client count is delivery
  *completeness*, not rate.

Buffer depth is the wrong dial. It bounds events queued, so the bound scales with the
producer's rate and the consumer's stall duration — neither of which the server
controls. Every additional slot buys latency before the same cliff.

## The observation this rests on

Nostos's server→client plane carries **state, not operations**. Three facts, verified
against shipping code:

1. `tuple_to_json_payload` (`replicator/pg.rs:1182`) iterates **every column in
   `meta.columns`**. A frame is a complete row image, never a delta. Unchanged-TOAST
   columns are backfilled from the OLD tuple under `REPLICA IDENTITY FULL`, which
   pg-init sets on synced tables.
2. The client's apply is already last-writer-wins gated on LSN
   (`nostos-client/src/sqlite.rs:649`): `DO UPDATE SET payload = excluded.payload
   WHERE cairn_data.applied_lsn <= excluded.applied_lsn`. Deletes are gated
   identically. **The client already discards an older frame for the same key.**
3. No delta op reaches this plane. `WriteOp::Increment` (ADR-0030 D1) is client→server
   only and replicates back as an ordinary full-row update. ADR-0030's addendum states
   the case directly: the HLC merge "is REDUNDANT for server-delivered frames — the
   server serializes, so the latest-LSN frame already carries the converged set."

Therefore, for two frames on the same `(table, pk)`, delivering only the higher-LSN one
leaves the client's SQLite in a **byte-identical** final state. Conflation is not a
weakened delivery contract; it moves a discard the client already performs one hop
upstream, where it saves a socket write instead of wasting one.

## Decision

Replace the per-session FIFO with a **bounded per-key conflating queue** for
`SinkMsg::Event`. `SinkMsg::Control` — snapshot boundaries, which carry no LSN and for
which ordering is the entire contract — bypasses conflation untouched. The two are
already distinct enum variants (`router.rs:52`); the type does the enforcing.

### 1. Move-to-tail, not replace-in-place

When a frame arrives for a key already queued, remove the queued entry and **append the
new one at the tail**. This preserves ascending-LSN delivery order: every arrival
carries the highest LSN seen so far, so appending keeps the queue sorted by induction.

This is the load-bearing detail. Replace-in-place (keep the old queue position, swap the
value) would deliver a higher LSN before a lower one still queued behind it, breaking the
monotonic per-socket checkpoint ADR-0009 depends on. Move-to-tail does not.

### 2. Capacity bounds distinct keys, not events

The sink's bound becomes "how many distinct rows may be pending", which is bounded by
the client's subscription shape rather than by the write rate. A client stalled for a
minute receives one frame per changed row on recovery, each current.

### 3. Overflow is a resync trigger, not a drop

Exceeding the key bound means the client is behind on more distinct rows than the sink
will track — at which point a snapshot is the cheaper repair. That is exactly ADR-0040's
`resync_required` path, which must therefore move from opt-in
(`NOSTOS_RESYNC_SIGNAL`, currently default off) to on by default. Shipping conflation
without that flip leaves the one genuinely lossy case silent.

### 4. `superseded` is a new counter, not `dropped`

`Metrics.dropped` means "the client will never see this state." A superseded frame is
not that. Counting the two together would quietly inflate the drop figure the project
treats as its honesty surface. New counter; `dropped` keeps its meaning.

## Consequences

- **Frame-to-frame continuity breaks by design.** Any client-side gap detection must
  distinguish "superseded, you are current" from "gap, resync". This is a wire-visible
  change and the reason this ADR exists.
- **Transactional grouping is unaffected** — there was none on this plane. FIFO
  drop-on-full already sheds mid-transaction; `txn_id` is a grouping hint, not an
  atomicity guarantee (ADR-0030 context).
- **The durable plane is untouched.** The op-log is written at the `FanOutService::run`
  chokepoint (ADR-0025), upstream of every session sink, so completeness-by-replay is
  independent of what the live plane conflates. This is what lets Nostos hold both
  contracts at once: live plane guarantees convergence, durable plane guarantees
  recoverability.
- **Raising `DEFAULT_SESSION_BUFFER` becomes unnecessary** and should not ship
  alongside this — a key-bounded map sized by subscription shape does not need depth.
- **Pre-existing, not introduced:** without `REPLICA IDENTITY FULL` an unchanged TOAST
  column renders as `""` and clobbers the client's copy (audit 2026-08-17 M2; the wire
  sentinel is still deferred). That is a per-frame defect under plain LWW today and
  conflation neither causes nor worsens it.

## Measurement gate (binding, per CLAUDE.md "measure before optimize")

Before/after on the drop-rate ladder rungs, same harness, `make bench`, with
`BENCHMARK-METHODOLOGY.md` §6.1 headroom verdicts recorded per tier. Ship only if
**50k stays sub-1% and at least one rung above 50k moves from over-1% to under-1%**;
otherwise revert.

**Known limitation of that gate.** The current harness runs load generator and server on
one 10-core host, which the ladder's own signature confirms is generator-bound (flat
throughput, rising drops). Published guidance is that co-located benchmarks are at best
indicative. The 100k question cannot be answered on this box, and the >1% rungs must not
be cited as Nostos server limits either before or after this change. A two-host harness is
a prerequisite for any claim about 100k, and is out of scope here.


## Implementation attempt (2026-09-22) — measured, failed the gate, reverted

Built it: the per-session `mpsc` was replaced with a `BTreeMap<seq, SinkMsg>` ordered by
a monotone sequence plus a `HashMap<RowKey, seq>` conflation index, behind a
`SinkReceiver` deliberately API-compatible with `mpsc::Receiver` (`recv().await` +
`try_recv()`) so the transport's batching loop and its ~20 tests were untouched.
`RowKey` borrows `(table, pk)` from the event through an `Arc` so the index costs a
refcount bump, not two `String` clones.

Six tests pin the invariants and all pass: supersede-on-same-row, move-to-tail keeping
LSNs ascending, distinct rows still shedding at capacity, no superseding across a control
frame, `recv()` ending when the sink drops, and `deliver_awaiting` parking for room
instead of truncating a snapshot. `make ci` clean.

**A/B, same machine, same session, 1k clients × 100k events, 3 runs per arm** (ADR-0030
D7 protocol):

| arm | ops/sec (3 runs) | median | drop% |
|---|---|---|---|
| baseline | 2,438,756 / 2,520,979 / 2,682,368 | **2,520,979** | 0.00 |
| conflating sink | 1,432,940 / 1,532,279 / 1,545,688 | **1,532,279** | 0.00 |

**−39.2%**, or +256 ns per delivery (397 → 653 ns). The revert threshold is 3%.

The cost is the implementation, not the design. Every delivery now takes a `Mutex`, a
`BTreeMap` insert (which allocates) and a `HashMap` insert with string hashing, replacing
a lock-free `mpsc::try_send`. At zero backlog none of that work buys anything — there is
nothing pending to supersede.

### What would fix it

Conflate **only under backlog**, mirroring the reasoning already used for batched writes
(`transport.rs`: batching "only kicks in when the channel already has a backlog"). Keep
`mpsc::try_send` as the fast path; on `Full`, set a backlog flag and divert into the
conflating queue; the receiver drains the channel first, then the queue, clearing the flag
when it empties. The fast path then costs exactly what it costs today.

That hybrid has one sharp edge worth naming before anyone writes it: the flag is read
outside the lock, so a producer can observe "backlogged" just as the receiver clears it
and divert an event into an already-drained queue, delivering it after newer events from
the channel. Out-of-order across *different* rows is harmless — the client's apply gate is
per `(table, pk)`, so each row still lands its own highest LSN. The residue is the same
exposure drop-on-full already has today (the client acks past a row it never received),
which is what `capacity_sheds` and ADR-0040's resync exist for. Strictly rarer than the
status quo, not a new class of bug — but it must be reasoned about deliberately, not
discovered.

### Not done, and why

`NOSTOS_RESYNC_SIGNAL` stays default-off. The decision above calls for flipping it, but
that sends a frame shape older clients have never seen, and with conflation held back the
flip would change wire behaviour for existing deployments while fixing nothing they are
not already living with. It belongs with the hybrid, as one wire-compat decision.

## Attempt 2 (2026-09-22) — the redesign works; the *metric* is what blocks it

Rebuilt per the plan above and it holds up. `mpsc::try_send` stays the fast path verbatim;
`Err(Full)` **is** the backlog signal, so the success path asks nothing — no flag, no lock,
no bookkeeping. Only the full branch folds the event into a conflating overflow map
(`BTreeMap<lsn, event>` + `HashMap<RowKey, lsn>`), and an event is shed only once that map
holds `capacity` distinct rows. A test asserts the overflow is never touched below the
channel cap.

The fast-path/slow-path literature names the trap attempt 1 fell into exactly: the overhead
comes from *"instrumentation on the fast path that manipulates the metadata the fallback
path uses."* Attempt 2 has none.

**Throughput A/B, 1k clients × 100k events, 3 runs per arm, same machine and session:**

| arm | ops/sec (3 runs) | median |
|---|---|---|
| baseline | 2,438,756 / 2,520,979 / 2,682,368 | 2,520,979 |
| overflow conflation | 2,531,945 / 2,660,070 / 2,863,173 | 2,660,070 |

0.00% drops and the full 100,000,000 deliveries in both arms. The ranges overlap, so the
honest reading is **no measurable change** — not a 5.5% speedup.

### The bug the benchmark caught and the unit tests did not

First build of attempt 2 measured 111k ops/sec at **86.6% drops**. Cause: `recv()` awaited a
message from the channel and then *discarded it* before re-consulting the overflow. Every
unit test happened to find its message via `try_recv` and never parked, so all of them
passed. Fixed, with a regression test that forces the park. Worth remembering: the six tests
pinning the conflation invariants were all green while the sink was losing 86% of its
traffic.

### Why the benefit still cannot be scored — and this is the blocker

`nostos-bench` computes `drop_rate = 1 - delivered / (events × clients)` where `delivered` is
the **client-side frame count** (`crates/nostos-bench/src/main.rs`). Conflation's entire
purpose is to send *fewer frames for the same state*. So the harness scores every supersede
as a loss: the metric counts precisely the thing this ADR is designed to reduce.

Two further harness facts found on the way, both worth keeping:

1. **`nostos-bench` hardcoded `distinct_keys: 0`** — every event gets a brand-new row, so
   there is *zero* conflation opportunity by construction. A `--distinct-keys` flag was added
   (default `0`, so every historical number in RESULTS.md is unaffected). The server already
   had `NOSTOS_FAKE_KEYS`, defaulting to 50; the bench never did.
2. **The 10k rung on this host is not a measurement regime.** Every run reported
   `elapsed_secs: 120.00` — the timeout, exactly — so its "drops" are window expiry, not sink
   sheds. Baseline at `--distinct-keys 200` came back **27.11%** then **1.19%** on two runs,
   a 23× swing. Per § 5 neither arm's numbers there mean anything.

**Status: held on branch `adr-0045-conflating-sink` (`1cb91a9`), not merged.** Not because it
regresses — it does not — but because merging it would make `drop%` silently wrong the moment
conflation engages, and that number is the project's honesty surface, quoted in README and
RESULTS.md. Shipping a change that corrupts the metric used to police changes is the exact
failure this repo keeps finding and fixing.

### The one remaining step

`DeliveryDecision` needs a third variant. The decision above already says "superseded is a
new counter, not `dropped`" — attempt 2 under-implemented it as a counter local to
`TokioEventSink`, which the fan-out and the bench cannot see. Promoting it to
`DeliveryDecision::Superseded`, threading it through `deliver_chunk` → `FanOutStats` →
`Metrics`, and having the bench compute `drop = attempted - delivered - superseded` makes the
benefit measurable and the metric honest again. That touches an application-layer port enum,
so it is a deliberate change, not a patch.
