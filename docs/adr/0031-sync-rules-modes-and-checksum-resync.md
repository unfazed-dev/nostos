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

That checksum is carried as an **explicit wire field**, `rules_checksum`, on
the `Subscribe` frame (`crates/nostos-infra/src/wire.rs`) — a sibling to the
existing `epoch` field from ADR-0025, not a value folded into the epoch
integer itself. A log reader can then tell "slot recreated" from "rules
changed" by looking at the frame instead of decoding a composed number. The
wire stays human-debuggable JSON: `rules_checksum` is one additional
optional field, nothing more.

A client that sends both `epoch` and `rules_checksum` is gated on **epoch
match AND checksum match** — independent comparisons. Backward compatibility
is unconditional in both directions: a client that omits `rules_checksum`
(pre-ADR-0031) is never rejected — the server falls back to composing the
checksum into the epoch value it advertises (the same `slot_epoch` mechanism
ADR-0025 shipped, via `encode_resume_info`), so an old client still resyncs
correctly on a rules change without ever parsing the new field. A mode flip
alone — even one that produces an equivalent predicate — forces a resync on
both paths, because the checksum is computed over `(sync_mode, section)` as a
pair, not over the predicate's semantic output.

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

Rules reload without an engine restart. v1 mechanism: **in-place predicate
swap** — a live session's compiled predicate is re-scoped to the new
ruleset without disconnecting the socket. Only sessions whose scope
*narrows* under the new ruleset are invalidated, and only for the affected
subscriptions (a table dropped from `[tables.*]`, or a `[[rules]]` predicate
that now excludes rows the session was receiving); a session whose scope is
unaffected or widens keeps running untouched. Task 14 in the sync-streams
suite wires the swap.

Disconnect-and-resync is the documented **fallback**, not the primary path:
if swap verification fails for a session, that session is closed with a
reason and reconnects into the new ruleset, exactly as a full-restart reload
would behave. `ponytail:` the fallback is coarse — a session that fails
verification pays for a full resnapshot rather than a targeted one. Ceiling:
every swap-verification failure costs that client a full snapshot, same as
today's whole-fleet disconnect would have cost every client. Upgrade path:
narrow the fallback further (partial resnapshot of only the newly-out-of-scope
rows) once swap-verification failures are common enough to matter.

### Admin auth on `PUT /rules` (D5, Task 21)

`PUT /rules` is the first route in nostos's history that writes operator
config rather than serving session-scoped reads. It is gated by a separate
`NOSTOS_ADMIN_TOKEN` bearer token (`crates/nostos-server/src/admin_auth.rs`),
deliberately **not** `NOSTOS_SYNC_AUTH`: that path authenticates *application
users* (a Supabase JWT, however valid, however privileged its claims), and no
application user may ever rewrite the server's rules. The two systems share
no code and no state — a valid `/sync` JWT presented to `PUT /rules` is
rejected the same as no credential at all. `NOSTOS_ADMIN_TOKEN` unset means
the route **404s**, not 401: a default deployment that never opts in has no
mutable surface to attack. A set-but-short (<32 char) token fails the server
at startup rather than serving a guessable admin route.

**CSRF stance — stated, not assumed.** The route authenticates with an
`Authorization: Bearer` header, never a cookie. Browsers do not attach
`Authorization` headers to cross-site requests the way they attach cookies —
there is no ambient credential for a cross-site form or image tag to ride on,
so the classic CSRF forgery (a third-party page causing the victim's browser
to submit an authenticated request the page itself never possessed) does not
apply here regardless of how the browser is configured. CSRF tokens are
therefore unnecessary *because of that credential choice*, not by oversight.
A future contributor who does not find this reasoning may be tempted to "fix"
the apparent missing CSRF token by moving the credential into a cookie —
don't; that would introduce the exact ambient-credential problem this design
avoids.

Two defenses make the property enforceable rather than merely incidental:

- `PUT /rules` rejects any request whose `Content-Type` is not
  `application/json` (415) — this closes the one classic simple-form CSRF
  vector (`<form>` submissions, which the browser restricts to a small set of
  "simple" content types) even in a hypothetical future where the credential
  moves to a cookie.
- Task 21 adds no CORS allowance for this route. **This is narrower than "CORS
  stays default-deny" for the server as a whole** — `NOSTOS_CORS_ORIGINS`
  unset already defaults to a permissive `CorsLayer` server-wide (pre-existing
  behavior, unrelated to this task; see `docs/OPERATING.md` §1). CORS governs
  whether a browser page running on another origin may *read* a cross-origin
  response via `fetch`/`XHR`, not whether it can attach ambient credentials —
  the CSRF argument above holds regardless of the CORS setting, because
  forging the request still requires the attacker's page to already possess
  the token to set the `Authorization` header, at which point CSRF is moot. An
  operator who wants `PUT /rules` responses unreadable by other-origin
  browser code should still set `NOSTOS_CORS_ORIGINS` explicitly; if a
  cross-origin allowance is genuinely needed for the web panel, it must be an
  explicit, reviewed, operator-configured origin — never `*`.

Audit: every successful mutation emits exactly one `tracing::info!` line at
target `nostos::audit` (`rules_mutation actor=<8-hex> source=<web|api>
mode_before=... mode_after=... checksum_before=0x... checksum_after=0x...
tables_changed=N`), success path only. `actor` is the first 8 hex characters
of SHA-256(token) — enough to distinguish two operators in the log, not
reversible back to the token. No claim values or row data are ever logged.

## Consequences

- **Positive:** operators get one declarative surface (`nostos_rules.toml`)
  for "what syncs" instead of ad hoc predicates wired in code; `all` gives a
  genuinely zero-config dev default without a security hole (tenant scoping
  still runs underneath); `toggles`/`hand` cover both the common case and the
  escape hatch without a third half-measure mode; an explicit `rules_checksum`
  wire field composes cleanly with the resume/resync machinery ADR-0025
  already built (old clients still work via the composed-epoch fallback)
  instead of forcing every SDK onto the new field at once; in-place predicate
  swap means most rules edits are invisible to connected clients — no
  fleet-wide resync on every toggle flip.
- **Negative:** in-place swap adds a verification step reload must pass
  before it can avoid disconnecting a session (see `ponytail:` above) — more
  moving parts than a blanket disconnect-everyone reload would have been.
  `rules_checksum` is one more field every SDK's wire layer must eventually
  understand, even though old clients keep working unmodified.
- **Non-goals (v1):** `OR`/`NOT` composition, joins across tables, and
  bucket/partition grammar (PowerSync-style bucket checksums, already called
  out as deferred moat machinery in ADR-0025's Divergence section) are out of
  scope — the grammar is intentionally the minimal `AND`-only subset that
  compiles to `PredicateExpr` today. Full-fleet disconnect-resync on every
  reload is out of scope in favor of selective in-place scope narrowing; it
  survives only as the fallback when swap verification fails.
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
  oplog backfill — `slot_epoch`/`client_epoch` resume-gate mechanism the
  `rules_checksum` field composes with via the composed-epoch fallback)
- Plan: `docs/plans/nostos-sync-streams-suite.md` — operator rulings D2
  (explicit `rules_checksum` wire field, Task 11), D3 (in-place predicate
  swap on reload, Task 14), and D5 (web authoring surface — authenticated
  `PUT /rules`, `NOSTOS_ADMIN_TOKEN` shape, Tasks 20–21), ratified 2026-08-06
- Runbook: `docs/OPERATING.md` §7 — setting, rotating, and responding to a
  leaked `NOSTOS_ADMIN_TOKEN`
- Brief: `.superpowers/sdd/nostos-sync-streams-suite/task-1-brief.md`
