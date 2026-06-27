# ADR-0010: /sync authentication and the Principal type

- **Status:** Accepted (shipped — Tier 0 security foundation)
- **Date:** 2026-06-27

## Context

The `/sync` WebSocket endpoint had **no authentication at all**. The first frame
a client sent was parsed as a `Subscribe { predicate }` and honored verbatim —
any connected client could subscribe to any table and read any tenant's rows.
This was flagged by the architecture review not as a gap but as a
**vulnerability**: an unbounded data-exfiltration hole. The JWT verifier
(`Hs256Verifier`) and license machinery live in `nostos-cloud`, but `/sync` lives
in `nostos-server` → `nostos-infra`, and these crates were disjoint (no dependency
edge between them).

## Decision

1. **A new pure type in `nostos-domain`: `Principal { account_id, tenant_id }`.**
   It lives in the shared low ring (where `Tier` already lives) so both the
   transport and the session store can see it without inverting layering.
2. **A new application port: `SyncAuth::authenticate(&token) -> Option<Principal>`.**
   The transport resolves a bearer token to a principal *before* the WebSocket
   upgrade; `None` → HTTP 401 (no upgrade).
3. **Two adapters in `nostos-infra`:**
   - `AllowAnonymous` — mints `Principal::anonymous()` for every connection. The
     `NOSTOS_SYNC_AUTH=none` default for OSS self-host dev (single-tenant only;
     logs a loud warning).
   - `SupabaseJwtAuth` — HS256-verifies a Supabase JWT (mirrors `nostos-cloud`'s
     `Hs256Verifier` algorithm exactly), lifts `sub` as account and tenant id.
4. **Token sources:** `Authorization: Bearer <token>` header **and** `?token=`
   query param (browsers cannot set headers on a WS handshake).
5. **Config:** `NOSTOS_SYNC_AUTH=none|supabase-jwt` +
   `NOSTOS_SUPABASE_JWT_SECRET`. A managed multi-tenant deploy MUST set
   `supabase-jwt`.

## Rationale

- Auth is resolved **before** the upgrade so an unauthenticated connection never
  reaches the session store — the cheapest correct place to gate.
- `Principal` is a domain type, not an infra concern: the predicate-enforcement
  that depends on it (ADR-0011) stays in the pure layer, testable without a
  runtime.
- Duplicating the ~30 lines of HS256 verification across the ring boundary is
  preferable to forcing `nostos-server` → `nostos-cloud` (which would pull the
  control-plane's sqlite/stripe/cookies into the sync server). The ponytail
  rule: prefer a small duplication to a layering inversion.

## Consequences

**Positive:** the data-exfiltration hole is closed; every connection now carries
an identity the server can enforce; the OSS dev path (`none`) is explicit and
loudly warned, not silently insecure.

**Negative:** Phase 0 sets `tenant_id = sub` (one tenant per Supabase user) —
sufficient to prove the enforcement path but not a real multi-tenant claim. Real
tenant resolution (an `app_metadata` claim or an RLS join) is a Phase-2 follow-up
under ADR-0011. **Mitigation:** the type is uniform (`tenant_id: String`), so
swapping the resolution later is a one-method change in `SupabaseJwtAuth`, with
no transport/store changes.

## Alternatives considered

- **Move `nostos-cloud`'s `JwtVerifier` into a shared crate:** rejected — adds a
  crate + a dependency edge to avoid ~30 lines of crypto.
- **Auth as a tower middleware layer:** rejected — the WS upgrade path doesn't
  flow through tower request middleware cleanly; resolving inline in the handler
  before `on_upgrade` is simpler and correct.
- **Anonymous-only (ship without auth):** rejected — the architecture review
  classified this as a vulnerability, not a feature gap.

## References

- Code: `crates/nostos-domain/src/principal.rs`, `crates/nostos-infra/src/auth.rs`,
  `crates/nostos-infra/src/transport.rs` (`sync_handler`).
