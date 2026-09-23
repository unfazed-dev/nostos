# Research: industry best practice for sync-scoping config UX

**Date:** 2026-08-06
**Requested by:** team-lead
**Scope:** how operators should define server-side partial-sync rules (which rows each
authenticated client may sync) — informing Nostos's introspection-driven rules-file design.
**Method:** primary docs only, fetched and cited below. Anything not directly verified from a
cited source is marked **[unverified]**.

---

## 1. Survey findings

### 1.2 Hasura — table tracking + row-permission builder

Two distinct steps, both introspection-driven:

1. **Table tracking**: Hasura reads your Postgres schema and lists all tables/views as
   "Untracked" in the Console. You click **Track** (or **Track All**) per table, or express the
   same via a `metadata/tables.yaml` file (`hasura metadata apply`) or the `pg_track_table`
   metadata API. Tracking = "expose this table over GraphQL"; it's the closest precedent to
   Nostos's "introspect DB → present toggles" idea, but the toggle here is per-table
   include/exclude, not a row filter.
   [Hasura table tracking docs](https://hasura.io/docs/2.0/schema/postgres/using-existing-database/)

2. **Row-level permissions**: separately, per (table, role, operation) you define a **boolean
   expression** ("row permissions") plus a **column selection** ("column permissions") in the
   Console's permission editor or via the metadata API. The boolean expression is a structured
   JSON tree over Postgres session variables (`X-Hasura-Role`, custom claims like
   `X-Hasura-User-Id`), not raw SQL — it compiles into a `WHERE` fragment Hasura appends to the
   generated SQL query. Permissions are stored as Hasura **metadata** (YAML/JSON files under
   `metadata/`), separable from the schema itself and hot-applied with `hasura metadata apply`.
   [Hasura permissions docs](https://hasura.io/docs/2.0/auth/authorization/permissions/)

So Hasura's answer to "what does the UI generate on disk" is: metadata YAML files, one concern
per file (`tables.yaml` for tracking, permission objects nested under each tracked table), applied
declaratively — not a single monolithic rules file.

### 1.3 Supabase — RLS editor + Realtime publication toggles

**RLS**: policies are hand-written SQL (`CREATE POLICY ... USING (auth.uid() = user_id)`),
editable via SQL Editor or a dashboard **Policy editor** UI. Supabase's RLS is Postgres-native —
there is no separate rules DSL; the "rule language" *is* SQL boolean expressions evaluated per-row
by Postgres itself. [Supabase RLS docs](https://supabase.com/docs/guides/database/postgres/row-level-security)

**Realtime scoping** is a *separate, coarser* mechanism: which tables broadcast change events at
all is controlled by **toggles in the dashboard's Publications page** ("under `supabase_realtime`,
toggle on the tables you want"), which under the hood runs
`ALTER PUBLICATION supabase_realtime ADD TABLE <table>`. This is exactly the
introspect-tables-and-toggle pattern the operator wants — but note it's a **per-table on/off**,
not a row filter; row-level scoping for Realtime is still delegated to RLS policies evaluated per
subscriber. [Supabase Realtime / Postgres Changes docs](https://supabase.com/docs/guides/realtime/postgres-changes)

Supabase does also ship an **AI policy assistant** in the dashboard for RLS (natural-language →
generated policy SQL) — referenced in Supabase marketing/docs navigation but I did not fetch a
primary doc page confirming its exact generation format; treat as **[unverified]** for this report
(the operator can confirm from the live dashboard).

### 1.4 ElectricSQL — Shapes

A **Shape** = `table` + optional `where` clause (SQL boolean expression, full Postgres operator
set — `=`, `IN`, `LIKE`, array containment, etc.) + optional `columns` clause (column masking) +
optional `queryable columns` restriction. Shapes are **single-table** (JOINs not supported; you
can use subqueries to filter by related data, but the shape only ever contains root-table rows)
and **immutable** once defined — you don't mutate a shape's filter in place, you request a
different shape. [ElectricSQL Shapes guide](https://electric-sql.com/docs/guides/shapes)

Auth is explicitly **not** baked into the shape definition or a rule DSL. Electric's stated
position: "Rules are optional... there's no need to use database rules... when your sync engine
runs over standard HTTP." Instead two patterns: **proxy auth** (your own API/edge-function sits in
front of `GET /v1/shape` and injects/validates the `where` params before forwarding) and
**gatekeeper auth** (your API mints a shape-scoped token; a proxy validates the token against the
requested shape params). This is the "config file" alternative — Electric deliberately pushes
scoping logic into application code rather than a declarative rules file.
[ElectricSQL Auth guide](https://electric-sql.com/docs/guides/auth)

### 1.5 Rocicorp Zero — synced queries

Zero replaced Postgres-RLS-flavored permissions (now under `/docs/deprecated/rls-permissions`)
with **synced queries**: a named query function, written in TypeScript/ZQL, that exists in *two*
copies — one runs on the client for instant optimistic reads, one runs on the server and is the
**enforcement point**. The docs are explicit: "the server can add extra filters to enforce
permissions that the client query does not." Query definitions live in application code
(`defineQuery(paramsSchema, (ctx) => zql...)`), not a separate declarative rules file — this is a
**code-as-rules** approach, one level more expressive than Hasura's boolean-JSON but requiring a
programming language rather than a config format.
[Zero Queries docs](https://zero.rocicorp.dev/docs/reading-data),
[Zero Auth docs](https://zero.rocicorp.dev/docs/permissions)

### 1.6 Ditto / Triplit / InstantDB / Firestore — rule-language ergonomics

- **Ditto**: subscriptions are DQL queries with a `WHERE` clause and bound parameters
  (`SELECT * FROM cars WHERE color = :color`), registered client-side, closest to Electric's shape
  model but query-language (DQL, SQL-like) rather than filter-tree.
  [Ditto stateful subscriptions](https://docs.ditto.live/stateful-subscriptions),
  [Managing sync subscriptions](https://docs.ditto.live/sdk/v4-7/sync/subscriptions-management)
- **Triplit**: docs page for permissions returned a connection error during this research —
  **[unverified]**, not covered.
- **InstantDB**: rule language is CEL-like boolean expressions per (namespace, action) —
  `view`/`create`/`update`/`delete` — with a `bind` block for named sub-expressions
  (`"isOwner": "auth.id != null && auth.id == data.creatorId"`), referencing `auth`, `data`
  (existing row), `newData` (proposed row). Defined in a version-controlled `instant.perms.ts` file
  and pushed via CLI (`instant-cli push perms`), **or** edited live as JSON in the dashboard's
  permissions editor — both write to the same underlying rule store. Unset rules default to
  `true` (permissive-by-default, flagged in their own docs as something to tighten before
  shipping). [InstantDB Permissions docs](https://www.instantdb.com/docs/permissions)
- **Firebase Firestore**: `match`/`allow` blocks in a dedicated rules file
  (`firestore.rules`), a bespoke rules language (not SQL, not CEL, though CEL-influenced) that
  supports path wildcards and `get()`/`exists()` for cross-document/cross-collection checks. Most
  mature "hand-written rules DSL with its own grammar and simulator" precedent in the survey.
  [Firestore Security Rules — Get started](https://firebase.google.com/docs/firestore/security/get-started)

### 1.7 Hot-reload precedent outside sync engines

**PgBouncer** `SIGHUP`/`RELOAD`: re-reads config without dropping active connections; a changed
**destination** for a database definition closes the *server*-side connection only when it's next
released back to the pool (respecting pooling mode), new connections get new params immediately;
TLS *setting* changes force reconnect automatically, TLS *file content* changes apply only to new
connections unless you explicitly issue `RECONNECT`. `WAIT_CLOSE` lets an operator block until a
reload has fully propagated. [PgBouncer config docs](https://www.pgbouncer.org/config.html),
[PgBouncer usage docs](https://www.pgbouncer.org/usage.html)

**Hasura** `hasura metadata apply` is a declarative reconcile — safe to rerun, hot-applies without
restarting the engine.

**Bucket-based sync engines'** deploy semantics = server-side checksum invalidation of only the
affected bucket — the closest analog to "a predicate tightened live" and the most relevant
precedent for Nostos, since Nostos's predicates are also a live, per-session filter tree rather
than a connection-pool config.

---

## 2. Answers

**A. Who does introspection-driven rule generation best, and what does the UI generate on disk?**
Nobody in this survey does exactly what the operator wants (introspect → toggle → generate a
committed rules file) end-to-end. The two closest pieces, combined, are the precedent:
- **Supabase Realtime Publications page** — pure per-table toggle over an introspected table list,
  generates `ALTER PUBLICATION ... ADD TABLE` (one boolean per table, no row filter).
- **Hasura table tracking + permission editor** — introspected table list with per-table
  track/untrack toggles, *plus* a structured (not raw-SQL) boolean-expression builder for row
  permissions, both serialized to versionable **metadata YAML/JSON files** applied declaratively
  (`hasura metadata apply`). Hasura is the best precedent for "toggle in a UI → structured file on
  disk, not raw SQL, not opaque dashboard-only state."
Granularity ceiling nobody fully automates via UI: column masking and cross-table JOINs are
either hand-written (Hasura column permissions, Electric `columns` clause) or added only in a
later product revision, still as hand-written SQL-like text.

**B. What rule-language shape won, and what it enables.**
Three shapes recur, each buying a different editor:
1. **SQL-like text with bound parameters from claims** (Ditto DQL, Electric `where`) — maximal
   expressiveness, but the editor affordance is a syntax-checked
   text box, not toggles; a generated UI can pre-fill it but can't safely reverse-engineer
   arbitrary SQL back into toggles.
2. **Structured boolean-expression tree over claim/session variables** (Hasura row permissions,
   InstantDB CEL-like rules) — this is what a toggle-based UI can *actually* round-trip: build the
   tree from UI clicks, serialize it, and re-render it back into the same UI later. This is the
   only shape that supports true bidirectional dashboard editing without a SQL parser.
3. **Full imperative code as the rule** (Zero synced queries) — most powerful (arbitrary logic,
   reuse of app-side query functions) but not UI-editable at all; it's a developer-only surface.
No survey system uses a raw opaque compiled-binary or protobuf rule format — every winner is
**human-readable text on disk** (YAML, TS, or a bespoke `.rules` grammar), because all of them
need diffability and version control for review before deploy.

**C. Empty-database story.**
No primary doc in this survey describes a "wait for schema to appear, offer templates" flow as a
first-class product feature — I could not verify this pattern anywhere. Adjacent precedent:
Supabase dashboard **quickstart SQL snippets** populate a template schema (e.g. the `todos` table
in the Realtime quick-start) so the introspection step has something to show; Hasura's Console
just shows an empty "Untracked Tables/Views" list with a `Track All` no-op until tables exist.
**Recommendation basis:** neither audited product blocks on an empty DB — they degrade gracefully
to "nothing to track yet," and ship optional starter-schema templates as separate onboarding
content, not gated logic in the rules tool itself. Nostos should do the same: `nostos rules init` on
an empty DB should print "no tables found — apply your schema first, or run `nostos rules init
--template <name>`" rather than block/poll.

**D. Rule-change deploy semantics for live clients.**
Industry norm, confirmed via primary docs for one bucket-based sync system (the only system in
this survey with primary-doc detail on this): **tighten a rule → the affected bucket's server-side checksum changes → already-connected
clients detect the checksum mismatch on their next checkpoint and re-download that bucket's full
current contents** (not a diff, not a silent local-only filter change) — scoped to the bucket(s)
whose definition changed, not a global wipe. This is exactly "versioned rules + checksum-triggered
resync," not "drop everything" and not "lazy/eventually apply." PgBouncer's SIGHUP is *not* a good
analog here — it's connection-pool config, not per-row authorization, so its "old connections keep
old params until released" semantics would be a correctness bug if applied to sync predicates (a
client could keep reading rows a tightened rule should have revoked). Nostos's predicate re-eval
already happens live per WAL event (ADR-0012), so the correct precedent to copy is: **on rules
reload, re-validate every live session's compiled predicate against the new config and, for any
session whose effective predicate changed, force a resnapshot of that session** (not the whole
server) — this mirrors the per-bucket invalidation pattern described above, adapted to Nostos's
per-session model.

**E. Recommendation for Nostos v1.**

Ground truth from the codebase: `PredicateExpr` (`crates/nostos-domain/src/predicate.rs`) is
already `Any | Eq | Ne | Lt | Gt | Le | Ge` leaves over `PredicateFilter{column, value}` combined
with `And | Or | Not`, single-table, and ADR-0011 already injects a server-side tenant equality
(`NOSTOS_TENANT_COLUMN = principal.tenant_id`) ahead of any client-supplied filter. The rules file
this recommendation describes is a **generator for exactly this tree**, not a new engine.

- **CLI**: `nostos rules init` — introspects the connected Postgres DB (already required for
  replication), lists tables + columns, and for each table proposes a default entry (see below).
  `nostos rules init --template <name>` seeds from a bundled template set (multi-tenant SaaS, single
  -owner-per-row, public-read) when the DB is empty or the operator wants a starting point instead
  of raw introspection. `nostos rules validate` type-checks a rules file against the live schema
  (catches renamed/dropped columns) without deploying. `nostos rules apply` writes the file and
  triggers the SIGHUP-equivalent reload the config system already plans to support.

- **File format** (`nostos.rules.toml`, ≤30 lines, matches the tenant-injection model already
  ratified in ADR-0011 rather than inventing a new one):

```toml
# generated by `nostos rules init` — hand-edit, then `nostos rules apply`
[tables.tasks]
sync = true                     # per-table toggle (Supabase-publication precedent)
scope = "owner_id = claims.sub" # structured, not raw SQL — see grammar below

[tables.tasks.columns]
# column masking deferred (see below) — omitted = all columns sync

[tables.projects]
sync = true
scope = "org_id = claims.org_id"

[tables.public_catalog]
sync = true
scope = "true"                  # explicit match-all, not a bare omission
```

  The `scope` string is **not raw SQL** — it's a restricted grammar (`column <op> claims.<field>`,
  ANDable, no OR/NOT at v1) that compiles 1:1 onto the existing `PredicateExpr` leaves/And, which a
  toggle UI can fully round-trip (pick column → pick op → pick claim), mirroring Hasura's
  structured-tree precedent (§2.A) rather than Electric's free-text SQL. This is the
  "no SELECT ceremony" requirement: no Parameter Query / Data Query split,
  no bucket naming — one line per table.

- **Toggle UI contract** (future dashboard, not v1 scope): reads/writes this TOML 1:1 — `sync`
  boolean is the table checkbox (Supabase Realtime precedent), `scope` is built from a
  column-dropdown + operator-dropdown + claim-dropdown row (Hasura precedent), never free-text SQL
  in the UI even though the file format is human-editable text.

- **Explicitly deferred for v1** (name each, per the ask):
  - **JOINs / cross-table scope** — Electric punts on this too; single-
    table matches Nostos's existing `PredicateExpr` exactly.
  - **Column masking** — Hasura and Electric both support it but it's a second axis of complexity;
    the `[tables.X.columns]` block above is a placeholder key reserved in the format, unimplemented.
  - **OR/NOT in `scope`** — the existing engine supports it (`PredicateExpr::Or/Not`), but a v1
    toggle UI can't safely round-trip boolean-tree branching from simple form controls; ship AND-
    only, expand once there's a real UI to drive it.
  - **The dashboard itself** — out of scope per the ask; the file format is designed so it *can*
    be built later without a rules-file rewrite.
  - **Empty-DB blocking** — per §2.C, don't block; print guidance and offer `--template`.

- **Deploy semantics**: on `nostos rules apply` (SIGHUP), re-render every live session's compiled
  `PredicateExpr` from the new file; if a session's resolved tree changed, force that session
  through the existing resnapshot path (already needed for reconnect/backfill per
  `docs/adr/0025-persisted-oplog-backfill-for-reconnect-resume.md`) rather than a global restart —
  this is the Nostos-shaped version of the checksum-invalidation pattern described in §2.D, and
  it's free: Nostos has no bucket-log/compaction machinery to rebuild, so the "cost" of
  a rule change is bounded to "the sessions actually affected" — cheaper than a bucket-rebuild-
  based design, not more expensive. This should be called out as a competitive point, not
  just an implementation detail.

---

## Sources

- [Hasura: Configuring Permission Rules](https://hasura.io/docs/2.0/auth/authorization/permissions/)
- [Hasura: Set Up a GraphQL Schema Using an Existing Postgres Database](https://hasura.io/docs/2.0/schema/postgres/using-existing-database/)
- [Supabase: Row Level Security](https://supabase.com/docs/guides/database/postgres/row-level-security)
- [Supabase: Postgres Changes (Realtime publications)](https://supabase.com/docs/guides/realtime/postgres-changes)
- [ElectricSQL: Shapes guide](https://electric-sql.com/docs/guides/shapes)
- [ElectricSQL: Auth guide](https://electric-sql.com/docs/guides/auth)
- [Rocicorp Zero: Queries](https://zero.rocicorp.dev/docs/reading-data)
- [Rocicorp Zero: Authentication / permission patterns](https://zero.rocicorp.dev/docs/permissions)
- [Ditto: Stateful Subscription Performance Guidelines](https://docs.ditto.live/stateful-subscriptions)
- [Ditto: Managing Sync Subscriptions](https://docs.ditto.live/sdk/v4-7/sync/subscriptions-management)
- [InstantDB: Permissions](https://www.instantdb.com/docs/permissions)
- [Firebase: Get started with Cloud Firestore Security Rules](https://firebase.google.com/docs/firestore/security/get-started)
- [PgBouncer: config docs](https://www.pgbouncer.org/config.html)
- [PgBouncer: usage docs](https://www.pgbouncer.org/usage.html)
- Triplit permissions docs: fetch failed (`ECONNRESET`) — not covered, marked unverified.
- Nostos code grounding: `crates/nostos-domain/src/predicate.rs` (`PredicateExpr`),
  `docs/adr/0011-server-enforced-predicates.md`, `docs/adr/0012-dynamic-predicate-expression-engine.md`,
  `docs/adr/0025-persisted-oplog-backfill-for-reconnect-resume.md`.

## Operator decisions (ratified 2026-08-06)

1. **A + B confirmed** as posed in-session (grammar subset generator + toggle-tree over introspected schema per the recommendation above).
2. **Mode model is now THREE mutually exclusive sync modes** (`sync_mode` in nostos_rules.toml):
   - `all` — NEW: auto sync mode; everything in the database is synced. No rules evaluated; equivalent to an implicit `select *` on every replicated table. Guardrail: emit a startup warning with introspected table/row-count estimate; intended as the zero-config dev default, prod use is a deliberate opt-in.
   - `toggles` — first-class: auto-detect (schema introspection) + toggle UI generates the rules; UI is the truth.
   - `hand` — optional flag: hand-authored rules file is the truth; auto-detect/toggle generation disconnected while active.
3. **Truth-switching semantics** (per operator ruling): switching modes moves the single source of truth. Entering `hand` freezes/disconnects the generator; switching back to `toggles` deactivates the hand-authored file and the UI becomes the new truth (hand switch off). `all` ignores both rule sources entirely but must not delete them — mode switch away from `all` restores whichever rules artifact that mode owns.
4. Versioned rules + checksum-triggered resync applies to all three modes (mode + rules hash both participate in the checksum, so a mode flip alone triggers resync).
