# ADR-0012: Dynamic predicate expression engine (Front 1 — deferred)

- **Status:** Deferred (Phase 2 — design sketch + kill criterion)
- **Date:** 2026-06-27

## Context

Front 1 ("Dynamic Reactive Sync — kill the buckets") is marketed as Nostos's
headline moat. The current `Predicate` (Phase 0) is a single table + a
conjunction of `column = value` equality filters over `ColumnValue::{Text, Any}`.
That is enough to benchmark the fan-out path and prove predicate-based delivery,
but it is **not** the boolean-tree expression engine the strategy doc sells.
PowerSync's bucket ceiling is the wedge; the predicate engine is the IP that
replaces it.

## Decision

**Defer the full engine to Phase 2.** The Phase-0 floor (table + AND-of-equalities)
ships now; the generalized engine is designed but not built.

**Design sketch (Phase 2):**
1. `Predicate` becomes a boolean tree: `And | Or | Not` over comparison leaves
   (`Eq | Ne | Lt | Gt | Le | Ge | In`).
2. `ColumnValue` gains `Number(i64)`, `Float(f64)`, `Bool`, `Timestamp` with
   typed comparison.
3. The `SessionStore` indexes predicates by a parameter-set digest (so the
   router evaluates a changed row only against predicates whose param sets
   *could* match), keeping fan-out at **O(changed rows × matching predicates)**.
4. A safe subset of SQL compiles to the tree at the subscribe boundary; anything
   outside the subset is rejected (no arbitrary expression evaluation).

## Rationale

- Phase-0 equality is sufficient to prove the architecture and benchmark against
  PowerSync's ceiling; building the full tree now would be premature
  optimization of a path whose bottleneck is still the fan-out, not the matcher.
- The tree must be designed alongside the parameter-indexing strategy — both are
  the "hard IP" the strategy doc says to build first and benchmark hardest.

## Consequences

**Positive:** the Phase-0 floor is real (not a stub), so the deferred work
extends a working matcher rather than replacing vapor.

**Negative:** Phase 0 cannot honestly claim "dynamic reactive sync GA" — the
strategy doc's Front-1 claim is forward-looking until this ships.

**Kill criterion:** if the predicate engine can't evaluate 10k concurrent
authenticated predicates against a live WAL stream without measurable source-DB
read cost, the architecture is wrong — pivot before building the product on it
(STRATEGY §10).

## Alternatives considered

- **Ship a stub tree now:** rejected — violates ponytail's no-scaffolding rule;
  a non-functional matcher is worse than an honest equality floor.

## References

- ADR-0003 (the original predicate decision — this extends it).
- Code: `crates/nostos-domain/src/predicate.rs` (the Phase-0 floor).
