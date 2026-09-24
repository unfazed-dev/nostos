# ADR-0031: Three-mode sync rules and checksum-gated resync

**Status:** Accepted — implemented 2026-08-07 · **Date:** 2026-08-06

## Context

nostos already has the machinery to filter what a client sees: server-enforced
predicates (ADR-0011) and a dynamic predicate compiler, `PredicateExpr`
(`crates/nostos-domain/src/predicate.rs`, ADR-0012). What it lacks is an
operator-facing way to declare *what syncs* — today that's env vars
(`NOSTOS_WRITE_TABLES` for writes; nothing equivalent for reads) and whatever
predicates a caller wires up in code. The common answer to this is "sync
rules" — a declarative, versioned config surface operators edit directly.
nostos has the predicate engine such rules would compile to, but no
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

Rules reload without an engine restart, but **not** via an in-place predicate
swap — Task 14 shipped a coarser mechanism than this ADR originally proposed,
and this section now describes what actually runs.

v1 mechanism as shipped: on every rules reload, the server diffs the
old and new rule-decision for each *subscribed* table. If a decision changed
at all — narrower **or wider** — that session's socket is closed and the
client reconnects and re-snapshots under the new ruleset, exactly as a
full-restart reload would behave. A session with no subscription to any
changed table is untouched. There is no partial, per-row resnapshot and no
distinction between narrowing and widening; both pay the same full-socket
cost. See `crates/nostos-infra/src/transport.rs` (`ponytail:` at the call
site names this ceiling directly: coarse whole-socket invalidation on any
decision change, including widen; upgrade path is a real in-place predicate
swap — re-scoping the live subscription without a disconnect — once resync
churn from wide fleets makes it worth the added verification-state
machinery this ADR's original draft assumed).

This is one of the two ponytails this ADR ships with (see Consequences).

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
target `nostos::audit` (`rules_mutation actor=<8-hex> source=api
mode_before=... mode_after=... checksum_before=0x... checksum_after=0x...
tables_changed=N`), success path only. `actor` is the first 8 hex characters
of SHA-256(token) — enough to distinguish two operators in the log, not
reversible back to the token. No claim values or row data are ever logged.
`source` is always `api` as shipped: nothing on the request distinguishes the
web panel from a direct `curl`/API caller, so the field does not yet do the
distinguishing work its name implies. `ponytail:` add an `X-Nostos-Source`
header from the panel if that distinction is ever needed for audit triage
(`crates/nostos-server/src/main.rs`) — a smaller, uncounted shortcut alongside
the two below.

`PUT /rules` and `nostos rules edit` write the same file with **no
optimistic-concurrency check between them** — whichever write lands last
wins, silently. `ponytail:` last-writer-wins, upgrade path is an `ETag` /
`If-Match` precondition on `PUT /rules` once concurrent editors are common
enough to make a silent clobber costly (`crates/nostos-server/src/main.rs`).
This and the whole-socket-disconnect reload above are the two ceilings this
ADR ships with (see Consequences).

## Consequences

- **Positive:** operators get one declarative surface (`nostos_rules.toml`)
  for "what syncs" instead of ad hoc predicates wired in code; `all` gives a
  genuinely zero-config dev default without a security hole (tenant scoping
  still runs underneath); `toggles`/`hand` cover both the common case and the
  escape hatch without a third half-measure mode; an explicit `rules_checksum`
  wire field composes cleanly with the resume/resync machinery ADR-0025
  already built (old clients still work via the composed-epoch fallback)
  instead of forcing every SDK onto the new field at once; a rules edit that
  touches no table any connected client has subscribed to costs nothing —
  reload only pays for the sessions actually affected, even though "affected"
  is table-granular, not row-granular (see Reload above).
- **Negative:** reload is whole-socket, not in-place — an operator who edits
  one table's scope disconnects and re-snapshots every session subscribed to
  that table, widen or narrow alike, where a live predicate swap would have
  cost nothing for a widen and little for a narrow (see the two `ponytail:`
  entries under Reload and D5 above). `rules_checksum` is one more field
  every SDK's wire layer must eventually understand, even though old clients
  keep working unmodified. The two authoring surfaces (CLI, web panel) have
  no concurrency control between them — see the last-writer-wins `ponytail:`
  above.
- **Non-goals (v1):** `OR`/`NOT` composition, joins across tables, and
  bucket/partition grammar (bucket-style checksums, already called
  out as deferred moat machinery in ADR-0025's Divergence section) are out of
  scope — the grammar is intentionally the minimal `AND`-only subset that
  compiles to `PredicateExpr` today. A live in-place predicate swap (re-scope
  a session's subscription without disconnecting it) is out of scope for v1;
  whole-socket disconnect-and-resync on any subscribed-table decision change
  is the only mechanism, not a fallback for one.
- **`NOSTOS_WRITE_TABLES` is not governed by rules in v1.** Writes remain
  gated by the separate allowlist from ADR-0013 (`PgWriteBack`,
  `crates/nostos-infra/src/write_back.rs`). This ADR's `sync_mode`/rules
  system governs what a client can *read* (subscribe/sync); it says nothing
  about what a client can *write* — that stays a distinct gate until a later
  ADR unifies them, if one ever does.
- **Backward compatibility — the four-quadrant matrix this suite had to
  prove** (Task 12, D2):

  | | old server | new server |
  |---|---|---|
  | **old client** | unchanged | composed-epoch fallback; resyncs on a rules change via the existing `slot_epoch` mechanism (ADR-0025), never parses `rules_checksum` |
  | **new client** | server ignores the unknown `rules_checksum` key (`crates/nostos-infra/src/wire.rs` has no `deny_unknown_fields`); `resume_info` carries no checksum, so the client stores `0` and its next `Subscribe` omits the field — composed-epoch fallback | explicit `rules_checksum` path |

  Every quadrant degrades to *more snapshots*, never to a wrong or missing
  row — that invariant is what makes "missing `rules_checksum` = accept,
  log" (D2) safe. One consequence worth calling out explicitly for release
  notes: the first time any pre-ADR-0031 client reconnects to an ADR-0031
  server, it hits the **old client / new server** cell above and pays one
  full resnapshot — expected, one-time, not a regression.

## References

- Prior: ADR-0011 (server-enforced predicates), ADR-0012 (dynamic predicate
  expression engine — `PredicateExpr`, missing-column semantics), ADR-0013
  (direct write-back design — `NOSTOS_WRITE_TABLES`), ADR-0025 (persisted
  oplog backfill — `slot_epoch`/`client_epoch` resume-gate mechanism the
  `rules_checksum` field composes with via the composed-epoch fallback)
- Plan: `docs/plans/nostos-sync-streams-suite.md` — operator rulings D2
  (explicit `rules_checksum` wire field, Task 11), D3 (resync-on-reload,
  shipped as whole-socket disconnect rather than the in-place swap the
  ruling originally described — see Reload above, Task 14), and D5 (web
  authoring surface — authenticated `PUT /rules`, `NOSTOS_ADMIN_TOKEN` shape,
  Tasks 20–21), ratified 2026-08-06
- Runbook: `docs/OPERATING.md` §7 — setting, rotating, and responding to a
  leaked `NOSTOS_ADMIN_TOKEN`
- Brief: `.superpowers/sdd/nostos-sync-streams-suite/task-1-brief.md`
