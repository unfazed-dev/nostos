# ADR-0012: Dynamic predicate expression engine (Front 1 — the marketed moat)

- **Status:** Moat complete — slices 1 & 2 shipped (boolean tree + typed comparison over real decoded values). Parameter-set-digest indexing and the safe-SQL-subset compiler remain deferred.
- **Date:** 2026-06-27 (sketch); slice 1 landed 2026-06-27; slice 2 landed 2026-06-27.

## Context

Front 1 ("Dynamic Reactive Sync — kill the buckets") is marketed as Nostos's
headline moat. The Phase-0 `Predicate` (single table + AND-of-equalities over
`ColumnValue::{Text, Any}`) proved the fan-out path and the predicate-based
delivery architecture, but it was **not** the boolean-tree expression engine the
strategy doc sells. PowerSync's bucket ceiling is the wedge; the predicate engine
is the IP that replaces it.

The critical enabling fact (now realized): the domain `Predicate::matches` takes
a caller-supplied `extract` closure (`Fn(&str) -> Option<ColumnValue>`). The
matcher is **fully decoupled from the payload codec** — so the tree can be built
and tested against a synthetic column view *without* the pgoutput column decoder
ever existing. That made slice 1 self-contained and low-risk.

## Decision

**Ship the boolean tree in two slices. Slice 1 is now shipped; slice 2 (typed
comparison) remains gated on the column decoder.**

### Slice 1 — shipped (boolean tree + Eq/Ne + And/Or/Not)

`Predicate` now carries a `PredicateExpr` tree:

```
PredicateExpr ::= Any
                | Eq(column, value)   | Ne(column, value)
                | And(Vec<Expr>)       | Or(Vec<Expr>)
                | Not(Box<Expr>)
```

- Leaves are `Eq` / `Ne` over the **existing text-only** `ColumnValue::{Text, Any}`.
  `Ne` is the new, *safe* inequality primitive.
- Combinators `and_eq` / `or_eq` fold into `And` / `Or` nodes, collapsing the
  match-all `Any` root to a bare leaf where possible (so the historical
  single-equality form stays structurally identical).
- Negation is the idiomatic `std::ops::Not` trait — `!predicate` and `!expr` —
  not a `.not()` method (a method would shadow the trait and trip clippy's
  `should_implement_trait`).
- `Predicate::matches(extract)` is byte-identical at the boundary: **every
  existing call site across all 9 crates compiles unchanged.** The only internal
  consumer of the old `.filters` field (`build_predicate` in the server
  transport) was rewritten onto the new combinators — ADR-0011's tenant injection
  is preserved exactly.

### Slice 2 — shipped (typed comparison + JSON extractor)

A re-examination of the codebase overturned the "no column decoder" premise that
deferred this slice: `PgReplicator::tuple_to_json_payload` *already* decodes
every real Postgres row into a JSON object `{"col":"val",...}` — every value is
a JSON **string**, regardless of SQL type. So the decoder existed all along; the
missing piece was just a JSON parser wired to the `extract` seam. Slice 2 ships:

- **Typed `ColumnValue`:** `Number(i64)`, `Float(f64)`, `Bool(bool)` added to
  the enum (alongside `Text`/`Any`).
- **Ordered leaves** `Lt | Gt | Le | Ge` on `PredicateExpr`, with `Predicate`
  builders `lt/gt/le/ge` + `PredicateExpr` leaf constructors.
- **Type coercion on the filter side** (the decisive design call): the row value
  arrives as `Text` (the JSON payload quotes everything), so ordered leaves
  parse the row string into the filter's declared type at match time. A parse
  failure or a cross-type numeric divide ⇒ no match (defensive, never
  over-deliver). This sidesteps the i64/f64 ambiguity the advisor flagged: the
  *filter* declares the type, the row conforms or fails.
- **`extract_json_column`** in `nostos-infra` (new `replicator::extract` module):
  parses the payload **once** into an owned `(String, ColumnValue)` map and
  returns a closure for the `matches` seam. No lifetime gymnastics — the closure
  owns its data. An end-to-end test proves `priority > 3` matches a row rendered
  in the exact `tuple_to_json_payload` shape.

**Float equality is intentional, not lint-suppressed by accident:** the row
value comes from a deterministic text parse (e.g. `"1.5"` → `1.5`), so exact
`==`/`partial_cmp` is correct — an epsilon margin would be wrong (1.5 must equal
1.5). The `float_cmp` lint assumes accumulated arithmetic error, which doesn't
apply; both comparison functions carry a scoped `#[allow(clippy::float_cmp)]`
with that rationale.

The moat is now complete: ranges over real decoded values, proven against real
PG rows via the JSON path.

### Still deferred (separate slices)

- **Parameter-set-digest indexing:** the table index already prunes the
  candidate-session set to O(sessions-on-this-table). The digest makes the
  constant factor smaller, not the architecture different.
- **Safe-SQL-subset compiler** at the subscribe wire boundary (the wire message
  still carries `FilterClause{column, value:String}`; typed predicates are
  constructed server-side for now).
- **Three-valued (NULL) logic** for the `!Eq{absent}` edge (see below).
- `Timestamp` typed value + `In`/`Like`/`Between` leaves — none needed for
  "scroll forever"; add when a real query demands it.

## The one real semantic decision — missing columns under composition

The existing invariant — an `Eq` filter on a column absent from the row does
**not** match (defensive, never over-deliver) — is extended uniformly to `Ne`:
an absent column can't be verified to differ, so `Ne{absent}` also returns
`false`. `Ne` is thus the *safe* inequality that never over-delivers.

`And` / `Or` / `Not` compose with **standard two-valued boolean logic** (the
simplest correct logic — no speculative NULL handling). This produces one
documented edge: because absence makes `Eq` return `false`, `!Eq{absent}`
returns `true`. This is the SQL-NULL three-valued-logic gap. Resolution for
slice 1:

- Stay two-valued (ponytail — don't build three-valued logic before a schema
  demands it).
- Document it loudly (here and in the `matches` doc comment).
- Provide `Ne` as the safe inequality that never over-delivers on absence.
- Pin the behavior with an explicit test (`not_of_missing_eq_returns_true_pinned_edge`)
  so it is never surprising.
- If real schemas demand it, three-valued logic is a documented future
  refinement — recorded here, not silently changed.

## Consequences

**Positive:** the structural foundation of the moat — recursion, the combinator
API, and end-to-end routing through `fan_out` — is proven and tested. A
predicate can now be a real boolean tree, not a flat AND list. Slice 2 is
additive (new `ColumnValue` variants + new leaves), so later work lands without
churn. The `matches(extract)` seam kept the change fully backward-compatible.

**Negative:** the moat's *evaluation* is complete, but the *product* is not —
Phase 2's "scroll forever on Flutter AND Web" demo still gates on OPFS
persistence (browser-durable storage) and the WASM transport, neither shipped
yet. The predicate engine is also missing its scaling accelerator
(parameter-set-digest indexing) before it can credibly claim the 10k-concurrent-
predicate bar. This ADR records those gaps honestly rather than overclaiming.

**Kill criterion (carried from the sketch):** if the predicate engine can't
evaluate 10k concurrent authenticated predicates against a live WAL stream
without measurable source-DB read cost, the architecture is wrong (STRATEGY §10).
Slices 1+2 prove the *correctness* (the tree routes and matches real rows); the
parameter-set-digest indexing that makes the *scale* claim true is a separate,
deferred slice.

### Measured baseline (2026-06-27) — the index is data-justified, for the right reason

`crates/nostos-application/tests/fanout_scale.rs` drives `FanOutService::fan_out`
against 10,000 concurrently-registered predicate-bearing sessions (the exact
workload the kill criterion names). It measures **three** regimes — two with the
production-shape extractor, one with a naive extractor as a contrast:

- **Parse-once eval-only** (production-shape: extract parses the row once, like
  `extract_json_column`; a row no predicate matches → empty JoinSet): **≈ 150-170
  events/sec through 10k predicates** = ~1.5-1.7M predicate tree-evals/sec,
  ≈ 6-7 µs/event. *This is the number the index decision turns on.*
- **Parse-once realistic** (matching thousands of sessions): **≈ 40-55
  events/sec**, dominated by the `JoinSet` delivery dispatch (one spawned task
  per match) — a separate concern from indexing.
- **Naive re-parse eval-only** (a careless extractor that re-parses the payload
  on every column lookup): **≈ 100 events/sec** — only **~1.5× slower** than
  parse-once, kept as a contrast to document why parse-once matters.

**What this resolved (read-the-damn-docs correction):** a first baseline pass
hypothesized the cost was an extractor artifact (re-parsing per leaf) and that
parse-once would close the gap. **It did not** — parse-once is only 1.5× faster.
The structural bottleneck is the **per-session predicate-tree evaluation loop**
itself (10k recursive bool evals per event), not the payload parse. The
param-set-digest index is therefore genuinely justified: it short-circuits that
loop for predicates whose equality filters cover the row's params, dropping them
to an O(1) digest lookup instead of a full tree walk. This is the next increment,
justified against the production-shape (~150-170 evt/s) number — not the naive
one.

The test is `#[ignore]`'d (~35s) and run explicitly with `--ignored` so it
doesn't slow the regular `cargo test` suite; its floor (50 events/sec on the
parse-once path) guards against regressions below the *current un-indexed*
production-shape state, not against the optimization gap itself.

## Alternatives considered

- **Ship a stub tree (Phase 0):** rejected — violates ponytail's no-scaffolding
  rule; a non-functional matcher is worse than an honest equality floor.
- **Land typed `ColumnValue` together with the tree:** rejected (advisor review,
  HIGH confidence) — `Lt`/`Gt`/`Le`/`Ge` genuinely require decoded column values
  to test meaningfully; without a decoder, typed operators are code that can't
  prove its real-world correctness. Slice them after the decoder exists.
- **Stub the typed `ColumnValue` variants now to minimize later churn:**
  rejected — three unused variants + three dead match arms is exactly the
  scaffolding ponytail forbids; the later additive work is identical in size.
- **Three-valued logic from the start:** rejected — speculative until a real
  schema demonstrates the `Not(Eq{absent})` edge bites; `Ne` covers the safe
  inequality use today.

## References

- ADR-0003 (the original predicate decision — this extends it).
- ADR-0011 (server-enforced tenant predicates — `build_predicate` now builds on
  these combinators).
- Code:
  - `crates/nostos-domain/src/predicate.rs` — `Predicate`, `PredicateExpr` (the
    tree + typed `ColumnValue` + ordered leaves + `cmp_op`).
  - `crates/nostos-infra/src/replicator/extract.rs` — `extract_json_column`
    (slice 2: wires the real JSON payload to the `matches` seam).
  - `crates/nostos-infra/src/replicator/pg.rs` — `tuple_to_json_payload` (the
    payload format the extractor reads).
  - `crates/nostos-application/src/fanout.rs` —
    `boolean_tree_or_and_not_route_through_fanout` integration test.
- Advisor consults (architecture, HIGH confidence):
  - Slice 1: split the tree from typed comparison; land `And|Or|Not` over
    `Eq|Ne` on text-only values first.
  - Slice 2: re-consulted after discovering the JSON payload already exists;
    ship typed comparison + the JSON extractor now to finish the moat (FFI
    breadth deferred — it gates on unstarted OPFS).
