# ADR-0011: Server-enforced predicates (never client-attested)

- **Status:** Accepted (shipped — Tier 1 multi-tenant isolation)
- **Date:** 2026-06-27

## Context

Even with `/sync` authenticated (ADR-0010), the client still chose its own
subscription `Predicate`. A client authenticated as tenant `acme` could send
`Subscribe { filters: [org_id = other] }` and read `other`'s rows — a cross-tenant
read. Self-attested predicates are the multi-tenant isolation hole; auth alone
closes "can you connect," not "can you read someone else's data."

## Decision

The server **never trusts the client's tenant filter** — it **injects** it:

1. When `NOSTOS_TENANT_COLUMN` is set (default `org_id`) and the principal is
   authenticated, `build_predicate` **drops** any client-supplied filter on that
   column and **ANDs** `<tenant_column> = <principal.tenant_id>` into every
   subscription.
2. A client requesting a *different* tenant's value silently gets its own: the
   server honors the request against the client's real scope, not the requested
   one. The predicate is never the impossible `org=X AND org=Y`.
3. Anonymous principals (`NOSTOS_SYNC_AUTH=none`) get no injection — there is no
   tenant to scope to (single-tenant dev mode only).

## Rationale

- The tenant filter is a **server responsibility**, derived from the
  authenticated principal — exactly how Postgres RLS works (the row filter is
  applied from the session role, never from the query the client sends).
- Silently overriding (rather than 403-ing) is the secure default that still
  serves the client: a buggy client that asks for the wrong tenant still gets its
  own data, not an error, and never escapes its scope.
- This is the minimal Phase-0 tenant isolation. It proves the path; full RLS
  (row-level security policies evaluated against the principal, not just a
  column equality) is the Phase-2 generalization.

## Consequences

**Positive:** cross-tenant reads are impossible regardless of what the client
subscribes to; the enforcement is server-side and uniform; the client SDK (when
built) doesn't even need to know the tenant column — the server adds it.

**Negative:** Phase-0 resolution is a single equality column (`org_id = tenant`).
Real multi-tenant schemas often have compound scoping (org + project, or
role-dependent visibility). **Mitigation:** the `Predicate` engine (ADR-0012)
generalizes to boolean trees; tenant injection then becomes "evaluate the
configured tenant policy against the principal," not "AND one equality." The
seam is `build_predicate` — one function.

## Alternatives considered

- **Trust the client's predicate (the original):** rejected — cross-tenant data
  leak. This ADR exists because of it.
- **403 on a mismatched tenant filter:** rejected as the default — it surfaces a
  footgun as an error rather than silently doing the safe thing. (Could be a
  future strict mode.)
- **Postgres RLS as the sole mechanism:** deferred — requires running queries
  through a per-principal session role, which the current read-path (WAL →
  fan-out) doesn't do. The injected column is the WAL-path equivalent.

## References

- Code: `crates/nostos-infra/src/transport.rs` (`build_predicate`),
  `crates/nostos-domain/src/principal.rs`.
- Test: `crates/nostos-infra/tests/auth_sync.rs`
  (`tenant_filter_is_server_enforced_not_client_attested`).

## Addendum (2026-08-17): the snapshot path joins the seam

The subscribe-time snapshot (`SnapshotSource`) shipped with a ponytail: an
UNFILTERED SELECT, documented as "multi-tenant deploys must NOT wire a
snapshotter". The audit of 2026-08-17 (H1) found the composition root wiring
`PgSnapshotter` unconditionally under `NOSTOS_REPLICATOR=pg` — the ponytail's
precondition was enforced nowhere, so any subscriber in a supabase-jwt
deployment received EVERY tenant's rows on subscribe (live fan-out was
filtered; the snapshot was not).

Fix: `SnapshotSource::snapshot` now takes `Option<TenantScope>` — the same
`Principal::tenant_scope` seam this ADR's read path and ADR-0018's write path
use, so all three enforcement points cannot drift. `PgSnapshotter` appends
WHERE "<tenant_col>"::text = $1 with the tenant value BOUND (the ::text
column cast keeps the bind type-correct for uuid/int tenant columns).
`None` (anonymous / single-tenant) stays legitimately unfiltered. Regression
test: `crates/nostos-infra/tests/e2e_pg_snapshot_tenant_scope.rs` (PG-gated).
