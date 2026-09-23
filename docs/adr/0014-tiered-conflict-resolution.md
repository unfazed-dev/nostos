# ADR-0014: Tiered conflict resolution (Front 6 — LWW shipped, CRDT/custom deferred)

- **Status:** Partially shipped (LWW via idempotent apply + CRDT add-wins OR-set via ADR-0030); custom merge deferred (Phase 4)
- **Date:** 2026-06-27

## Context

ADR-0004 decided three tiers: (a) server-authoritative LWW per field (default),
(b) opt-in CRDT-per-field, (c) custom merge functions. The strategy doc markets
all three as Front 6. In reality: tier (a) is *implicit* in how `RowOp` apply
works (Insert/Update = upsert by pk, so the last-arriving write wins), but there
is **no explicit version/etag check, no CRDT engine, and no custom-merge hook**.

## Decision

- **Tier (a) LWW: shipped as the `RowOp` apply semantics.** An Insert or Update
  by pk is an upsert; the WAL order (server-authoritative) decides the winner.
  This is the sane default ADR-0004 specified, and it requires no extra code —
  it falls out of "Postgres is the source of truth, replayed in WAL order."
- **Tiers (b) CRDT-per-field and (c) custom merge: deferred to Phase 4.** They
  are coupled with direct write-back (ADR-0013) — they only matter once clients
  write offline and reconcile, which the read-only Phase 0/1 doesn't exercise.

**Design sketch (Phase 4):**
1. A column annotation marks a column as a CRDT type (counter / set / rich-text).
2. The merge step in the apply path consults the column's CRDT primitive
   (Loro-style) instead of LWW — *only* for marked columns.
3. A custom-merge function registry for the hard cases.

## Rationale

- LWW is the proven Zero/Electric default and needs no engine — it's
  WAL-order replay. Claiming it as "shipped" is honest because it's the actual
  apply behavior.
- CRDT-per-field is genuinely hard (semantic primitives per type) and only
  valuable once writes exist; building it now would be scaffolding on a
  read-only engine.

## Consequences

**Positive:** the default conflict behavior is real and predictable.

**Negative:** CRDT and custom merge are roadmap debt. The strategy doc's Front-6
"three tiers" claim is aspirational until Phase 4.

## Alternatives considered

- **Bolt on a full CRDT document library now:** rejected — Yjs/Loro solve
  *document* collaboration, not relational sync; ADR-0004 already excluded it.

## References

- ADR-0004 (the original three-tier decision — this records its ship status).
- Code: `crates/nostos-domain/src/events.rs` (`RowOp` — the implicit LWW apply).
