# Nostos Sync-Streams Suite — Implementation Plan

**Status:** ratified, not started · **Written:** 2026-08-06 · **Owner:** implementing engineer (zero prior context assumed)

## Goal

Give Nostos a first-class answer to the sync-rules / stream-editor pattern used by comparable sync products: a
`nostos_rules.toml` file with **three mutually exclusive sync modes** (`all`,
`toggles`, `hand`), a schema-introspecting `nostos rules init`, a toggle editor
that owns the file in `toggles` mode, a restricted claims grammar that compiles
to the existing `PredicateExpr`, and checksum-triggered resync so a rules change
re-scopes clients without an engine restart.

Binding source of truth for behaviour: `docs/plans/research-sync-scoping-config-ux-2026-08-06.md`,
section **"Operator decisions (ratified 2026-08-06)"** — *except* on the five
points below.

## Operator rulings (ratified 2026-08-06) — these supersede the research doc

The research doc recommended the cheaper option on five design questions. The
operator ruled the other way on four of them. **Where this section and the
research doc disagree, this section wins.** Where neither speaks, the research
doc wins — raise it before coding.

| # | Ruling | Effect |
|---|---|---|
| **D1** | **Full flat claims.** `SupabaseJwtAuth` lifts *all* flat scalar JWT claims into `Principal.extra`. | Task 2, plus a blocking security review in the same task (hostile-JWT tests, size cap, reserved-name collisions). |
| **D2** | **Explicit wire field.** `rules_checksum` is added to `Subscribe`; the wire stays human-debuggable JSON. | Task 11 rewritten; Task 12 added (nine SDKs + backward compat). Composed epoch (Task 5) is retained as the fallback for clients that omit the field. |
| **D3** | **Whole-socket disconnect-and-resync, as shipped.** Reload diffs the old/new decision per subscribed table; any change — narrow **or** widen — closes that session's socket and the client reconnects and re-snapshots under the new ruleset. Table-granular, not row-granular; a session with no subscription to a changed table is untouched. In-place predicate swap (re-scope without disconnecting) was the original proposal but was not built — it is the documented upgrade path, not a fallback. | Task 14 shipped this. See ADR-0031 §Reload for the as-built description and the `ponytail:` upgrade path. |
| **D4** | **`sync = false` default** in `nostos rules init`; `--sync-all` flips it. | Task 15 unchanged (the plan already took this position). |
| **D5** | **Web authoring in v1.** An authenticated, config-mutating `PUT /rules` on `nostos-server`; the web panel becomes a real toggle editor. `nostos rules edit` remains. | Tasks 20, 21, 22 added/rewritten. |

Two of these (D2, D5) enlarge the blast radius materially: D2 makes the wire
contract a nine-SDK coordination problem, and D5 puts a config-mutating,
filesystem-writing route on the sync server — a genuinely new class of surface
for `nostos-server`, which has never accepted a write to its own configuration.
Task 21 exists solely to keep that surface honest and is **not** optional.

## Architecture

Hexagonal, dependencies point inward (see `CLAUDE.md` crate map):

| layer | new/changed surface |
|---|---|
| `nostos-domain` | `scope.rs` (claims-grammar template), `rules.rs` (`SyncRules`, `SyncMode`, canonicalization, checksum), `principal.rs` (extra-claims map), `sync_epoch.rs` (epoch composition) |
| `nostos-application` | `rules.rs` (`ActiveRuleset` — compiled, per-mode resolution), `ports.rs` (`RulesSource`, `TableStatsSource`) |
| `nostos-infra` | `rules_file.rs` (TOML load/save), `schema_source.rs` (`PgTableStats`), `transport.rs` (subscribe-time enforcement + epoch gate + live invalidation) |
| `nostos-server` | `main.rs` (config flags, ruleset wiring, `all`-mode startup warning, `GET /rules`, reload watcher) |
| `nostos-cli` | `commands/rules.rs` (`init` / `edit` / `check`) |
| `web` | admin toggle editor (D5) — reads `GET /rules`, writes `PUT /rules` |
| SDKs (×9) | `rules_checksum` on the `Subscribe` frame (D2) |

**Two authoring surfaces, one file (D5).** `web/` is a SvelteKit app built with
`@sveltejs/adapter-static` and deployed to Cloudflare Pages
(`web/package.json`: "marketing site + admin console … static-exported"). A
statically-exported site has no server process of its own, so it cannot write
the operator's `nostos_rules.toml` directly. D5 resolves this by putting the
write on the **sync server** instead: `PUT /rules` (Task 20), authenticated and
audited (Task 21), with the browser as a pure client of that route.

That makes `nostos_rules.toml` a file with two writers — the CLI editor
(`nostos rules edit`, Tasks 15–17) and the server route. Both must therefore:
- write **atomically** (`write to <path>.tmp` + `rename`), so a half-written
  file is never observable by the reload watcher;
- validate **before** writing, using the identical `check_report` logic
  (Task 17), so the two surfaces cannot disagree about what is valid;
- leave the inactive section byte-preserved (`rules_file::save`, Task 7).

There is deliberately **no** file locking between them: last-writer-wins on a
single operator's config file is acceptable, and the audit log (Task 21) makes
the sequence reconstructible. This is a `ponytail:` ceiling recorded in
ADR-0031 — the upgrade path is an `If-Match` precondition on the checksum,
which Task 20 already returns.

## Tech Stack

Rust 2021 workspace (`rust-toolchain.toml`), tokio, axum, clap (derive),
serde/serde_json, `toml` crate (already a `nostos-cli` dependency; add to
`nostos-infra`), `tokio-postgres` for introspection, SvelteKit 5 for the
read-only admin panel. No new third-party crates beyond moving `toml` into
`nostos-infra`'s dependency list.

---

## Global Constraints

Apply to **every** task below. Violations fail review.

1. `unsafe` is forbidden workspace-wide. Every new module inherits
   `#![forbid(unsafe_code)]` from its crate root; do not add `unsafe` anywhere.
2. Clippy pedantic is on and CI fails on warnings. Run `make ci`
   (fmt-check + clippy `-D warnings` + full test suite) **before every commit**.
   A task is not done until `make ci` is green.
3. Commits: single line, conventional prefix, no author mentions, no co-author
   trailers. Example: `feat: compile nostos_rules.toml scopes into PredicateExpr`.
4. Hexagonal dependency direction: `domain` depends on nothing; `application`
   on domain only; `infra` on application+domain; `server` on all. Never import
   `nostos-infra` from `nostos-application`, never import anything I/O-shaped
   (tokio, axum, toml, postgres) into `nostos-domain`.
5. `nostos-domain` stays pure: zero I/O, zero async. Parsing a string into a
   value type is pure and belongs there; reading a file is not and does not.
6. The wire protocol stays human-debuggable JSON (`crates/nostos-infra/src/wire.rs`).
   D2 adds exactly one field — `rules_checksum` on `Subscribe` (Task 11) — as a
   JSON string, not a binary or packed encoding. Adding any *other* wire field
   is out of scope.
6b. **Backward compatibility is non-negotiable.** A client that omits
   `rules_checksum` must keep working against a new server, and a new client
   must keep working against an old server. Every task that touches the wire
   proves both directions with a test, not a comment.
7. Do not edit any performance claim. `benches/results/RESULTS.md`,
   `README.md` throughput numbers, `docs/STRATEGY.md` multiples: untouched.
   No task here re-runs or re-quotes the benchmark.
8. Deliberate shortcuts carry a `ponytail:` comment naming the ceiling and the
   upgrade path. Two are pre-authorised: the composed-epoch fallback for
   pre-D2 clients (Task 11) and last-writer-wins between the two authoring
   surfaces (Task 20). Do not add more without recording them in the ADR.
9. Code cites its ADR. Every new module's doc comment names `ADR-0031`.
10. **Fail closed.** Any ambiguity in rule evaluation (missing claim, unknown
    table, unparsable scope) resolves to *deny*, never to `PredicateExpr::any()`.
11. `all` mode disables **rules**, not **security**. Tenant scoping
    (`Principal::tenant_scope`, ADR-0011) is ANDed in on every path in every
    mode. There is no code path in this plan where a subscribe skips it.

---

## Non-Goals (deferred — do not implement)

- JOINs / relational scoping ("sync a task if its project is mine").
- Column masking / per-column allowlists.
- `OR` and `NOT` in the v1 rules grammar (the underlying `PredicateExpr` and
  `parse_predicate_expr` support them; the *rules* grammar accepts `AND` only).
- `IN`, `LIKE`, `BETWEEN`, `IS NULL`, aggregates, subqueries.
- Bucket/partition modelling, priority buckets, or bucket-based
  `bucket_definitions`-style configuration.
- Multi-file rule includes, per-environment rule overlays, rule inheritance.
- A hosted/multi-tenant rules editor, rule versioning history, or rollback UI.
- Coupling rules to writes: `NOSTOS_WRITE_TABLES` remains an independent
  server-side allowlist. Rules govern **read scope only** in v1.
- Multi-operator concurrency control on `nostos_rules.toml` (no locking, no
  `If-Match`; last-writer-wins — see Task 20).
- Rules-change history, diffing, or rollback in either authoring surface. The
  audit log (Task 21) records *that* a change happened and by whom, not a
  restorable snapshot.
- Any **breaking** SDK API change. D2 touches the `Subscribe` frame's
  serialization plus two *defaulted, additive* methods on `nostos-core`'s public
  `Storage` trait (`rules_checksum` / `save_rules_checksum`, Task 12). Additive
  with defaults = every existing `Storage` impl compiles untouched. What stays
  out of scope: changed method signatures, new SDK-facing types, and any change
  to the nine SDKs' own public surfaces — `rules_checksum` never crosses the
  FFI boundary.

---

## Task 1 — ADR-0031: three-mode sync rules

**Files**
- Create: `docs/adr/0031-sync-rules-modes-and-checksum-resync.md`
- Modify: `docs/adr/README.md` *(only if it exists and carries an index; check
  with `ls docs/adr/README.md` — skip silently if absent)*

**Content requirements** (the ADR is the contract every later task cites):

1. **Context** — nostos has server-enforced predicates (ADR-0011) and a dynamic
   predicate compiler (ADR-0012) but no operator-facing way to declare *what
   syncs*. Comparable sync products have declarative sync rules; nostos has env vars.
2. **Decision — three mutually exclusive modes**, `sync_mode` in
   `nostos_rules.toml`:
   - `all` — everything replicated is synced. No rules evaluated; equivalent to
     an implicit `select *` per table. Zero-config dev default. Guardrail:
     startup warning with introspected table count + row-count estimate.
     **Tenant scoping still applies** (ADR-0011) — `all` disables rules, not
     security.
   - `toggles` — schema introspection + toggle editor generate the
     `[tables.*]` section; **that section is the truth**.
   - `hand` — the `[[rules]]` section is hand-authored and is **the truth**;
     the generator is frozen and must not write `[tables.*]` while active.
   - The modes are exclusive: exactly one section is compiled per boot.
3. **Truth-switching semantics** — switching modes moves the source of truth.
   Entering `hand` freezes the generator (`nostos rules edit` refuses to write).
   Switching back to `toggles` deactivates the hand section (it is *ignored*,
   never deleted) and the toggle section becomes truth again. `all` ignores
   both sections and **must never delete either**.
4. **Versioned rules + checksum resync** — `version` field plus a canonical
   checksum over `(sync_mode, canonicalized active section)`. The checksum is
   folded into the epoch the server advertises at subscribe, so a mode flip
   alone forces a resync (Task 11). Canonicalization (not raw file bytes) means
   a whitespace or key-order edit does **not** resnapshot the fleet.
5. **Grammar (v1)** — `column <op> claims.<field>` or `column <op> <literal>`,
   ops `= != < > <= >=`, composed with `AND` only. Compiles to the existing
   `PredicateExpr`. Missing claim at request time → **deny the table** (empty
   result), never `any()`.
6. **Reload** — rules reload without an engine restart. v1 mechanism:
   checksum change → live sessions are closed with a reason and reconnect into
   the new ruleset (Task 14). Record as a `ponytail:` ceiling: in-place
   predicate swap without a disconnect is the upgrade path.
7. **Consequences** — lists the Non-Goals above; states that
   `NOSTOS_WRITE_TABLES` is not governed by rules in v1.
8. **Status:** Accepted. **Date:** 2026-08-06.

**Interfaces produced:** none (documentation). Every later task's module doc
comment must reference `ADR-0031`.

**Steps**
1. `ls docs/adr/` and confirm `0030-crdt-merge-tier.md` is the highest number;
   the new file is `0031-…`. If a higher number exists, use the next free one
   and update every "ADR-0031" reference in this plan's later tasks.
2. Write the ADR following the structure of `docs/adr/0025-persisted-oplog-backfill-for-reconnect-resume.md`
   (Context / Decision / Consequences / Status).
3. `make ci` (docs-only, but the gate is unconditional).
4. Commit: `docs: add ADR-0031 for three-mode sync rules and checksum resync`.

---

## Task 2 — Domain: extra claims on `Principal`

**Files**
- Modify: `crates/nostos-domain/src/principal.rs`
- Modify: `crates/nostos-infra/src/auth.rs` (populate from the verified JWT)
- Test: inline `#[cfg(test)]` in both files

**Why:** `Principal` currently carries only `account_id` and `tenant_id`
(`crates/nostos-domain/src/principal.rs:24`), and `SupabaseJwtAuth` lifts only
`sub`. The rules grammar references `claims.<field>`, so arbitrary flat claims
must survive verification.

**Interfaces produced**

```rust
// crates/nostos-domain/src/principal.rs
pub struct Principal {
    pub account_id: String,
    pub tenant_id: String,
    /// Flat, string-valued JWT claims beyond `sub`, keyed by claim name.
    /// Populated by the auth adapter; empty for `Principal::anonymous`.
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

impl Principal {
    pub fn with_claims(
        account_id: impl Into<String>,
        tenant_id: impl Into<String>,
        extra: std::collections::BTreeMap<String, String>,
    ) -> Self;

    /// Resolve a `claims.<field>` reference. `sub` → `account_id`,
    /// `tenant_id` → `tenant_id`, anything else → `extra`.
    /// `None` means "this principal has no such claim" → caller denies.
    #[must_use]
    pub fn claim(&self, field: &str) -> Option<&str>;
}
```

`Principal::new` keeps its two-argument signature and sets `extra` empty, so no
existing call site changes.

**Interfaces consumed:** none.

**Steps**
1. Failing test in `principal.rs`: `claim_resolves_sub_tenant_and_extra` —
   `Principal::with_claims("u1", "t1", [("org_id","acme")])` returns
   `Some("u1")` for `"sub"`, `Some("t1")` for `"tenant_id"`, `Some("acme")` for
   `"org_id"`, `None` for `"role"`.
2. Failing test: `anonymous_has_no_claims` — `Principal::anonymous().claim("sub")`
   is `None` (empty `account_id` must not resolve to an empty-string claim).
3. Implement. Keep `#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]`
   working — `BTreeMap<String, String>` satisfies all of them.
4. Failing test in `crates/nostos-infra/src/auth.rs`:
   `jwt_lifts_flat_string_claims` — an HS256 token with `{"sub":"u1","org_id":"acme","role":"admin","n":7}`
   yields `extra == {"org_id":"acme","role":"admin","n":"7"}` (JSON numbers and
   booleans stringify; objects and arrays are skipped).
5. Implement by widening `SupabaseClaims` with
   `#[serde(flatten)] rest: serde_json::Map<String, serde_json::Value>` and
   filtering to scalars. `exp`/`sub` stay explicit fields.
6. **Complete the security review below before committing.**
7. `make ci`.
8. Commit: `feat: carry flat JWT claims on Principal for rules scoping`.

### Security review (D1 — blocking, same task, same commit)

D1 turns an attacker-controlled document (the JWT payload) into a map that is
consulted during predicate construction. The token is signature-verified
first — an attacker cannot forge claims without the HS256 secret — but a
*legitimately issued* token from a self-signup Supabase project can still carry
arbitrary user-controlled claim names and values. These four properties are the
review, and each is a test, not a code comment.

**Interfaces produced**

```rust
// crates/nostos-infra/src/auth.rs
/// Max number of lifted claims. Beyond this the token is REJECTED (not
/// truncated — a silently truncated claim set could drop the claim a scope
/// depends on and change the meaning of a rule).
const MAX_EXTRA_CLAIMS: usize = 64;
/// Max byte length of any single lifted claim name or value. Same rejection
/// rule and same reason.
const MAX_CLAIM_LEN: usize = 1024;
/// Claim names that may never enter `extra`, because `Principal::claim`
/// resolves them from the typed fields and a duplicate would be ambiguous.
const RESERVED_CLAIMS: [&str; 4] = ["sub", "tenant_id", "exp", "iat"];
```

**Steps**
1. Failing test `nested_and_array_claims_are_dropped` — a token with
   `{"org": {"id": "acme"}, "roles": ["a","b"], "ok": "yes"}` lifts **only**
   `ok`. Objects and arrays are dropped, never stringified into JSON text (a
   stringified object would let `scope = "col = claims.org"` compare against
   `{"id":"acme"}` and silently never match).
2. Failing test `null_claims_are_dropped` — `{"x": null}` does not produce
   `x = ""`. An empty-string claim would make `col = claims.x` match rows with
   an empty column, which is a real widening.
3. Failing test `oversized_claim_set_is_rejected` — 65 claims → the whole token
   is rejected with an auth error; the connection is refused, not downgraded to
   anonymous.
4. Failing test `oversized_claim_value_is_rejected` — a 1 KiB+ value → same.
5. Failing test `reserved_claim_names_cannot_shadow` — a token carrying
   `{"sub": "u1", "tenant_id": "evil"}` in the flattened rest must **not**
   overwrite the `tenant_id` the tenant-resolution path derived. Assert
   `principal.tenant_id` is unchanged **and** `principal.claim("tenant_id")`
   returns the derived value, not the payload's. This is the one that turns a
   claims map into a tenant-escape hole if it is missed.
6. Failing test `claims_do_not_leak_into_logs` — construct a `Principal` with
   `extra = {"secret_token": "hunter2"}` and assert its `Debug` rendering does
   not contain `hunter2`. Implement by hand-writing `impl fmt::Debug for
   Principal` that prints `extra` as a sorted list of **names only**. Remove
   `Debug` from the derive list when doing so.
7. Implement all six.
8. Re-read every existing `tracing::` call site that formats a `Principal`
   (`git grep -n "principal" -- crates/nostos-infra/src crates/nostos-server/src`)
   and confirm none of them formats `extra` by another route.
9. `make ci`.

---

## Task 3 — Domain: the scope grammar (`ScopeExpr`)

**Files**
- Create: `crates/nostos-domain/src/scope.rs`
- Modify: `crates/nostos-domain/src/lib.rs` (add `pub mod scope;` + re-exports)
- Test: inline `#[cfg(test)]` in `scope.rs`

**Why:** `parse_predicate_expr` (`crates/nostos-domain/src/predicate_compile.rs:64`)
parses literals only — it has no notion of `claims.<field>`. Rules need a
*template* that is parsed once at config load and resolved per session.

**Interfaces produced**

```rust
// crates/nostos-domain/src/scope.rs — cites ADR-0031
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("empty scope expression")]
    Empty,
    #[error("unexpected token `{0}`")]
    UnexpectedToken(String),
    #[error("`{0}` is not supported in a v1 rules scope (AND-composition only)")]
    Unsupported(String),
    #[error("expected a comparison operator after `{0}`")]
    MissingOperator(String),
    #[error("expected a value after `{0}`")]
    MissingValue(String),
}

/// A resolved-at-request-time reference to a principal claim, or a constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeValue {
    Claim(String),       // `claims.org_id`
    Literal(ColumnValue),// `'open'`, `3`, `true`
}

/// One `column <op> value` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeTerm {
    pub column: String,
    pub op: ScopeOp,
    pub value: ScopeValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeOp { Eq, Ne, Lt, Gt, Le, Ge }

/// A parsed scope: one or more terms, AND-composed. Empty vec == unscoped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScopeExpr {
    pub terms: Vec<ScopeTerm>,
}

impl ScopeExpr {
    /// Parse the v1 grammar:
    ///   scope      := comparison ( "AND" comparison )*
    ///   comparison := IDENT OP ( "claims." IDENT | literal )
    ///   OP         := "=" | "!=" | "<" | ">" | "<=" | ">="
    /// `OR`, `NOT`, and parentheses are rejected with `Unsupported`.
    pub fn parse(input: &str) -> Result<Self, ScopeError>;

    /// Resolve against a principal. `None` = **deny this table**: a referenced
    /// claim is absent, so no row can be proven in scope. `Some(any())` is
    /// returned only for a genuinely empty scope (`terms.is_empty()`).
    pub fn resolve(&self, principal: &Principal) -> Option<PredicateExpr>;

    /// The claim names this scope references, sorted and deduped
    /// (consumed by `nostos rules check`, Task 17).
    #[must_use]
    pub fn referenced_claims(&self) -> Vec<&str>;

    /// Stable text form used by the checksum. Uppercase `AND`, single space
    /// around operators, terms sorted by `(column, op, value)`.
    #[must_use]
    pub fn canonical(&self) -> String;
}
```

**Interfaces consumed:** `crate::predicate::{ColumnValue, PredicateExpr}`,
`crate::principal::Principal`.

Literal typing matches `predicate_compile.rs`: bare integer → `Number`,
decimal → `Float`, `true`/`false` → `Bool`, `'quoted'` or bare ident → `Text`.
A claim value always resolves to `ColumnValue::Text` (JWT claims are strings) —
document that a numeric comparison against a claim compares text.

**Steps**
1. Failing tests, in this order:
   - `parses_single_claim_term` — `"owner_id = claims.sub"` → one term,
     `ScopeValue::Claim("sub")`.
   - `parses_and_composition` — `"org_id = claims.org_id AND status != 'archived'"`
     → two terms, second is a `Literal(Text("archived"))`.
   - `rejects_or` / `rejects_not` / `rejects_parens` → `ScopeError::Unsupported`.
   - `rejects_lone_operator` → `MissingValue`/`MissingOperator`.
   - `resolve_missing_claim_denies` — scope references `claims.org_id`,
     principal has no `org_id` → `resolve` returns `None` (**not** `any()`).
     This is the fail-closed guarantee; it is a test, not a comment.
   - `resolve_builds_and_chain` — two terms → `PredicateExpr::And`.
   - `canonical_is_order_insensitive` — two scopes differing only in term
     order and whitespace produce the identical `canonical()` string.
2. Implement with a hand-rolled tokenizer + linear parse (no recursion needed:
   AND-only). Reuse `predicate_compile.rs`'s literal-typing logic by copying
   the small `classify_literal` behaviour into `scope.rs` — do **not** make
   `predicate_compile` public-API-unstable for this.
3. `make ci`.
4. Commit: `feat: add claims-scoped rules grammar to nostos-domain`.

---

## Task 4 — Domain: the rules model, canonicalization, checksum

**Files**
- Create: `crates/nostos-domain/src/rules.rs`
- Modify: `crates/nostos-domain/src/lib.rs` (`pub mod rules;` + re-exports)
- Test: inline `#[cfg(test)]` in `rules.rs`

**Why:** the file shape and its checksum are pure data; they belong in domain so
both the server and the CLI compute an identical checksum.

**Interfaces produced**

```rust
// crates/nostos-domain/src/rules.rs — cites ADR-0031
pub const RULES_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    #[default]
    All,
    Toggles,
    Hand,
}

impl SyncMode {
    /// `"all" | "toggles" | "hand"` (lowercase, the on-disk spelling).
    #[must_use] pub fn as_str(self) -> &'static str;
    pub fn parse(s: &str) -> Option<Self>;
}

/// One generator-owned table entry (`[tables.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRule {
    pub table: String,
    pub sync: bool,
    /// Raw scope text; `None` or empty = whole table (still tenant-scoped).
    pub scope: Option<String>,
}

/// One hand-authored rule (`[[rules]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandRule {
    pub table: String,
    pub scope: Option<String>,
}

/// The whole `nostos_rules.toml`, parsed but not yet compiled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncRules {
    pub version: u32,
    pub mode: SyncMode,
    pub tables: Vec<TableRule>,
    pub hand: Vec<HandRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RulesError {
    #[error("unsupported rules version {0} (this server understands {RULES_VERSION})")]
    UnsupportedVersion(u32),
    #[error("unknown sync_mode `{0}` (expected all|toggles|hand)")]
    UnknownMode(String),
    #[error("table `{table}`: {source}")]
    Scope { table: String, source: ScopeError },
    #[error("duplicate table `{0}`")]
    DuplicateTable(String),
}

impl SyncRules {
    /// Structural validation + every active-section scope parses.
    /// Inactive sections are NOT validated (a stale hand section must not
    /// block `toggles` mode).
    pub fn validate(&self) -> Result<(), RulesError>;

    /// Canonical text over `(version, mode, ACTIVE section only)`:
    /// tables sorted by name, scopes via `ScopeExpr::canonical()`,
    /// one `table\tsync\tscope` line each. `All` canonicalizes to just the
    /// mode line, so toggling table entries under `all` does not resync.
    #[must_use] pub fn canonical(&self) -> String;

    /// FNV-1a 64 of `canonical()`. Stable across processes and machines.
    #[must_use] pub fn checksum(&self) -> u64;
}
```

**Interfaces consumed:** `crate::scope::{ScopeExpr, ScopeError}`.

**Steps**
1. Failing tests:
   - `checksum_ignores_whitespace_and_order` — two `SyncRules` differing only in
     table order and scope whitespace share a checksum.
   - `checksum_changes_with_mode` — same tables, `Toggles` vs `Hand` vs `All` →
     three distinct checksums (a mode flip alone must resync).
   - `all_mode_checksum_ignores_sections` — under `All`, adding a `TableRule`
     does not change the checksum.
   - `validate_rejects_bad_scope` → `RulesError::Scope`.
   - `validate_ignores_inactive_section` — mode `Toggles` with a syntactically
     broken `[[rules]]` entry validates OK.
   - `validate_rejects_duplicate_table`.
   - `validate_rejects_future_version`.
2. Implement. FNV-1a is ~10 lines; do not add a hashing crate.
3. `make ci`.
4. Commit: `feat: add versioned sync-rules model with canonical checksum`.

---

## Task 5 — Domain: composed sync epoch

**Files**
- Create: `crates/nostos-domain/src/sync_epoch.rs`
- Modify: `crates/nostos-domain/src/lib.rs`
- Test: inline `#[cfg(test)]`

**Interfaces produced**

```rust
// crates/nostos-domain/src/sync_epoch.rs — cites ADR-0031 + ADR-0025
/// Fold the replication-slot epoch (ADR-0025 slice 4b) and the rules checksum
/// (ADR-0031) into the single `u64` the server advertises at subscribe.
/// A change in EITHER input changes the result, so a rules edit invalidates a
/// client's resume exactly the way a slot recreate does.
#[must_use]
pub fn compose_sync_epoch(slot_epoch: u64, rules_checksum: u64) -> u64;
```

**Steps**
1. Failing tests: `distinct_inputs_distinct_epochs` (vary each input
   independently), `is_deterministic`, `zero_checksum_is_not_identity`
   (`compose(5, 0) != 5` — an old client's persisted raw slot epoch must not
   accidentally match).
2. Implement as FNV-1a over the two little-endian u64s (reuse the helper from
   Task 4 — move it to a private `fnv` module shared by `rules.rs` and
   `sync_epoch.rs`).
3. `make ci`.
4. Commit: `feat: compose slot epoch and rules checksum into one sync epoch`.

---

## Task 6 — Application: `ActiveRuleset` + ports

**Files**
- Create: `crates/nostos-application/src/rules.rs`
- Modify: `crates/nostos-application/src/lib.rs` (`pub mod rules;` + re-export)
- Modify: `crates/nostos-application/src/ports.rs` (add `TableStatsSource`)
- Test: inline `#[cfg(test)]` in `rules.rs`

**Interfaces produced**

```rust
// crates/nostos-application/src/rules.rs — cites ADR-0031
/// A validated, pre-compiled ruleset. Built once per load; cheap to consult
/// per subscribe (no parsing on the hot path).
#[derive(Debug, Clone)]
pub struct ActiveRuleset {
    mode: SyncMode,
    checksum: u64,
    /// table -> compiled scope. Absent under `All`.
    scopes: std::collections::BTreeMap<String, ScopeExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleDecision {
    /// Subscribe allowed; AND this into the session predicate.
    Allow(PredicateExpr),
    /// Table is toggled off / not listed → close the socket with this reason.
    DeniedTable,
    /// A scope claim is missing on this principal → deny (fail closed).
    DeniedClaim(String),
}

impl ActiveRuleset {
    /// Compile from a validated `SyncRules`. Selects the active section by
    /// mode; the inactive section is dropped, never consulted.
    pub fn compile(rules: &SyncRules) -> Result<Self, RulesError>;

    /// The permissive zero-config ruleset (`sync_mode = "all"`), used when no
    /// `nostos_rules.toml` exists.
    #[must_use] pub fn all_mode() -> Self;

    #[must_use] pub fn mode(&self) -> SyncMode;
    #[must_use] pub fn checksum(&self) -> u64;

    /// Decide one subscribe. Under `All`, always `Allow(PredicateExpr::any())`
    /// — the caller still ANDs tenant scoping in (ADR-0011); this function
    /// deliberately knows nothing about tenants.
    #[must_use]
    pub fn decide(&self, table: &str, principal: &Principal) -> RuleDecision;

    /// Table names the ruleset syncs, for logs and `GET /rules`.
    #[must_use] pub fn synced_tables(&self) -> Vec<&str>;
}
```

```rust
// crates/nostos-application/src/ports.rs — appended
/// Estimated size of one replicated table, for the `all`-mode startup warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStat {
    pub table: String,
    /// `None` when the source cannot estimate (e.g. Postgres `reltuples = -1`
    /// on a never-analyzed table). Render as "unknown", never as a number.
    pub estimated_rows: Option<u64>,
}

#[async_trait::async_trait]
pub trait TableStatsSource: Send + Sync {
    async fn table_stats(&self) -> Result<Vec<TableStat>, SchemaError>;
}
```

**Interfaces consumed:** `nostos_domain::{Principal, PredicateExpr, ScopeExpr, SyncRules, SyncMode, RulesError}`.

**Steps**
1. Failing tests:
   - `all_mode_allows_any_table` — `all_mode().decide("anything", &p)` is
     `Allow(any())`.
   - `toggles_denies_unlisted_table` → `DeniedTable`.
   - `toggles_denies_sync_false` → `DeniedTable`.
   - `toggles_allows_with_compiled_scope` — scope `owner_id = claims.sub`
     against principal `u1` → `Allow(PredicateExpr::eq("owner_id", Text("u1")))`.
   - `missing_claim_denies` → `DeniedClaim("org_id")`, and assert the result is
     **not** any `Allow` variant.
   - `hand_mode_uses_hand_section_only` — a `SyncRules` whose `[tables.*]`
     allows `notes` and whose `[[rules]]` allows only `tasks`, compiled in
     `Hand` mode, denies `notes` and allows `tasks`.
   - `toggles_mode_ignores_hand_section` — the mirror of the above.
   - `checksum_matches_domain` — `ActiveRuleset::compile(&r).checksum() == r.checksum()`.
   - `all_mode_helper_matches_compiled_all_mode` —
     `ActiveRuleset::all_mode().checksum()` equals
     `ActiveRuleset::compile(&SyncRules { version: RULES_VERSION, mode: All, .. }).checksum()`,
     including when that `SyncRules` carries table entries. Without this,
     `nostos rules init --mode all` resnapshots the entire fleet for a semantic
     no-op — exactly what the Task 4 canonicalization exists to prevent.
2. Implement.
3. `make ci`.
4. Commit: `feat: compile sync rules into a per-session decision engine`.

---

## Task 7 — Infra: `nostos_rules.toml` load/save

**Files**
- Create: `crates/nostos-infra/src/rules_file.rs`
- Modify: `crates/nostos-infra/src/lib.rs` (`pub mod rules_file;`)
- Modify: `crates/nostos-infra/Cargo.toml` (add `toml = { workspace = true }`)
- Test: inline `#[cfg(test)]` using `std::env::temp_dir()` + a uuid-suffixed dir

**On-disk format** (the exact file the CLI writes and the server reads):

```toml
# nostos_rules.toml — generated by `nostos rules init` (ADR-0031).
# sync_mode selects WHICH section below is the truth:
#   all     — sync everything; neither section is read (both are preserved)
#   toggles — [tables.*] is the truth; `nostos rules edit` owns it
#   hand    — [[rules]] is the truth; the generator is frozen
version = 1
sync_mode = "toggles"

[tables.tasks]
sync = true
scope = "owner_id = claims.sub"

[tables.notes]
sync = false

[[rules]]
table = "tasks"
scope = "org_id = claims.org_id AND status != 'archived'"
```

**Interfaces produced**

```rust
// crates/nostos-infra/src/rules_file.rs — cites ADR-0031
/// Default file name, resolved relative to the process working directory.
pub const RULES_FILE_NAME: &str = "nostos_rules.toml";

#[derive(Debug, thiserror::Error)]
pub enum RulesFileError {
    #[error("reading {path}: {source}")] Io { path: String, source: std::io::Error },
    #[error("malformed {path}: {source}")] Malformed { path: String, source: toml::de::Error },
    #[error("invalid rules in {path}: {source}")] Invalid { path: String, source: RulesError },
    #[error("serializing rules: {0}")] Serialize(#[from] toml::ser::Error),
}

/// Read + validate. `Ok(None)` when the file does not exist (caller falls back
/// to `ActiveRuleset::all_mode()` — zero-config dev default).
pub fn load(path: &std::path::Path) -> Result<Option<SyncRules>, RulesFileError>;

/// Write with the banner comment above. Overwrites; preserves BOTH sections
/// regardless of mode (truth-switching must never delete an artifact).
pub fn save(path: &std::path::Path, rules: &SyncRules) -> Result<(), RulesFileError>;

/// Rewrite ONLY `sync_mode`, leaving both sections byte-identical.
/// Used by `nostos rules edit --mode` and by mode-switch tests.
pub fn set_mode(path: &std::path::Path, mode: SyncMode) -> Result<(), RulesFileError>;
```

**Steps**
1. Failing tests:
   - `round_trips_both_sections` — save → load returns an equal `SyncRules`
     with both `tables` and `hand` populated.
   - `missing_file_is_none` (not an error).
   - `malformed_toml_is_malformed_error` (message names the path).
   - `set_mode_preserves_sections` — write toggles+hand, `set_mode(Hand)`,
     reload: both sections intact, only `sync_mode` changed.
   - `load_rejects_future_version`.
2. Implement with a private serde mirror type (`#[derive(Serialize, Deserialize)]`)
   that maps `[tables.<name>]` (a `BTreeMap<String, TableEntry>`) to the domain's
   `Vec<TableRule>`. Domain types stay serde-free of TOML specifics.
3. `make ci`.
4. Commit: `feat: read and write nostos_rules.toml`.

---

## Task 8 — Infra: Postgres table stats

**Files**
- Modify: `crates/nostos-infra/src/schema_source.rs` (add `PgTableStats`)
- Test: `crates/nostos-infra/tests/e2e_pg_table_stats.rs` (real-PG, self-skips
  without `NOSTOS_E2E_PG=1` — follow the guard pattern in
  `crates/nostos-infra/tests/e2e_pg_schema.rs`)

**Interfaces produced**

```rust
// crates/nostos-infra/src/schema_source.rs
/// Row-count estimates for the tables in a publication, for the `all`-mode
/// startup warning (ADR-0031). Estimates only: reads `pg_class.reltuples`,
/// never `count(*)` (a full scan at boot is unacceptable).
pub struct PgTableStats { /* pg_url, publication */ }

impl PgTableStats {
    pub fn new(pg_url: &str, publication: &str) -> Self;
}

#[async_trait::async_trait]
impl TableStatsSource for PgTableStats {
    async fn table_stats(&self) -> Result<Vec<TableStat>, SchemaError>;
}
```

Query shape (join the publication to `pg_class`):

```sql
SELECT c.relname,
       c.reltuples
  FROM pg_publication_tables t
  JOIN pg_class c ON c.relname = t.tablename
 WHERE t.pubname = $1
 ORDER BY c.relname
```

`reltuples < 0.0` (never analyzed) → `estimated_rows: None`. Never emit a
negative or a coerced-to-zero count.

**Steps**
1. Failing e2e test `reltuples_estimate_or_unknown`: create two tables in the
   publication, `ANALYZE` one, leave the other untouched; assert the analyzed
   one has `Some(_)` and the un-analyzed one is `None` **or** `Some(_)` (PG may
   have autovacuumed) — the hard assertion is *never negative, never a panic*.
2. Implement, reusing the TLS/connection setup already in `PgSchemaSource::new`.
3. Run `docker compose -f docker/docker-compose.yml up -d`, then
   `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn cargo test -p nostos-infra --features pg`.
   Confirm the test actually ran (without `NOSTOS_E2E_PG=1` it self-skips and
   reports a false pass).
4. `make ci`.
5. Commit: `feat: estimate replicated table sizes from pg_class reltuples`.

---

## Task 9 — Server: load rules, wire into router state

**Files**
- Modify: `crates/nostos-server/src/main.rs` (config field + load + wiring)
- Modify: `crates/nostos-infra/src/transport.rs` (`SyncRouterState` fields +
  builder)
- Modify: `crates/nostos-cli/src/commands/dev.rs` (pass `NOSTOS_RULES_FILE`)
- Test: inline `#[cfg(test)]` in `transport.rs` for the builder default;
  inline `#[cfg(test)]` in `dev.rs` for the env pair

**Interfaces produced**

```rust
// crates/nostos-infra/src/transport.rs — SyncRouterState gains:
pub struct SyncRouterState {
    // …existing fields (manager, session_buffer, metrics, auth, tenant_column,
    // write_back, write_tables, snapshotter, schema_source, oplog_reader)…
    /// The active ruleset (ADR-0031). Swapped at runtime by the reload watcher
    /// (Task 14); read at SUBSCRIBE time only — never per delivered event.
    /// Defaults to `ActiveRuleset::all_mode()`.
    pub rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
    /// Broadcasts the active ruleset's checksum. A per-connection
    /// `Receiver::changed()` arm is free while unchanged, so live-session
    /// invalidation (Task 14) costs nothing on the delivery path.
    /// Defaults to a channel seeded with `ActiveRuleset::all_mode().checksum()`.
    pub rules_changed: tokio::sync::watch::Receiver<u64>,
}

impl SyncRouterState {
    pub fn with_rules(
        mut self,
        rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
        rules_changed: tokio::sync::watch::Receiver<u64>,
    ) -> Self;
}
```

```rust
// crates/nostos-server/src/main.rs — Config gains (clap derive, matching the
// existing style at fields `write_tables` / `or_set_columns`):
/// Path to the sync-rules file (ADR-0031). Missing file = `all` mode.
#[arg(long, env = "NOSTOS_RULES_FILE", default_value = "nostos_rules.toml")]
rules_file: String,
```

Rules state lives on `SyncRouterState`, **not** on `Metrics` — `Metrics` is a
gauge bag (`crates/nostos-application/src/ports.rs:666`) and must stay one.

**Steps**
1. Failing test in `transport.rs`: `router_state_defaults_to_all_mode` —
   `SyncRouterState::new(manager, auth)` yields a state whose
   `rules.read().await.mode() == SyncMode::All`.
2. Implement the state field + builder (default `all_mode()` so no existing
   construction site breaks).
3. **`nostos dev` must carry the path explicitly.** `commands/dev.rs:27` builds
   the child env from `NostosConfig::server_env` (`crates/nostos-cli/src/config.rs:144`),
   which knows nothing about the project directory; the child inherits the
   CLI's cwd, so the `nostos_rules.toml` default resolves *by coincidence*. Make
   it explicit: in `dev.rs`, after `let env_pairs = cfg.server_env(...)`, push
   `("NOSTOS_RULES_FILE", cwd.join("nostos_rules.toml").display().to_string())`.
   Do **not** widen `server_env`'s signature (it is also used by
   `commands/deploy.rs`, where the rules file ships inside the image and the
   default is correct). Failing test first:
   `dev_env_includes_absolute_rules_file_path`.
4. In `main.rs`, after config parse and before the router is built: call
   `nostos_infra::rules_file::load(Path::new(&cfg.rules_file))`; on `Ok(Some(r))`
   compile and log `sync_mode=<mode> tables=<n> checksum=<hex>`; on `Ok(None)`
   log `no nostos_rules.toml found; sync_mode=all (zero-config default)`; on
   `Err(e)` **exit non-zero with the error** — a malformed rules file must not
   silently degrade to "sync everything". Create the
   `tokio::sync::watch::channel(ruleset.checksum())` here and hand the
   `Receiver` to `SyncRouterState::with_rules`; keep the `Sender` for the
   Task 14 watcher.
5. `make ci`.
6. Commit: `feat: load nostos_rules.toml at server boot`.

---

## Task 10 — Infra: enforce rules at subscribe

**Files**
- Modify: `crates/nostos-infra/src/transport.rs` (`build_predicate`, ~line 971,
  and its two call sites — the first-table path near line 348 and the
  mid-session subscribe path near line 545)
- Test: inline `#[cfg(test)]` in `transport.rs`; contract test in
  `crates/nostos-infra/tests/ws_contract.rs`

**Interfaces produced**

```rust
// crates/nostos-infra/src/transport.rs — build_predicate gains the ruleset and
// a failure mode. Signature (replacing the current infallible one):
fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<Predicate, SubscribeRejection>;

/// Why a subscribe was refused. Rendered into the close reason.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubscribeRejection {
    /// `"table `notes` is not synced by the active rules (sync_mode=toggles)"`
    NotSynced { table: String, mode: SyncMode },
    /// `"missing claim `org_id` required by the rules for table `tasks`"`
    MissingClaim { table: String, claim: String },
    /// `"invalid where_sql: <ParseError>"` — the existing ADR-0012 rejection,
    /// folded into this enum so all refusals share one path.
    InvalidWhereSql(String),
}
```

Composition order (all three ANDed, none skippable):
`rules scope` **AND** `tenant scope (ADR-0011)` **AND** `client filters + where_sql (ADR-0012)`.

**Steps**
1. Failing tests:
   - `subscribe_to_unsynced_table_is_rejected` — `toggles` mode, `notes`
     `sync = false` → `Err(NotSynced)`; over the socket, the connection closes
     with that reason and **no event frame is ever sent** (same shape as the
     existing invalid-`where_sql` rejection).
   - `rules_scope_is_anded_with_tenant_scope` — ruleset scope
     `status = 'open'` + tenant column `org_id` + principal tenant `acme` →
     the built `Predicate` matches a row only when both hold. Assert both a
     positive and a negative row via `Predicate::matches`.
   - `all_mode_still_applies_tenant_scope` — `ActiveRuleset::all_mode()`,
     tenant column configured → a row from another tenant does **not** match.
     This is the ADR-0031 security invariant; it gets its own test.
   - `missing_claim_rejects_subscribe` → `Err(MissingClaim)`, and assert the
     socket closes rather than delivering an unscoped stream.
   - `where_sql_cannot_widen_past_rules` — client sends
     `where_sql = "owner_id = 'someone_else'"` against a rules scope of
     `owner_id = claims.sub`; the composed predicate matches nothing.
2. Implement; update both call sites and the existing tests that construct
   `build_predicate` arguments.
3. Add a `ws_contract.rs` case asserting the close reason text for
   `NotSynced` (the SDKs surface it verbatim to app developers).
4. `make ci`.
5. Commit: `feat: enforce sync rules at subscribe time`.

---

## Task 11 — D2: `rules_checksum` on the wire (+ composed-epoch fallback)

**Files**
- Modify: `crates/nostos-infra/src/wire.rs` (`ClientMessage::Subscribe`
  at line 69; `encode_resume_info` at line 369; `decode_resume_info` at
  line 383)
- Modify: `crates/nostos-infra/src/transport.rs` (advertise site ~line 331,
  resume gate ~line 697)
- Modify (expected breakage — every test that hand-constructs an epoch):
  `crates/nostos-infra/tests/resume_and_ack.rs`,
  `crates/nostos-infra/tests/chaos.rs`,
  `crates/nostos-infra/tests/e2e_pg_oplog_replay.rs`. Update them to compose
  through `compose_sync_epoch` rather than asserting a raw slot epoch;
  `git grep -n "slot_epoch" -- crates/nostos-infra/tests` to find the full set
  before starting.
- Test: inline `#[cfg(test)]` in `transport.rs` (extend the existing
  `replay_delivers_on_epoch_match_in_window` / `snapshot_on_epoch_mismatch`
  block near line 1155)

**Decision (D2) — one explicit field, two negotiated paths.** The client
already persists and resends the `u64` epoch the server advertises
(`encode_resume_info` at wire.rs:369 → `ClientMessage::Subscribe { epoch:
Option<u64> }` at wire.rs:69, ADR-0025 slice 4b). D2 adds a sibling field
rather than overloading that integer, so a log reader can tell "slot recreated"
from "rules changed" by looking at the frame.

The server learns whether a client speaks D2 **from the Subscribe frame, which
arrives before `resume_info` is emitted**, and therefore picks the path
per session:

| client | Subscribe carries | server advertises | resume gate compares |
|---|---|---|---|
| new (D2) | `epoch` + `rules_checksum` | raw `slot_epoch` **and** `rules_checksum` | epoch match **AND** checksum match |
| old | `epoch` only | `compose_sync_epoch(slot_epoch, checksum)` | composed value (Task 5, unchanged) |

Missing field = **accept**, never reject: an old client keeps working and still
resyncs on a rules change, because the fallback folds the checksum into the
epoch exactly as before. Log it once per session at `debug`:
`client omitted rules_checksum; using composed-epoch fallback`.

**Interfaces produced**

```rust
// crates/nostos-infra/src/wire.rs — ClientMessage::Subscribe gains:
    /// The rules checksum the client last synced under (ADR-0031, D2).
    /// `u64`, symmetric with `epoch` directly above it — no SDK parses this
    /// frame by hand (see the Task 12 survey), so there is no JS-precision
    /// argument for a string encoding, and an integer keeps
    /// `skip_serializing_if` behaviour identical to `epoch`.
    /// `None` = a pre-D2 client → composed-epoch fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules_checksum: Option<u64>,

/// Advertise the resume epoch, and — for D2 clients — the active rules
/// checksum. `rules_checksum: None` omits the key entirely, so the frame a
/// pre-D2 client sees is byte-identical to today's.
pub fn encode_resume_info(epoch: u64, rules_checksum: Option<u64>) -> Vec<u8>;

/// Decode a `resume_info` frame. Returns the epoch and the checksum when
/// present. Unknown keys are ignored (forward compatibility).
pub fn decode_resume_info(data: &[u8]) -> Option<(u64, Option<u64>)>;
```

**Interfaces consumed:** `nostos_domain::sync_epoch::compose_sync_epoch`
(retained — it is the fallback path, not dead code).

**One-time client effect (expected, not a bug):** a client that upgrades starts
sending `rules_checksum`, so the server advertises the raw slot epoch where it
previously advertised a composed one. The stored value mismatches and the
client takes exactly one full snapshot. Say so in the Task 23 release notes so
it is not diagnosed as a regression.

**Steps**
1. **Compatibility precondition — already verified 2026-08-06, re-assert it.**
   `ClientMessage` is `#[serde(tag = "type", rename_all = "lowercase")]`
   (wire.rs:43) and sets **no** `deny_unknown_fields`, so an *old* server
   silently ignores a new client's `rules_checksum` instead of erroring. That is
   what makes new-client/old-server safe. Re-run
   `git grep -n "deny_unknown_fields" -- crates/nostos-infra/src/wire.rs`
   (must print nothing) before writing code; if that ever changes, D2 needs
   negotiated protocol versioning and this task stops.
2. Failing wire tests in `wire.rs`:
   - `subscribe_without_rules_checksum_decodes` — today's exact JSON still
     parses, with `rules_checksum: None`.
   - `subscribe_with_rules_checksum_decodes` — hex string round-trips.
   - `resume_info_omits_checksum_when_none` — the emitted JSON has no
     `rules_checksum` key at all (assert on the raw bytes, not the parsed
     value); `decode_resume_info` returns `(epoch, None)`.
   - `resume_info_roundtrips_checksum`.
   - `decode_resume_info_ignores_unknown_keys` — a frame with a future
     `"foo":1` key still decodes.
3. Failing gate tests in `transport.rs` (extend the existing
   `replay_delivers_on_epoch_match_in_window` / `snapshot_on_epoch_mismatch`
   block near line 1155):
   - `d2_client_replays_when_epoch_and_checksum_match`.
   - `d2_client_snapshots_when_only_checksum_differs` — the point of D2: same
     slot epoch, different rules → snapshot.
   - `legacy_client_snapshots_on_rules_change` — no `rules_checksum` sent;
     the composed fallback still forces the snapshot.
   - `legacy_client_replays_when_nothing_changed` — proves the fallback did not
     break ADR-0025.
   - `advertised_values_match_gate_values` — for both paths, what
     `encode_resume_info` sent is what the gate compares (per the comment at
     transport.rs:325–330).
4. Implement wire + both transport sites.
5. Update the tests that hand-construct epochs (see the Files list above);
   `git grep -n "encode_resume_info\|slot_epoch" -- crates/nostos-infra/tests`
   for the full set before starting.
6. Add the `ponytail:` comment at the fallback branch: *"ponytail: pre-D2
   clients get the rules checksum folded into the advertised epoch, so their
   logs cannot distinguish a slot recreate from a rules edit. Ceiling: log-level
   attribution only for old clients. Upgrade path: drop the fallback once the
   SDK floor is D2-or-newer."*
7. `make ci`.
8. Commit: `feat: add rules_checksum to the subscribe and resume_info frames`.

---

## Task 12 — D2 client half + the nine SDKs + backward compat

**The survey that sizes this task (re-runnable, and it shrank the work):**

```sh
# Who builds a Subscribe frame? Exactly one place, and it is Rust.
git grep -n "ClientMessage::Subscribe {" -- crates/ sdk/
#   crates/nostos-client/src/client.rs:845   (first subscribe)
#   crates/nostos-client/src/client.rs:862   (re-subscribe)

# Does any SDK hand-roll the frame in its own language? No.
grep -rn '"subscribe"' sdk/*/src --include=*.ts --include=*.dart \
                                --include=*.kt --include=*.swift --include=*.cs
#   (no hits — every hit is inside node_modules/)
```

All nine SDKs — `nostos_flutter`, `nostos_kotlin`, `nostos_swift`, `nostos_dotnet`,
`nostos_node`, `nostos_tauri`, `nostos_react_native`, `nostos_capacitor`,
`nostos_web` — are bindings over `nostos-client` / `nostos-ffi-wasm`
(`sdk/*/src/lib.rs`). **The wire change lands once, in Rust, and the nine
inherit it.** Do not invent nine code changes to match the ruling's phrasing;
the ruling asked for *coverage*, and coverage here is one change plus nine
verifications.

**Files**
- Modify: `crates/nostos-core/src/storage.rs` (`Storage` trait — new defaulted
  methods beside `epoch()` at line 67 / `save_epoch()` at line 74)
- Modify: `crates/nostos-core/src/apply.rs` (passthrough beside `epoch()` at
  line 174 / `save_epoch()` at line 179)
- Modify: `crates/nostos-client/src/storage.rs` (SQLite impl — same meta table
  that holds the epoch)
- Modify: `crates/nostos-client/src/client.rs` (send at 845 + 862; persist at
  the `decode_resume_info` interception ~1009)
- Test: `crates/nostos-client/tests/` (new `rules_checksum_roundtrip.rs`)

**Interfaces produced**

```rust
// crates/nostos-core/src/storage.rs — DEFAULTED, exactly like epoch()/save_epoch().
// A default impl is what keeps this from being a nine-SDK change: the WASM
// in-memory storage and any third-party Storage keep compiling untouched, and
// simply fall back to the composed-epoch path.

/// The rules checksum this client last synced under (ADR-0031 D2).
/// `0` = unknown (fresh DB, or a storage that does not persist it) → the
/// Subscribe omits the field → server uses the composed-epoch fallback.
fn rules_checksum(&self) -> crate::Result<u64> { Ok(0) }

/// Persist the rules checksum advertised in `resume_info`. Non-fatal on
/// failure — mirrors `save_epoch`: a persist failure costs one extra
/// snapshot next reconnect, it must never kill the session.
fn save_rules_checksum(&self, _checksum: u64) -> crate::Result<()> { Ok(()) }
```

**Backward compatibility — the four-quadrant matrix this task must prove**

| | old server | new server |
|---|---|---|
| **old client** | unchanged | composed-epoch fallback (Task 11); resyncs on rules change |
| **new client** | server ignores the unknown `rules_checksum` key (wire.rs:43 is `tag="type"` with no `deny_unknown_fields`); `resume_info` carries no checksum → client stores `0` → next Subscribe omits the field → composed fallback | explicit checksum path |

Every quadrant degrades to *more snapshots*, never to a wrong or missing row.
That is the invariant to assert, and it is the reason `Missing = accept, log`
(operator ruling D2) is safe.

**Steps**
1. Failing test `rules_checksum_roundtrip.rs`: a client that receives a
   `resume_info` carrying a checksum persists it, and its **next** Subscribe
   carries it back. Then: server changes the ruleset → the reconnect takes the
   snapshot path. Reuse the existing epoch-persistence test as the template
   (`git grep -rn "save_epoch" -- crates/nostos-client/tests`).
2. Failing test `absent_checksum_is_accepted`: a `resume_info` with **no**
   `rules_checksum` key leaves the stored value at `0` and the next Subscribe
   omits the field. Assert the `debug` log line fires once, not per frame.
3. Implement: trait defaults → SQLite impl → `client.rs` send + persist.
   Persist the checksum in the **same** `spawn_blocking` hop that already
   persists the epoch — two hops can interleave and strand a half-updated pair.
4. **Nine-SDK verification (the actual per-SDK work).** For each SDK: rebuild,
   regenerate bindings where the toolchain generates them, run its smoke test.
   Record the result in a table in the commit message.
   - Codegen'd, must be regenerated: `nostos_kotlin` + `nostos_swift` (UniFFI),
     `nostos_flutter` (flutter_rust_bridge), `nostos_dotnet`.
   - Plain rebuild: `nostos_node`, `nostos_tauri`, `nostos_web` (wasm-pack).
   - TS wrappers over a native module, no Rust of their own:
     `nostos_react_native`, `nostos_capacitor` — verify only.
   - Expected diff in all nine: **none in public API**. `rules_checksum` never
     crosses the FFI boundary; it is internal to the transport. If a generated
     binding's public surface changes, stop — something leaked that shouldn't.
   - Gotchas that have burned this repo before: `sdk/nostos_swift/swift-sources/`
     is gitignored regen output (never `git add` it); the kotlin sdk-e2e needs
     `nostos_api34` on port **5556**; never run `git add -A` in `sdk/` (deleted
     `fixtures/` is intentionally uncommitted).
5. `make ci`, then `make sdk-e2e` (9/9 — this task is exactly the kind of
   change that only the live PUSH+ECHO run can falsify).
6. Commit: `feat: persist and resend the rules checksum from the client`.

---

## Task 13 — `all`-mode startup warning

**Files**
- Modify: `crates/nostos-server/src/main.rs` (after rules load, before serve)
- Modify: `crates/nostos-infra/src/schema_source.rs` only if a helper is needed
- Test: `crates/nostos-server/tests/all_mode_warning.rs` (new file; unit-test the
  formatting function, which lives in `main.rs` and is `pub(crate)` — expose it
  via a small `mod` or test it inline in `main.rs` under `#[cfg(test)]`;
  prefer **inline in `main.rs`**, since `nostos-server` is a binary crate with no
  library target)

**Interfaces produced**

```rust
// crates/nostos-server/src/main.rs
/// Render the `sync_mode = "all"` guardrail banner. Pure formatting so it is
/// testable without a database. `None` row estimates render as "unknown"
/// (Postgres reports `reltuples = -1` for a never-analyzed table).
fn format_all_mode_warning(stats: &[TableStat]) -> String;
```

Expected output shape:

```
WARNING: sync_mode = "all" — every replicated row reaches every authorised client.
  tasks    ~12,400 rows
  notes    ~340 rows
  audit    unknown rows (never analyzed)
  3 tables, ~12,740 rows estimated.
  This is the zero-config development default. For production, run
  `nostos rules init` and switch sync_mode to "toggles".
```

**Steps**
1. Failing tests:
   - `warning_lists_tables_and_total`.
   - `unknown_estimate_renders_unknown` — a `TableStat { estimated_rows: None }`
     renders the literal word `unknown` and is **excluded** from the total, and
     no `-1` or negative number appears anywhere in the string.
   - `empty_stats_still_warns` — zero tables still emits the WARNING line (an
     unreachable/unanalyzed DB must not silence the guardrail).
2. Implement. Emit via `tracing::warn!` at boot, only when
   `ruleset.mode() == SyncMode::All`. Wire `PgTableStats` only when
   `cfg.replicator == "pg"`; otherwise pass an empty slice (the fake replicator
   has no database to introspect).
3. A stats-fetch failure must **not** abort boot: log
   `could not estimate table sizes: <err>` and print the warning without
   numbers.
4. `make ci`.
5. Commit: `feat: warn at startup when sync_mode is all`.

---

## Task 14 — Hot reload + live-session invalidation

**Files**
- Modify: `crates/nostos-server/src/main.rs` (spawn the watcher task)
- Modify: `crates/nostos-infra/src/transport.rs` (close-with-reason on stale
  ruleset)
- Test: `crates/nostos-infra/tests/rules_reload.rs` (new)

**Why:** the ratified reload semantics are "per-session compiled predicates +
checksum-triggered resync on rules change (**no engine restart**)". Task 11
covers *reconnects*; a client that is connected when the operator flips a toggle
would otherwise keep its old predicate forever.

**Interfaces produced**

```rust
// crates/nostos-server/src/main.rs
/// Poll the rules file and swap the shared `ActiveRuleset` when its checksum
/// changes. Returns only when `shutdown` fires.
async fn watch_rules(
    path: std::path::PathBuf,
    rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
    poll_interval: std::time::Duration,
    shutdown: tokio::sync::watch::Receiver<bool>,
);
```

```rust
// crates/nostos-infra/src/transport.rs
/// The close reason a live session receives when the ruleset changed under it:
/// `"rules changed (checksum <old> -> <new>); reconnect to re-scope"`.
pub(crate) const RULES_CHANGED_CLOSE_REASON: &str = "rules changed; reconnect to re-scope";
```

**Mechanism — and the hot path it must not touch.** The per-connection select
loop in `transport.rs` iterates **once per delivered event**. A
`RwLock::read().await` (or any shared-state poll) inside it would run millions
of times a second at 1k clients and is exactly the kind of change that moves
`benches/results/RESULTS.md`. Do not put the checksum check there.

Instead: the watcher owns a `tokio::sync::watch::Sender<u64>` (created in
Task 9); each connection clones the `Receiver` and adds one
`_ = rules_rx.changed() => { … }` arm to the **existing** select. A `watch`
receiver that has seen the current value never becomes ready, so the arm is
free — no lock, no poll, no per-event work — until the operator actually edits
the file. When it does fire, the connection closes with
`RULES_CHANGED_CLOSE_REASON`. Clients reconnect (existing logic in
`crates/nostos-client`), the composed epoch has changed (Task 11), and they get
a fresh snapshot under the new rules. The `Arc<RwLock<ActiveRuleset>>` is read
only at subscribe time (Task 10) — a cold path, once per subscription.

`ponytail:` comment required at the close site: *"ponytail: rules reload
re-scopes by disconnecting live sessions rather than swapping predicates in
place. Ceiling: one reconnect + snapshot per connected client per rules edit.
Upgrade path: in-place predicate swap + a differential resync frame."*

**Steps**
1. Failing tests in `crates/nostos-infra/tests/rules_reload.rs`:
   - `checksum_change_closes_live_session` — connect, swap the shared ruleset,
     assert the socket closes with the reason.
   - `identical_reload_does_not_close` — write the file again with different
     whitespace but an identical canonical form; the session survives (this is
     the canonicalization payoff from Task 4).
   - `malformed_reload_keeps_previous_ruleset` — write garbage to the file;
     the watcher logs an error, does **not** swap, does **not** widen scope,
     and live sessions stay open.
2. Implement the watcher (5-second poll; a poll loop, not `notify` — no new
   dependency, and a 5s worst-case reload is acceptable for an operator edit).
3. `make ci`.
4. Commit: `feat: hot-reload sync rules and re-scope live sessions`.

---

## Task 15 — CLI: `nostos rules init`

**Files**
- Create: `crates/nostos-cli/src/commands/rules.rs`
- Modify: `crates/nostos-cli/src/commands/mod.rs` (`pub mod rules;`)
- Modify: `crates/nostos-cli/src/main.rs` (`Rules(commands::rules::RulesArgs)`
  subcommand + dispatch arm)
- Test: inline `#[cfg(test)]` in `rules.rs` for the pure generation function;
  `crates/nostos-cli/tests/e2e_pg_rules.rs` for the introspecting path (real-PG,
  self-skipping — follow `crates/nostos-cli/tests/e2e_pg_cli.rs`)

**Interfaces produced**

```rust
// crates/nostos-cli/src/commands/rules.rs
#[derive(Debug, clap::Args)]
pub struct RulesArgs {
    #[command(subcommand)]
    pub command: RulesCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum RulesCommand {
    /// Introspect the database and write nostos_rules.toml (one entry per table).
    Init(InitRulesArgs),
    /// Interactive per-table sync toggles (toggles mode only).
    Edit(EditRulesArgs),
    /// Validate nostos_rules.toml and print the active mode + checksum.
    Check,
}

#[derive(Debug, clap::Args)]
pub struct InitRulesArgs {
    /// Overwrite an existing nostos_rules.toml.
    #[arg(long)] pub force: bool,
    /// sync_mode to write. Default: toggles.
    #[arg(long, default_value = "toggles")] pub mode: String,
    /// Default `sync` value for every discovered table. Default: false
    /// (opt-in beats accidental full-fleet exposure).
    #[arg(long)] pub sync_all: bool,
}

/// Pure: build the initial ruleset from introspected table names.
/// Empty input yields a valid ruleset with zero table entries — never an error.
pub fn rules_from_tables(tables: &[String], mode: SyncMode, sync_default: bool) -> SyncRules;

pub async fn run(args: RulesArgs, cwd: &std::path::Path) -> anyhow::Result<()>;
```

Behaviour:
- Reads `nostos.toml` for the PG URL/publication (`crates/nostos-cli/src/config.rs`,
  `NostosConfig::load`), then lists publication tables with the same query shape
  `PgSchemaSource` uses.
- Refuses to overwrite an existing file without `--force`; the error names the
  file and suggests `nostos rules edit`.
- **Empty DB degrades gracefully:** no tables found → do **not** fail. Write a
  valid `nostos_rules.toml` containing the header, `sync_mode`, and a commented
  template entry, then print: *"No tables found in publication `<name>`. Wrote
  a template `nostos_rules.toml` — re-run `nostos rules init --force` after
  creating tables."* Exit code 0.

**Steps**
1. Failing tests:
   - `rules_from_tables_defaults_to_sync_false`.
   - `rules_from_tables_empty_is_valid` — `validate()` passes on zero tables.
   - `rules_from_tables_respects_sync_all`.
   - e2e: `init_writes_one_entry_per_publication_table` (real-PG, guarded).
2. Implement; register the subcommand in `main.rs`.
3. `make ci`, plus the guarded real-PG run from Task 8's step 3.
4. Commit: `feat: add nostos rules init to generate nostos_rules.toml`.

---

## Task 16 — CLI: `nostos rules edit` (toggle editor)

**Files**
- Modify: `crates/nostos-cli/src/commands/rules.rs`
- Modify: `crates/nostos-cli/src/prompt.rs` (add a bounded selection helper)
- Test: inline `#[cfg(test)]` for the pure state-machine function

**Interfaces produced**

```rust
// crates/nostos-cli/src/prompt.rs
/// Prompt with a default; empty input returns `default`.
pub fn prompt_default(label: &str, default: &str) -> std::io::Result<String>;

// crates/nostos-cli/src/commands/rules.rs
#[derive(Debug, clap::Args)]
pub struct EditRulesArgs {
    /// Switch sync_mode without entering the toggle loop.
    #[arg(long)] pub mode: Option<String>,
}

/// One editor command, parsed from a line of stdin. Pure and unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditCommand {
    Toggle(usize),
    Scope { index: usize, scope: Option<String> },
    Mode(SyncMode),
    Save,
    Quit,
    Help,
    Unknown(String),
}

pub fn parse_edit_command(line: &str) -> EditCommand;

/// Apply one command to the working ruleset. Returns a user-facing message.
/// Rejects `Toggle`/`Scope` when `rules.mode != SyncMode::Toggles` with the
/// message: "sync_mode is `hand` — the generator is frozen. Run
/// `nostos rules edit --mode toggles` to hand truth back to the toggle editor."
pub fn apply_edit(rules: &mut SyncRules, cmd: &EditCommand) -> Result<String, String>;
```

Rendered screen (plain stdout, no raw-mode terminal, no new dependency):

```
nostos_rules.toml — sync_mode = toggles          checksum 0x9f1c…
  [1] [x] tasks    scope: owner_id = claims.sub
  [2] [ ] notes
  [3] [x] projects scope: org_id = claims.org_id
commands: <n> toggle · s <n> <scope> · mode <all|toggles|hand> · w save · q quit · ? help
>
```

**Steps**
1. Failing tests:
   - `parse_toggle_and_scope_and_mode` over the command strings above.
   - `edit_refuses_toggle_in_hand_mode` — `apply_edit` returns `Err` naming
     `hand`.
   - `edit_allows_mode_switch_in_any_mode`.
   - `scope_is_validated_on_entry` — `s 1 owner_id OR x` → `Err` carrying the
     `ScopeError` text; the ruleset is left unmodified.
   - `save_preserves_hand_section` — edit in toggles mode, save, reload:
     `[[rules]]` is byte-preserved.
2. Implement the loop over `apply_edit` (the loop itself is thin I/O; the
   tested surface is the pure functions).
3. `make ci`.
4. Commit: `feat: add nostos rules edit toggle editor`.

---

## Task 17 — CLI: `nostos rules check`

**Files**
- Modify: `crates/nostos-cli/src/commands/rules.rs`
- Test: inline `#[cfg(test)]`

**Interfaces produced**

```rust
/// Render the `nostos rules check` report. Pure so the output is testable.
/// Lines: mode, checksum (hex), synced table count, per-table scope, and every
/// validation error. Returns `Err` text when the ruleset is invalid so the
/// caller can exit non-zero.
pub fn check_report(rules: &SyncRules) -> Result<String, String>;
```

Report shape:

```
nostos_rules.toml — valid
  sync_mode: toggles          (hand section present but inactive)
  checksum:  0x9f1c4b2e77a10d33
  synced:    2 of 3 tables
    tasks     owner_id = claims.sub
    projects  org_id = claims.org_id
  claims referenced: org_id, sub
```

**Steps**
1. Failing tests: `check_reports_mode_and_checksum`,
   `check_flags_invalid_scope_with_table_name`,
   `check_notes_inactive_section` (the "hand section present but inactive"
   parenthetical), `check_lists_referenced_claims_sorted_deduped`.
2. Implement; `run` exits non-zero on `Err`.
3. `make ci`.
4. Commit: `feat: add nostos rules check validation report`.

---

## Task 18 — Mode-switch truth-transfer tests

**Files**
- Create: `crates/nostos-infra/tests/rules_mode_switch.rs`
- Test only — no production code changes. If a test fails, the bug is in
  Tasks 4/6/7/14; fix it there.

**What is proved** (each is one `#[test]`/`#[tokio::test]`):

1. `toggles_to_hand_transfers_truth` — file with `[tables.tasks] sync=true` and
   `[[rules]] table="notes"`; mode `Toggles` → `tasks` allowed, `notes` denied.
   `set_mode(Hand)` → reload → `notes` allowed, `tasks` denied.
2. `hand_to_toggles_deactivates_hand_file` — the reverse; the hand section is
   still present on disk afterwards (`load` returns it) but has no effect.
3. `all_ignores_but_preserves_both_sections` — `set_mode(All)` → every table
   allowed; reload the file and assert both sections are byte-preserved.
4. `all_to_toggles_restores_the_toggle_artifact` — after a round trip through
   `all`, `toggles` mode yields exactly the pre-`all` decisions.
5. `mode_flip_alone_changes_checksum` — same sections, three modes, three
   checksums; assert each flip changes the composed sync epoch (Task 5) and so
   forces a resync.
6. `all_mode_never_bypasses_tenant_scope` — the security invariant, asserted at
   the integration level as well as the unit level (Task 10).

**Steps**
1. Write all six as failing tests first (they will fail to compile until the
   earlier tasks land — that is expected if this task is attempted early;
   otherwise they should pass immediately, which is the point: this task is the
   acceptance gate for the ratified truth-switching semantics).
2. If any passes trivially, strengthen it until it can fail.
3. `make ci`.
4. Commit: `test: prove sync-rules mode-switch truth transfer`.

---

## Task 19 — Server: read-only `GET /rules`

**Files**
- Modify: `crates/nostos-server/src/main.rs` (handler + `.route("/rules", get(rules_handler))`
  next to the existing `/schema` route at line 634)
- Test: inline `#[cfg(test)]` in `main.rs` for the JSON shape

**Interfaces produced**

```rust
// crates/nostos-server/src/main.rs
/// `GET /rules` — what the server is ENFORCING right now (ADR-0031).
/// The read half of the route; Task 20 adds `.put()` on the same path, so
/// this handler is read-only, the *route* is not.
async fn rules_handler(State(state): State<SyncRouterState>) -> Json<serde_json::Value>;
```

Response:

```json
{
  "sync_mode": "toggles",
  "checksum": "0x9f1c4b2e77a10d33",
  "sync_epoch": "0x2b71…",
  "tables": [
    { "table": "tasks", "scope": "owner_id = claims.sub" },
    { "table": "projects", "scope": "org_id = claims.org_id" }
  ]
}
```

Never echo claim **values** — only claim names appear, and only inside scope
text. No principal data, no row counts.

**Deliberate asymmetry: GET open, PUT gated.** Task 20 mounts `PUT /rules` on
this same path behind an operator admin token (Task 21). `GET` stays
unauthenticated in v1 anyway — not because "the route is read-only" (it no
longer is), but because the *disclosure* it makes is strictly smaller than one
nostos already ships openly. Reasoning below; if you disagree with it, gate both
methods, never just re-title this task.

**Auth parity with `/schema`.** `GET /schema` is deliberately unauthenticated
today (`crates/nostos-server/src/main.rs:707`: *"v1 is unauthenticated (schema is
publication-wide metadata, not tenant-scoped rows)"*, with a standing note at
the route at line 632 to add auth in v2). `/rules` discloses strictly less than
`/schema` already does — table names plus scope column text, no columns, no
types — so match `/schema` exactly: unauthenticated in v1, and add `/rules` to
the same v2 note so both get gated together. If `/schema` has been gated by the
time this task runs, gate `/rules` identically in the same commit; do not leave
the two routes on different policies.

**Steps**
1. Failing test `rules_handler_reports_mode_and_tables` (build a
   `SyncRouterState` with a known ruleset; assert the JSON keys and values).
2. Failing test `rules_handler_under_all_mode_lists_no_tables` — `all` mode
   returns `"tables": []` and `"sync_mode": "all"` (the server genuinely has no
   per-table rules to report).
3. Implement.
4. `make ci`.
5. Commit: `feat: expose the active ruleset at GET /rules`.

---

## Task 20 — Server: `PUT /rules` (the config-mutating endpoint)

D5 makes the web panel a real editor, which means the **server** — not the
browser — writes `nostos_rules.toml`. `web/` is static-exported to Cloudflare
Pages with no server process of its own; it can only ever be a client of this
route.

**Files**
- Modify: `crates/nostos-server/src/main.rs` (handler +
  `.route("/rules", get(rules_handler).put(put_rules_handler))` — same path as
  Task 19, one more method)
- Modify: `crates/nostos-infra/src/rules_file.rs` (Task 7) — atomic save
- Test: inline `#[cfg(test)]` in `main.rs`; integration test in
  `crates/nostos-server/tests/`

**Interfaces produced**

```rust
// crates/nostos-server/src/main.rs
/// `PUT /rules` — replace the active ruleset (ADR-0031, D5). Operator-only;
/// see Task 21 for authn/authz. Body is the toggle model, not raw TOML:
/// the server owns serialization so the file shape stays canonical.
async fn put_rules_handler(
    State(state): State<SyncRouterState>,
    headers: HeaderMap,
    Json(body): Json<PutRulesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

#[derive(Deserialize)]
struct PutRulesRequest {
    /// `"all" | "toggles" | "hand"` — but `"hand"` is REJECTED here (422).
    /// Hand-written `[[rules]]` are the CLI's surface; the toggle editor must
    /// never rewrite a file it cannot faithfully round-trip.
    sync_mode: String,
    tables: Vec<PutRulesTable>,
}

#[derive(Deserialize)]
struct PutRulesTable { table: String, sync: bool, scope: Option<String> }
```

Success `200` returns the same body shape as `GET /rules` (Task 19), so the
client can re-render from the response without a second fetch. Failure returns
`422` with `{"error": "<the same text `nostos rules check` prints>"}` — one
validation implementation, two surfaces (Task 17).

**Ordering — write, then swap, and why that order.**

1. Validate the request into a `Ruleset` using the Task 6 compile path. Invalid
   → `422`, nothing touched.
2. Serialize + **atomically** write `nostos_rules.toml` (temp file in the *same
   directory*, `fsync`, `rename` — a cross-filesystem `rename` fails and a
   temp file in `/tmp` is a different filesystem).
3. Only after the write succeeds, swap the in-process `ActiveRuleset` and run
   the Task 14 live-session invalidation.
4. Return `200` with the new checksum.

If the process died between 2 and 3 the file is the truth and startup reloads
it — a crash costs a reload, never a divergence between the file and what is
enforced. The reverse order can enforce a ruleset that was never persisted.

**Interaction with the Task 14 watcher (must be handled, not hoped about).**
The atomic write fires the file watcher, which would reload and re-invalidate
every live session a second time. Task 14's reload path therefore dedupes on
checksum: a reload whose computed checksum equals the currently-active one is
a **no-op** — no swap, no invalidation, one `debug` line. Assert this with
`put_rules_does_not_double_invalidate`.

**Concurrency: last-writer-wins, deliberately.** No `If-Match`, no locking (a
ratified non-goal). Two operators editing simultaneously is out of scope for
v1; the audit log (Task 21) is what makes the outcome explainable after the
fact. Carry the `ponytail:` comment on the handler: *"ponytail: no optimistic
concurrency between the CLI editor and PUT /rules — last write wins. Ceiling:
a concurrent edit can be silently overwritten. Upgrade path: ETag from the
checksum + `If-Match`."* This is the second and last pre-authorised shortcut
(Global Constraint 8).

**Steps**
1. Failing test `put_rules_writes_file_and_swaps_active` — hit the route,
   assert the file on disk **and** `state.active_ruleset()` both changed.
2. Failing test `put_rules_rejects_hand_mode` (422, file unchanged).
3. Failing test `put_rules_rejects_invalid_scope` — reuse a bad-scope fixture
   from Task 17 and assert the error text matches the CLI's byte for byte.
4. Failing test `put_rules_does_not_double_invalidate` (see above).
5. Failing test `put_rules_write_failure_leaves_active_ruleset_untouched` —
   point `NOSTOS_RULES_FILE` at an unwritable path; expect `500` and an
   unchanged in-process ruleset.
6. Implement.
7. `make ci`.
8. Commit: `feat: add PUT /rules to mutate the active ruleset over HTTP`.

---

## Task 21 — Security for the mutation surface (blocking, not optional)

Task 20 adds the first route in nostos's history that **writes operator config**.
Everything before it was read-only or session-scoped. This task exists solely
to keep that surface honest; it ships in the same release or Task 20 does not
ship.

> **Shape ratified by the operator 2026-08-06 — do not re-litigate.**
> `NOSTOS_ADMIN_TOKEN` env var, route **404s when unset** (disabled by default),
> **no `nostos-cloud` control-plane binding**. This was raised as an open
> question and explicitly closed; if you think it should live in `nostos-cloud`,
> that is a new ADR, not a Task 21 edit.

**Files**
- Modify: `crates/nostos-server/src/main.rs` (extractor + audit line)
- Create: `crates/nostos-server/src/admin_auth.rs`
- Test: `crates/nostos-server/tests/admin_auth.rs`
- Modify: `docs/OPERATING.md` (how to set and rotate the token)

**1. Authn/authz — operator principal only, and disabled by default.**

```rust
// crates/nostos-server/src/admin_auth.rs
/// Bearer-token gate for config-mutating routes (ADR-0031, D5).
///
/// DELIBERATELY NOT the sync auth path: `NOSTOS_SYNC_AUTH=supabase-jwt`
/// authenticates *application users*, and no application user may ever
/// rewrite the server's rules. A Supabase JWT — however valid, whatever
/// claims it carries — is rejected here.
pub struct AdminAuth;

/// `None` when `NOSTOS_ADMIN_TOKEN` is unset → the route is **not mounted**.
/// Fail-closed: a default deployment has no mutable surface at all, so an
/// operator who never opts in cannot be attacked through it.
pub fn admin_token_from_env() -> Option<SecretString>;
```

- Compare with a **constant-time** comparison (`subtle::ConstantTimeEq` or an
  equivalent already in the tree — check `Cargo.lock` before adding a dep).
- Minimum length enforced at startup (32 chars); a short token fails boot
  loudly rather than serving a guessable admin route.
- Never log the token, never echo it in an error, never include it in the
  `GET /rules` response.

**2. CSRF stance — stated, not assumed.** The route authenticates with an
`Authorization: Bearer` header, never a cookie. No ambient credential exists,
so a cross-site form or image cannot forge an authenticated request; CSRF
tokens are unnecessary *because of that choice*, not by oversight. Two
defences make the property enforceable rather than incidental:

- Reject any `PUT /rules` whose `Content-Type` is not `application/json`
  (blocks the simple-form vector outright).
- CORS stays default-deny; do not add a permissive `Access-Control-Allow-Origin`
  to make the web panel work. The panel is operator-run and points at their own
  server; if a cross-origin allowance is genuinely needed it is an explicit,
  reviewed, operator-configured origin — never `*`.

Write this paragraph into the ADR too. A future contributor who does not find
the reasoning will "fix" the missing CSRF token by adding cookies.

**3. Audit log — one line per mutation, on the success path only.**

```
rules_mutation actor=<token-id> source=<web|api> mode_before=toggles \
  mode_after=toggles checksum_before=0x… checksum_after=0x… tables_changed=3
```

- `actor` is a non-secret identifier — the first 8 chars of the token's SHA-256,
  never the token. Enough to distinguish two operators, useless if leaked.
- `tracing::info!` at target `nostos::audit` so it can be routed independently.
- Never log claim values or row data (Global Constraint on `/rules` output
  applies here identically).

**Steps**
1. Failing test `put_rules_without_token_is_404` (route unmounted when
   `NOSTOS_ADMIN_TOKEN` is unset — 404, not 401: an absent surface should not
   advertise itself).
2. Failing test `put_rules_with_wrong_token_is_401`.
3. Failing test `supabase_jwt_is_not_admin` — a valid sync JWT is rejected.
   This is the test that stops the two auth systems from being conflated.
4. Failing test `short_admin_token_fails_startup`.
5. Failing test `put_rules_rejects_form_content_type`.
6. Failing test `audit_line_emitted_once_per_mutation` — capture the
   `nostos::audit` target; assert one line, and assert the token does **not**
   appear anywhere in the captured output.
7. Implement, document in `docs/OPERATING.md` (set, rotate, and what to do if
   leaked: rotate + read the audit log).
8. `make ci`.
9. Commit: `feat: gate the rules mutation route behind an operator admin token`.

---

## Task 22 — Web admin: rules toggle editor

**Files**
- Create: `web/src/routes/admin/rules/+page.svelte`
- Modify: `web/src/routes/admin/+page.svelte` (link to the new panel)
- Test: `cd web && npm run check` (svelte-check) is the gate; there is no
  browser test harness in this repo and this task does not add one.

**Scope (D5) — a real editor, client-side only.** `web/` is static-exported
(`@sveltejs/adapter-static`) to Cloudflare Pages: no server process, no
filesystem, no secrets at build time. It reads `GET /rules` (Task 19) and
writes through `PUT /rules` (Task 20) on a user-supplied server URL. The CLI
(Tasks 15–17) remains a fully independent authoring surface; neither is
privileged over the other, and the file is the shared truth.

**Interfaces consumed:** `GET /rules` (Task 19), `PUT /rules` (Task 20).

**Behaviour**
- Server URL input, persisted to `localStorage` under key `nostos.adminServerUrl`
  (match whatever `web/src/routes/admin/+page.svelte` already does; if it
  already stores a server URL, reuse that key rather than adding a second).
- **Admin token input, kept in memory for the session only — never
  `localStorage`, never `sessionStorage`, never a cookie.** A static site on a
  shared origin is exactly where a persisted admin credential becomes an XSS
  payload's prize. The operator re-enters it per session; that friction is the
  feature.
- A toggle per table plus a scope text field, rendered from the `GET` response.
  Save issues one `PUT` with the whole model (the endpoint is a replace, not a
  patch) and re-renders from the `200` body.
- `sync_mode: "hand"` renders the panel **read-only** with an explanatory
  banner: the server rejects hand-mode writes (Task 20) and the UI must not
  offer an action that is guaranteed to 422.
- `all` mode keeps the Task 13 warning banner, verbatim wording.
- A `422` renders the server's error text inline, unmodified — the operator
  should see exactly what `nostos rules check` would have told them.
- No polling, no optimistic UI: the panel shows what the server last confirmed.
  Because concurrency is last-writer-wins (Task 20), a stale panel that saves
  will silently overwrite a CLI edit — say so in a one-line hint next to Save.

**Steps**
1. Read `web/src/routes/admin/+page.svelte` and match its component/style
   conventions (`web/src/lib/components/ui`).
2. Implement the page.
3. Manually verify against a local server with `NOSTOS_ADMIN_TOKEN` set: load,
   toggle, save, reload the page, confirm the change persisted **and** that
   `nostos rules check` on the server's file agrees.
4. `cd web && npm run check` — must pass clean.
5. `make ci` (Rust unaffected, gate is still unconditional).
6. Commit: `feat: add sync-rules toggle editor to the admin console`.

---

## Task 23 — Documentation

**Files**
- Modify: `docs/OPERATING.md` (a "Sync rules" section: the three modes, the
  `all`-mode warning, how to switch truth, what triggers a resync, **and the
  two authoring surfaces** — CLI and web panel — with last-writer-wins stated
  plainly. Task 21 adds the `NOSTOS_ADMIN_TOKEN` set/rotate/leak procedure to
  the same file; coordinate so the two edits do not collide.)
- Modify: `docs/QUICKSTART.md` (mention that the zero-config default is `all`
  and how to move to `toggles` in two commands)
- Modify: `README.md` (one line in the feature list + a link to ADR-0031)
- Modify: `docs/adr/0031-sync-rules-modes-and-checksum-resync.md` (flip Status
  to `Accepted — implemented <date>`; record the two `ponytail:` ceilings
  actually shipped; **carry the D2 backward-compat matrix and the Task 21 CSRF
  reasoning into the ADR** — a contributor who cannot find why there is no CSRF
  token will "fix" it by adding cookies)
- Modify: `CLAUDE.md` (add `nostos rules init|edit|check` to the Verbs section)

**Release-notes item (D2, must not be omitted):** every existing client takes
**one** full snapshot on upgrade — the advertised epoch changes shape the first
time a client sends `rules_checksum` (Task 11). Expected, one-time, not a
regression. Say it in the release notes or it will be diagnosed as a bug.

**Also document (new in D5):** `PUT /rules`, that it is **disabled unless
`NOSTOS_ADMIN_TOKEN` is set**, that a Supabase sync JWT is *not* admin
credentials, and that the web toggle editor holds the token in memory for the
session only — an operator who "helpfully" persists it has created an XSS
target.

**Constraint:** do not touch any performance claim, throughput number, or
competitor-comparison multiple in any of these files (Global Constraint 7).

**Steps**
1. Write the docs.
2. Verify every command shown actually runs as written (`nostos rules check` on
   a real file; `GET /rules` against a locally running server).
3. `make ci`.
4. Commit: `docs: document the three-mode sync-rules suite`.

---

## Acceptance checklist

The suite is done when all of the following hold:

- [ ] `make ci` green (fmt-check + clippy `-D warnings` + full suite).
- [ ] Real-PG e2e green: `docker compose -f docker/docker-compose.yml up -d`
      then `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn cargo test -p nostos-infra --features pg`
      (confirm the tests **ran**, not self-skipped).
- [ ] `cd web && npm run check` clean.
- [ ] A server with no `nostos_rules.toml` boots in `all` mode and prints the
      warning.
- [ ] `nostos rules init` → `nostos rules edit` → restart-free reload → a
      connected client re-snapshots under the new scope.
- [ ] All six Task 18 mode-switch tests pass.
- [ ] `make sdk-e2e` 9/9 after the D2 wire change (Task 12) — the live
      PUSH+ECHO run, not just `make ci`.
- [ ] All four backward-compat quadrants in the Task 12 matrix are covered by a
      test, and every failure mode in them is "an extra snapshot", never a
      dropped or wrong row.
- [ ] `git grep -n "deny_unknown_fields" -- crates/nostos-infra/src/wire.rs`
      returns nothing (the precondition D2 rests on).
- [ ] A `PUT /rules` with no `NOSTOS_ADMIN_TOKEN` set returns 404, and a valid
      Supabase sync JWT is rejected by it (Task 21).
- [ ] One `nostos::audit` line per rules mutation, and the admin token appears
      in no log, no error body, and no `GET /rules` response.
- [ ] Round-trip proof that the two authoring surfaces share one truth: edit in
      the web panel (Task 22) → `nostos rules check` on the server's file agrees
      → edit with `nostos rules edit` → the panel shows it after a reload.
- [ ] No file in `benches/`, no throughput number anywhere, was modified.
- [ ] `git grep -n "TODO\|TBD\|FIXME" -- crates/nostos-domain/src/rules.rs crates/nostos-domain/src/scope.rs crates/nostos-application/src/rules.rs crates/nostos-infra/src/rules_file.rs crates/nostos-cli/src/commands/rules.rs`
      returns nothing.

---

## Open decisions — CLOSED

All five were ratified by the operator on **2026-08-06**. The rulings are
recorded in *Operator rulings* at the top of this plan and are already folded
into the tasks; this section is kept only so the historical questions are not
lost.

| # | Question | Ruling | Landed in |
|---|---|---|---|
| D1 | How much of the JWT reaches `Principal.extra`? | **Full flat scalar claims**, with a blocking security review in the same commit | Task 2 |
| D2 | Wire change, or ride the existing epoch? | **Explicit `rules_checksum` field**, composed epoch kept as the fallback | Tasks 11, 12 |
| D3 | How does a reload re-scope live sessions? | **Whole-socket disconnect-and-resync, as shipped**; any decision change (narrow or widen) closes the socket, table-granular; in-place predicate swap is the documented upgrade path, not what was built | Task 14 |
| D4 | Default for a fresh toggles file | **`sync = false`**, `--sync-all` flips it | Task 15 |
| D5 | Web authoring surface in v1? | **Yes** — authenticated `PUT /rules` + a real toggle editor; the CLI editor remains | Tasks 20, 21, 22 |

**No open decisions remain.** Execution starts at Task 1.
