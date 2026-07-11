# ADR-0018: Write-path tenant enforcement (extends ADR-0011 to writes)

- **Status:** Accepted (shipped)
- **Date:** 2026-07-12

## Context

ADR-0011 closed the cross-tenant **read** hole: the server injects the tenant
filter into every subscription predicate, so a client can never read another
tenant's rows regardless of what it requests. ADR-0013 (v1 addendum) shipped
direct write-back over the sync socket, but its trust boundary was
allowlist-only: any table in `NOSTOS_WRITE_TABLES` was writable by any
authenticated client, with **no tenant scoping at all**. A client authenticated
as tenant `acme` could upsert or delete tenant `other`'s rows outright — the
write-side mirror of the hole ADR-0011 closed for reads.

## Decision

When tenant enforcement is active — a tenant column is configured AND the
principal is not anonymous, the exact same condition ADR-0011 uses for reads
([`Principal::tenant_scope`], `crates/nostos-domain/src/principal.rs`) — the
write path enforces tenant scope at two points:

1. **Upsert (INSERT):** the tenant column in the payload is **force-stamped**
   to the principal's tenant value, overwriting any client-supplied value.
   This makes the write path symmetric with ADR-0011's read-side injection:
   the client's own claim is never trusted, only silently overridden.
2. **Upsert (ON CONFLICT DO UPDATE) and DELETE:** the write is constrained so
   it can only affect a row that ALREADY belongs to the principal's tenant.
   A write that targets an existing row owned by a **different** tenant is
   **rejected outright** (`WriteBackError::Forbidden`, surfaced as
   `WriteResult{ok:false}` on the wire) — not silently applied and not
   silently dropped. This is a deliberate departure from ADR-0011's
   "silently override" philosophy for reads: for a write, telling the client
   nothing happened when it actually didn't is misleading (an optimistic
   client UI would show a change that never landed). The client must be able
   to tell the difference between "my write succeeded" and "my write was
   rejected."

The `Principal` (and the operator-configured tenant column) is threaded from
the transport into `dispatch_write` and the `WriteBack` port as a new
`Option<TenantScope<'a>> { column, value }` parameter (`nostos-domain`, pure
type — no I/O). `None` means no tenant enforcement is active (anonymous /
single-tenant deploys); behavior there is byte-for-byte unchanged from
pre-ADR-0018. The allowlist check (ADR-0013) remains first and unconditional.

### Mechanism (PgWriteBack, `crates/nostos-infra/src/write_back.rs`)

- **Upsert force-stamp:** the tenant column is inserted/overwritten in the
  parsed JSON payload before the column list is built, so it flows through
  the SAME identifier-regex + parameterized-value path as every other
  column — no special-casing of the SQL builder.
- **Upsert conflict guard:** the `ON CONFLICT (id) DO UPDATE SET ... WHERE
  "tenant_col" = EXCLUDED."tenant_col"` clause. Because `EXCLUDED`'s tenant
  column is always the force-stamped principal value, this guard reads as
  "only update if the existing row already belongs to this tenant." A fresh
  insert (no conflict) always affects exactly one row; a conflict where the
  guard fails affects zero. `PgWriteBack` checks `rows_affected`: under an
  active tenant scope, `0` can **only** mean the guard fired, so it maps to
  `Forbidden`.
- **Delete guard:** a single-round-trip CTE —
  `WITH deleted AS (DELETE ... WHERE id=$1 AND tenant_col=$2 RETURNING 1)
  SELECT count(*) FROM deleted, EXISTS(SELECT 1 FROM t WHERE id=$1)` —
  distinguishes "row deleted" (count > 0, success) from "row belongs to
  another tenant" (count = 0, still exists → `Forbidden`) from "row never
  existed" (count = 0, doesn't exist → idempotent success, preserving the
  pre-ADR-0018 `delete_of_missing_row_is_success` contract).

## Rationale

- **Symmetry with ADR-0011.** Both paths derive scope from the same
  authenticated `Principal` via the same seam (`Principal::tenant_scope`),
  so the read and write enforcement conditions cannot drift apart —
  `build_predicate` (read) and `dispatch_write` (write) both call it.
- **Reject, don't silently no-op, for writes.** Reads default to "silently
  scope to what's actually yours" because there's no notion of a failed
  read. Writes are different: a client that thinks its write succeeded when
  it didn't is a correctness bug in the client's local state, not just an
  access-control nicety. `Forbidden` makes the failure observable.
- **The tenant column becomes just another column.** Rather than adding
  bespoke SQL-generation logic for the tenant column, stamping it into the
  payload before the existing column-processing pipeline runs means it gets
  identifier validation, sorted ordering, and typed-value binding for free.

## Consequences

**Positive:** writes can no longer read, hijack, or delete another tenant's
row via the write-back socket, closing the write-side mirror of ADR-0011's
read hole. The enforcement condition is provably identical to the read path
(shared `tenant_scope` seam) rather than a parallel, potentially-drifting
check.

**Negative — documented trade-off, not swept under a ponytail:** the
tenant-scoped DELETE's CTE reveals to a caller whether a primary key they
already named **exists at all**, even under another tenant (a `Forbidden`
response vs. an idempotent-success response). This is a narrow information
disclosure: it tells the caller "that specific id exists (elsewhere)"
without revealing anything else about the row. We accept this for v1 because
(a) the caller supplied the pk themselves — the write already asserted
ownership over that specific id, so confirming its existence adds at most
one bit beyond what an upsert-conflict attempt on the same pk would already
reveal (Consequence above), and (b) the alternative (always returning
idempotent success, hiding cross-tenant existence entirely) would silently
mask genuine cross-tenant delete attempts as no-ops, which is a worse
default for a security-sensitive code path. A stricter mode that always
folds "belongs to another tenant" into idempotent success is a config-flag
sized change if a design partner needs it — no one has asked. Also
unaddressed for v1 (Phase-2, same class as ADR-0011's compound-scoping
follow-on): the tenant column is assumed present on every allowlisted table,
mirroring the same assumption the read path already makes.

## Alternatives considered

- **Silent no-op for cross-tenant writes (mirroring ADR-0011's read
  behavior):** rejected — see Rationale. An optimistic client that thinks a
  write landed when it silently didn't is a data-integrity bug waiting to
  happen, not a security nicety.
- **A separate SELECT before every delete to distinguish "missing" from
  "someone else's":** rejected in favor of the single-round-trip CTE — same
  information, one fewer database round trip, no additional TOCTOU window
  beyond what the existing single-connection model already has.
- **Reject the whole class of stamping (require the client to omit the
  tenant column and have the server always inject it, erroring if present):**
  rejected — a stricter reject-on-presence policy adds friction (every
  client must know to omit the field) for no additional security over
  silently overriding it (the override is unconditional either way).

## References

- Code: `crates/nostos-domain/src/principal.rs` (`TenantScope`,
  `Principal::tenant_scope`), `crates/nostos-application/src/ports.rs`
  (`WriteBack::upsert`/`delete` signatures, `WriteBackError::Forbidden`),
  `crates/nostos-infra/src/write_back.rs` (`stamp_tenant_column`, the
  `PgWriteBack` guard SQL), `crates/nostos-infra/src/transport.rs`
  (`dispatch_write`, `build_predicate` — both call `Principal::tenant_scope`).
- Tests: `crates/nostos-infra/src/write_back.rs` (`stamp_tenant_column_*` unit
  tests), `crates/nostos-infra/tests/e2e_pg_writeback.rs`
  (`cross_tenant_insert_is_stamped_to_callers_tenant`,
  `cross_tenant_upsert_conflict_is_rejected`,
  `cross_tenant_delete_is_rejected_row_survives`,
  `cross_tenant_delete_of_absent_row_is_idempotent_success`,
  `own_tenant_writes_flow_normally`).
- Depends on: ADR-0011 (server-enforced predicates), ADR-0013 (write-back v1).
- Plan: `docs/plans/flutter-supabase-plug-and-play-launch.md` (W1).
