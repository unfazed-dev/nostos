# ADR-0004: Server-authoritative LWW default, opt-in CRDT-per-field, custom merge

- **Status:** Accepted (default strategy); CRDT-per-field and custom merge ship Phase 4
- **Date:** 2026-06-26

## Context

When two devices edit offline and reconnect, changes must reconcile. PowerSync's default is **last-write-wins per field** (LWW) with no CRDTs; custom resolution is DIY. CRDT libraries (Yjs/Loro/Automerge) solve *decentralized document* collaboration, not relational app-state sync — they don't map cleanly to Postgres rows and bring bloat/parse cost.

## Decision

Three tiers, by column:

1. **Default: server-authoritative LWW per field.** Postgres is the single source of truth; an `updated_at` (or version/etag) column decides the winner. Same model PowerSync/Zero/Electric converged on. Sane, predictable, matches what most apps want.
2. **Opt-in: CRDT-per-field.** For specific columns (counters, sets, rich-text), the developer marks the column as a CRDT type; Nostos merges it with Loro-style primitives — **without** bolting a whole CRDT document onto the schema.
3. **Escape hatch: custom merge functions.** For the hard cases, the developer provides a function. (PowerSync's only tier.)

## Rationale

- LWW-as-default matches the proven PowerSync/Zero/Electric model and is what most relational apps need.
- CRDT-per-field gives the magic (counters that don't clobber, text that merges) **only where wanted**, avoiding the bloat of making every column a CRDT.
- The right primitive per column, not a one-size hammer.

## Consequences

**Positive:** predictable default; opt-in sophistication; no global CRDT-document bloat.

**Negative:** three tiers is more to explain than "it just works." **Mitigation:** LWW is the documented default; the other two are explicitly opt-in per column, so the common path is one mental model.

## Out of scope

- **Full-document CRDT sync** (Yjs-style) — that's a different product (collaborative editors); Yjs/Loro/Liveblocks own it. We don't compete there.
