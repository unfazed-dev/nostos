# ADR-0003: Dynamic reactive sync (predicates), not static buckets

- **Status:** Accepted
- **Date:** 2026-06-26

## Context

PowerSync's most-cited limitation is the **1,000-bucket-per-user hard cap** with static-only sync rules. Buckets are *static and cardinality-bound*: one bucket per unique filter value, so a user with 10k chats or 50k items either can't sync or must manually bucket. Exceeding 1,000 makes the sync connection **fail before any data loads.** Their own proposal #349 admits the model does full-reprocessing rather than incremental.

## Decision

Nostos does **not use buckets.** Instead:

1. A client opens a sync session authenticated with **parameters** (`user_id`, `org_id`, roles).
2. The client subscribes with one or more **live predicates** — a safe, scoped expression (e.g. `tasks WHERE org_id = $org AND assignee_id = $user`).
3. As logical-replication deltas arrive, the server evaluates each changed row against the set of *authenticated, live* predicates — indexed by `Predicate.table` for O(1) candidate pruning — and streams matching deltas to the right sessions.
4. State is **cursor-based (LSN + per-stream op offset)**, so reconnects resume exactly where they left off. No full reprocessing.
5. As the user scrolls, the client expands its predicate window dynamically. **No fixed cardinality ceiling.**

## Rationale

- Replaces the #1 PowerSync complaint with a strictly better model.
- Complexity is **O(changed rows × matching predicates)**, not O(all buckets) — scales with what changes, not what exists.
- Cursor-based state gives incremental, resumable sync (the thing PowerSync proposal #349 is trying to add).
- The predicate-evaluation engine is hard IP we build first and benchmark hardest — a real moat.

## Consequences

**Positive:** no ceiling; incremental; the headline differentiator.

**Negative / risk:**
- Evaluating thousands of predicates per changed row could be slow if naïve. **Mitigation:** index predicates by `table` (and later by parameter hash) so the candidate set is tiny before evaluation; benchmark this in Week 2.
- Expressiveness vs. safety: predicates must be a safe subset (no arbitrary SQL). **Mitigation:** a small expression AST, evaluated against auth-scoped parameters; never trust client SQL.

## Scope for Week 1

The Week-1 server ships **table + simple-equality** predicates only — enough to benchmark the fan-out path. The full expression engine (boolean tree, ranges) arrives in Phase 2.

## Alternatives considered

- **Copy PowerSync's bucket model.** Rejected — we'd inherit the ceiling and the complaint.
- **CRDTs everywhere.** Rejected — CRDTs are for decentralized document collaboration, not server-authoritative relational sync (see ADR-0004).
