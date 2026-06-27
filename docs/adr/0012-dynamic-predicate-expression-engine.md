# ADR-0012: Dynamic predicate expression engine (Front 1 — the marketed moat)

- **Status:** Slice 1 shipped (Phase 2 — boolean tree over text-only values); typed comparison deferred pending the pgoutput column decoder.
- **Date:** 2026-06-27 (sketch); slice 1 landed 2026-06-27.

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

### Slice 2 — deferred (typed comparison)

`ColumnValue` gains `Number(i64)`, `Float(f64)`, `Bool`, `Timestamp`, and the
ordered leaves `Lt | Gt | Le | Ge`. **Why deferred:** ordered comparison
(`a < b`) is meaningless unless you can read real typed values out of the row —
and the payload is still opaque `Bytes` (no column decoder). Shipping typed
operators testable-only-against-synthetic data would inflate the diff with code
that can't prove its real-world correctness. The decoder is the true gate;
once it exists, slice 2 extends the tree *additively* (the recursion and
combinator API below are the foundation).

Also deferred (separate slices, not part of the moat's structural foundation):
the parameter-set-digest indexing (the table index already prunes to
O(sessions-on-table)), and the safe-SQL-subset compiler at the subscribe
boundary.

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

**Negative:** Phase 2 still cannot honestly claim "dynamic reactive sync GA" in
full — typed comparison (ranges, ordered filters) is the half the strategy doc
sells for "scroll forever," and it gates on the decoder. This ADR records that
honestly rather than overclaiming.

**Kill criterion (carried from the sketch):** if the predicate engine can't
evaluate 10k concurrent authenticated predicates against a live WAL stream
without measurable source-DB read cost, the architecture is wrong (STRATEGY §10).
Slice 1 doesn't yet reach that scale claim — the parameter-set-digest indexing
that makes it true is a separate, deferred slice.

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
- Code: `crates/nostos-domain/src/predicate.rs` (`Predicate`, `PredicateExpr`),
  `crates/nostos-infra/src/transport.rs` (`build_predicate`),
  `crates/nostos-application/src/fanout.rs`
  (`boolean_tree_or_and_not_route_through_fanout` integration test).
- Advisor consult (architecture, HIGH confidence): split the tree from typed
  comparison; land `And|Or|Not` over `Eq|Ne` on text-only values first.
