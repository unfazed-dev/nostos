# ADR-0031: Three-mode sync rules and checksum-gated resync

**Status:** Accepted · **Date:** 2026-08-06

## Context

nostos already has the machinery to filter what a client sees: server-enforced
predicates (ADR-0011) and a dynamic predicate compiler, `PredicateExpr`
(`crates/nostos-domain/src/predicate.rs`, ADR-0012). What it lacks is an
operator-facing way to declare *what syncs* — today that's env vars
(`NOSTOS_WRITE_TABLES` for writes; nothing equivalent for reads) and whatever
predicates a caller wires up in code. PowerSync's answer to this is "sync
rules" — a declarative, versioned config surface operators edit directly.
nostos has the predicate engine PowerSync's rules would compile to, but no
config format, no mode model, and no resync trigger tied to a rules change.
This ADR defines that surface.

No `nostos_rules.toml` or `sync_mode` exists in the codebase yet — this is new
config surface, not documentation of something already built.

## Decision

### Three mutually exclusive modes

A new config file, `nostos_rules.toml`, declares exactly one `sync_mode`.
Exactly one section is compiled per server boot; the other section, if
present, is inert.

- **`all`** — everything replicated is synced; no rules evaluated. Equivalent
  to an implicit `select *` per table. This is the zero-config dev default.
  Guardrail: startup logs a warning with the introspected table count and a
  row-count estimate, so an operator doesn't reach production on `all` by
  accident. **Tenant scoping (ADR-0011) still applies** — `all` disables
  *rules*, not the tenant-isolation predicate underneath them. It is a
  visibility knob, not a security bypass.
- **`toggles`** — schema introspection plus a toggle editor generate a
  `[tables.*]` section; that section is the truth. Suited to the common case
  (per-table on/off, maybe a tenant-scoping column) without hand-writing
  predicate grammar.
- **`hand`** — a hand-authored `[[rules]]` section is the truth. Entering this
  mode freezes the toggle generator: `nostos rules edit` (or equivalent
  tooling) refuses to write `[tables.*]` while `hand` is active. This is for
  rules the toggle grammar can't express yet, or operators who want the
  literal grammar rather than a generated one.

### Truth-switching semantics

Switching modes moves the source of truth without destroying the mode you
left:

- Entering `hand` freezes the generator (`toggles` section stops being
  writable, but is not deleted).
- Switching back to `toggles` **deactivates** the `[[rules]]` section — it is
  ignored, never deleted — and the `[tables.*]` section becomes truth again.
- Entering `all` ignores both sections. `all` **must never delete either
  section** — it's a read-time override, not a config mutation, so toggling
  away from `all` restores whichever section was truth before.

The rule in one sentence: mode switches change what's *read*, never what's
*stored*. An operator flipping back and forth doesn't lose hand-authored or
toggle-generated work.

### Versioned rules + checksum resync

`nostos_rules.toml` carries a `version` field. The server computes a canonical
checksum over `(sync_mode, canonicalized active section)` — canonicalized
means normalized whitespace and key order, not raw file bytes, so a cosmetic
edit (reformatting, reordering keys) does not change the checksum and does
not force a resync. A semantic edit (a rule's predicate, a toggled table, a
mode flip) does change it.

That checksum folds into the epoch the server already advertises at
subscribe time — the `slot_epoch`/`client_epoch` mechanism from ADR-0025
(`crates/nostos-infra/src/transport.rs`, `encode_resume_info`). A rules change
is composed into the same epoch comparison that already gates
snapshot-vs-resume, rather than adding a second, parallel version channel
that clients would need to check independently. A mode flip alone — even one
that produces an equivalent predicate — forces a resync, because the
checksum is computed over `(sync_mode, section)` as a pair, not over the
predicate's semantic output.

### Grammar (v1)

Rules compile to the existing `PredicateExpr` (`crates/nostos-domain/src/predicate.rs`).
v1 grammar is deliberately small:

```
column <op> claims.<field>
column <op> <literal>
```

with `op` one of `= != < > <= >=`, composed with `AND` only (no `OR`, no
`NOT`, no joins, no bucket/partition grammar). This is a strict subset of
what `PredicateExpr` can already represent — the compiler targets an existing
type, it doesn't extend one.

Missing-claim handling is a distinct case from ADR-0012's missing-*column*
rule. ADR-0012 (`docs/adr/0012-dynamic-predicate-expression-engine.md`,
"missing columns under composition") governs a column absent from a *row* at
evaluation time — `Eq{absent}` and `Ne{absent}` both resolve defensively to
non-match, two-valued logic, no NULL semantics. Missing-claim is a *request*
axis, not a row axis: a rule reads `claims.<field>` from the connecting
principal's auth context, and if that claim isn't present at request time,
the rule cannot be evaluated at all — this is an authorization gap, not a
data gap. v1's resolution: **deny the table entirely** (empty result), never
fall back to `PredicateExpr::any()`. A missing claim must never accidentally
widen visibility; the two rules (missing-column, missing-claim) reach the
same "fail closed" conclusion via different mechanisms because they're
answering different questions.

### Reload

Rules reload without an engine restart. v1 mechanism: a checksum change
closes live sessions with a reason and lets clients reconnect into the new
ruleset, driven by the epoch fold above (Task 14 in the sync-streams suite
wires the close-and-reconnect path).

`ponytail:` this is a full-disconnect reload, not an in-place predicate
swap — simplest correct thing that doesn't require rebuilding a session's
in-flight fan-out state under a changed predicate. The upgrade path is
swapping the compiled predicate on a live session in place, skipping the
disconnect, once reload frequency or session-churn cost justifies the extra
complexity.

## Consequences

- **Positive:** operators get one declarative surface (`nostos_rules.toml`)
  for "what syncs" instead of ad hoc predicates wired in code; `all` gives a
  genuinely zero-config dev default without a security hole (tenant scoping
  still runs underneath); `toggles`/`hand` cover both the common case and the
  escape hatch without a third half-measure mode; checksum-in-epoch means
  rules changes reuse the resume/resync machinery ADR-0025 already built
  instead of adding a parallel versioning channel.
- **Negative:** v1 reload is a full session disconnect-and-reconnect, not a
  live predicate swap (see `ponytail:` above) — every rules edit is visible
  to connected clients as a brief resync, not a silent hot-swap.
- **Non-goals (v1):** `OR`/`NOT` composition, joins across tables, and
  bucket/partition grammar (PowerSync-style bucket checksums, already called
  out as deferred moat machinery in ADR-0025's Divergence section) are out of
  scope — the grammar is intentionally the minimal `AND`-only subset that
  compiles to `PredicateExpr` today. In-place predicate swap on reload is
  also out of scope, tracked as the `ponytail:` upgrade path above.
- **`NOSTOS_WRITE_TABLES` is not governed by rules in v1.** Writes remain
  gated by the separate allowlist from ADR-0013 (`PgWriteBack`,
  `crates/nostos-infra/src/write_back.rs`). This ADR's `sync_mode`/rules
  system governs what a client can *read* (subscribe/sync); it says nothing
  about what a client can *write* — that stays a distinct gate until a later
  ADR unifies them, if one ever does.

## References

- Prior: ADR-0011 (server-enforced predicates), ADR-0012 (dynamic predicate
  expression engine — `PredicateExpr`, missing-column semantics), ADR-0013
  (direct write-back design — `NOSTOS_WRITE_TABLES`), ADR-0025 (persisted
  oplog backfill — `slot_epoch`/`client_epoch` resume-gate mechanism this ADR
  folds the rules checksum into)
- Brief: `.superpowers/sdd/nostos-sync-streams-suite/task-1-brief.md`
