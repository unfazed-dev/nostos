---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "The sink's queue discipline is observable on the wire: clients stop seeing intermediate row values, so any consumer built against per-event delivery (counting frames, animating each change, auditing intermediates) breaks. Reverting after SDKs ship means a wire-contract regression across Rust/Dart/TS clients, not a server-side revert."
  surprising_without_context: true
  surprise_reason: "A future reader finds a fan-out server that deliberately discards events it successfully produced, inside a project whose headline claim is '0.00% drops'. Without ADR-0030's addendum and the LSN-gated client upsert in hand, this reads as data loss rather than as work the client was already doing one hop later."
  result_of_real_tradeoff: true
  rejected_alternatives: "Raise DEFAULT_SESSION_BUFFER to 16-32k (measured zero-loss in ADR-0040 but buys time, not a bound); disconnect slow clients at a high watermark (correct for event-shaped data, wrong for state-shaped); block fan-out on a full sink (one slow client stalls every client); make the op-log default-on and replay mid-session (reverses ADR-0025's opt-in posture and adds a Postgres write per commit)."
  all_three_true: true
status: accepted
---

# ADR-0045: Per-key conflating session sinks

- **Status:** Accepted (2026-09-23). Three implementation attempts are recorded below —
  the first cost **39% of the 1k headline** and was reverted the same day, and the
  original gate was superseded once the instrument turned out to be wrong. The shipped
  design is attempt 3 (`DeliveryDecision::Superseded`), and the **replacement gate is
  met**: convergence lag holds at 0 for every key under
  `router::tests::conflation_holds_convergence_lag_at_zero_where_a_plain_channel_loses_every_key`
  in `make ci`, with drop-on-full as an in-test control, and the unbacklogged fast path
  measures within noise of the parent (1,645,329 vs 1,715,366 median, overlapping).
  Accepted by the operator 2026-09-23.
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

## Measurement gate — ORIGINAL, superseded 2026-09-22 (see "The instrument was wrong")

> Before/after on the drop-rate ladder rungs, same harness, `make bench`, with
> `BENCHMARK-METHODOLOGY.md` §6.1 headroom verdicts recorded per tier. Ship only if
> **50k stays sub-1% and at least one rung above 50k moves from over-1% to under-1%**;
> otherwise revert.

**This gate is unmeetable, and not because of the host.** Drop rate counts frames; conflation's
benefit is not a frame-count benefit. No drop-rate ladder, on any hardware, at any scale, could
have scored this change. The gate was the wrong instrument from the day it was written. Its
replacement is at the end of this document. The throughput half of it — *do not regress* —
still stands and is still measured.

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

**Attempt 2 was held on a branch, not merged** — not because it regressed (it did not) but
because merging it would have made `drop%` silently wrong the moment conflation engaged, and
that number is the project's honesty surface, quoted in README and RESULTS.md. Shipping a
change that corrupts the metric used to police changes is the exact failure this repo keeps
finding and fixing.

### The step that unblocked it

`DeliveryDecision` needed a third variant. The decision above already said "superseded is a
new counter, not `dropped`" — attempt 2 under-implemented it as a counter local to
`TokioEventSink`, which neither the fan-out nor the bench could see.

## Attempt 3 (2026-09-22) — `DeliveryDecision::Superseded`, and the metric is honest again

`DeliveryDecision` now has a third variant, threaded end to end:

```
TokioEventSink::deliver  →  DeliveryDecision::Superseded
  → deliver_chunk        →  (delivered, dropped, superseded, faulted)
  → FanOutOutcome.superseded / FanOutOutcome::merged
  → Metrics.superseded → MetricsSnapshot.superseded → cairn_events_superseded_total
  → nostos-bench: drop = attempted − delivered − superseded
```

The sink returns `Superseded` for the event that REPLACES a waiting frame, not for the one it
replaced. That is what makes the arithmetic close: a row updated *n* times while backlogged
yields one `Delivered` (the first frame, which queues) and *n−1* `Superseded`, and exactly one
frame reaches the wire — so `delivered` equals frames sent, and `attempted − delivered −
superseded` equals genuine loss. The contract is now
`delivered + dropped + superseded + faulted <= matched`.

`TokioEventSink::superseded()` stays as the sink-local diagnostic the router's own unit tests
assert against; the port variant is the aggregate view. Both are pinned:

- `router::tests::overflow_conflates_a_backlogged_row_instead_of_shedding` — b's FIRST frame
  queues (`Delivered`); every later b returns `Superseded`.
- `fanout::tests::superseded_is_counted_apart_from_both_delivered_and_dropped` — a supersede
  lands in neither `delivered` nor `dropped`.

### The measurement

One run, two formulas — the same binary and the same run, so no run-to-run noise separates
them. 1k clients × 20,000 events, `--buffer 16 --distinct-keys 64` (a small buffer forces the
backlog that 1k clients at the default buffer never produce):

| | frames | drop% |
|---|---|---|
| attempted | 20,000,000 | |
| delivered (client-side frame count) | 17,163,573 | |
| superseded (router-side) | 380,637 | |
| **old formula** `1 − delivered/attempted` | | **14.18%** |
| **new formula** `1 − (delivered+superseded)/attempted` | | **12.28%** |

The 1.90 pp gap is precisely the conflation the old metric scored as loss. It is not a
throughput result and must not be quoted as one — the tiny buffer exists to manufacture
backlog, and `--distinct-keys 64` is nothing like the default stream.

Non-regression at the headline shape (1k × 100,000, default buffer, `--distinct-keys 0`):

Interleaved, alternating arms, 3 runs each, one session, `--distinct-keys 0`:

| run | attempt 2 (parent, `81e747b`) | attempt 3 (`Superseded`) |
|---|---|---|
| 1 | 2,046,799 | 2,031,996 |
| 2 | 1,645,329 | 1,715,366 |
| 3 | 1,520,508 | 1,692,788 |
| **median** | **1,645,329** | **1,715,366** |

Ranges overlap heavily (within-arm spread 1.35× and 1.20×), so the reading is **no measurable
change** — not a 4.3% gain. 0.00% drops and the full 100,000,000 deliveries in every run, with
`superseded = 0` throughout: the default stream is monotonic keys, which offers conflation
nothing.

### A methodology fact this run forced out

The first three runs of attempt 3 came in at 1.74M / 1.90M / 2.03M against the **2,520,979 /
2,660,070 medians recorded for attempts 1–2 earlier the same day**, which reads as a 30%
regression. Re-measuring the *parent commit* in the same session settled it: attempt 2 now
medians **1,645,329**, below attempt 3. Both arms decline run-over-run and both sit far under
their own earlier figures, so the drift is the host, not the code.

**Cross-session absolute ops/sec are not comparable on this box.** Only arms interleaved within
one session are. The 2.52M/2.66M figures above are therefore valid as a *pair* and invalid as a
baseline for anything measured later — as is the 2,618,601 headline in `CLAUDE.md` if it is ever
compared against a fresh number rather than against its own session's control. This is the same
shape as the `ack_progress` retraction (n=1 per arm is not a measurement), one level up: n=1 per
*session* is not a baseline.

### The instrument was wrong (2026-09-22) — the benefit is measurable, and needs no second host

Attempt 3 left the benefit unmeasured and blamed the single-host harness. That was wrong, and
a literature pass found why within one search. Full writeup:
`docs/plans/measuring-conflation-honestly.md`.

The formal metric for staleness at a receiver is **Age of Information**: `Δ(t) = t − U(t)`,
where `U(t)` is the largest generation time among frames that have reached the destination.
Age rises with time and **resets when a frame with a larger generation time arrives**. A
conflating queue is a device for making that reset happen sooner. In age terms:

> Superseding 99 intermediate values costs a client nothing. Losing the 100th costs it
> everything.

That is ADR-0045's own premise — every frame is a complete row image, only the newest matters —
stated in a metric that can express it. `drop_rate` cannot, at any scale, on any host. Attempt 3
stopped it *penalising* conflation; nothing built on frame counts could ever *credit* it.

Two results worth carrying forward: **push-out and last-generated-first-served queue disciplines
are the formal analogue of conflation and have known AoI-optimality results** — move-to-tail is a
push-out discipline, so the design has theory behind it, it only lacked an instrument. And the
**penalty function matters more than raw age**; Nostos wants the threshold form `1(Δ > d)`, not
the average.

### The measurement

`ReplicationEvent` carries no timestamp and does not need one. The discrete form of age —
**convergence lag**, per key, highest LSN emitted vs. highest LSN held — needs no clocks, no wire
change, no domain change, and no benchmark host. It is deterministic, so per the ponytail ladder
it is not a benchmark at all: it is a test, and it runs in `make ci`.

`router::tests::conflation_holds_convergence_lag_at_zero_where_a_plain_channel_loses_every_key`
feeds **800 events over 8 keys into 16 slots** (channel 8 + overflow 8) with nothing draining,
then drains and compares:

| same 800-event stream, same 16 slots | keys left holding a stale value | sheds |
|---|---|---|
| drop-on-full (pre-ADR-0045) | **8 of 8** | 784 |
| conflating overflow (ADR-0045) | **0 of 8** | **0** |

The drop-on-full arm is inside the same test as a control, so the property cannot pass vacuously.
This is the before/after `docs/ROADMAP.md` and CLAUDE.md's "measure before optimize" asked for,
and it took no host at all.

### Replacement gate (adopted 2026-09-23 — the original is superseded above)

> **Convergence.** While pending distinct keys fit the overflow, conflation holds convergence lag
> at **0 for every key**, on a stream where drop-on-full at the same memory budget leaves every
> key stale. Pinned by a test in `make ci`, not by a benchmark run.
>
> **No regression.** The unbacklogged fast path measures within noise of the parent, arms
> interleaved within one session, ≥3 runs each. (Attempt 3: 1,645,329 vs 1,715,366 median,
> overlapping.)

Adopting this in place of the original was a deliberate change to a gate marked binding, so it was
left for the operator. **Adopted 2026-09-23**, and the ADR moved to *accepted* with it. The
original gate stands superseded, not passed: absolute throughput at 50k–100k is still unmeasured
(see "What is still not claimed"), and nothing below is a throughput claim.

## What is still not claimed

Absolute throughput at 50k–100k still needs the load generator off the server's host — that part
of the old gate's limitation was real and is unchanged. The 10k rung reports `elapsed_secs:
120.00` on every run (window expiry, so per §5 not a measurement) and the baseline at 200 keys
swung 27.11% → 1.19% across two runs. **Do not cite the 100k tier as a Nostos server limit.**

`docs/plans/measuring-conflation-honestly.md` also names four harness defects that are
independent of this ADR: the `FakeReplicator` floods flat-out so the drop rate measures where the
system falls over rather than whether it meets a rate (the fix is an open-loop constant arrival
rate, `wrk2 --rate`); a run that hits its timeout still reports a number instead of
`throughput_valid: false`; there is no fixed repetition policy; and attempt 3's own A/B used a
fixed `A,B,A,B` order, which hands the parent every cold-cache slot.
