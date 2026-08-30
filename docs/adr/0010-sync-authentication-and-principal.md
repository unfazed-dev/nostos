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

## Addendum: JWKS / RS256+ES256 support (2026-07-12)

**Context.** Supabase projects created since 2025-10-01 sign user JWTs with an
asymmetric key by default (RS256; ES256/EdDSA optional), publishing the public
keys at `<project>/auth/v1/.well-known/jwks.json` (edge-cached ~10 min). The
HS256-only verifier this ADR originally shipped fails against every such
project — the launch plan's W2 workstream.

**Decision.**

1. `SupabaseJwtAuth` (`crates/nostos-infra/src/auth.rs`) now routes on the
   token's header `alg`: `HS256` verifies against the existing legacy shared
   secret (`verify_supabase_hs256`, **unchanged**); `RS256`/`ES256`/`EdDSA`
   verify against a fetched-and-cached JWKS (`crates/nostos-infra/src/jwks.rs`,
   `JwksVerifier`). Both paths mirror `Principal` extraction exactly (`sub` →
   `account_id` and `tenant_id`) — downstream tenant enforcement (ADR-0011,
   ADR-0018) is unaffected either way.
2. **No algorithm confusion.** A key's algorithm is fixed from its JWK's key
   type at cache time; `jsonwebtoken`'s `Validation` is then pinned to exactly
   that algorithm. An HS256 token is never checked against JWKS key material
   (and vice versa) — the header `alg` alone routes to one verifier or the
   other, and `alg: none` fails immediately at header-parse time (the
   `Algorithm` enum has no such variant).
3. **Cache policy.** JWKS entries are cached for a TTL (default 10 min,
   matching Supabase's edge cache). An unknown `kid` triggers one refetch;
   that refetch attempt is additionally rate-limited (5s) independent of TTL,
   so a client presenting many distinct bogus `kid`s can't turn verification
   into a JWKS-endpoint hammer. A fetch failure fails closed.
4. **Config** (`crates/nostos-server/src/main.rs`): `NOSTOS_SYNC_AUTH=supabase-jwt`
   now requires at least one of `NOSTOS_SUPABASE_JWT_SECRET` (legacy HS256) or
   `NOSTOS_SUPABASE_URL`/`NOSTOS_SUPABASE_JWKS_URL` (JWKS). Both may be set —
   each token's `alg` picks the verifier. See `.env.example`.
5. **Dependency:** `jsonwebtoken` (MIT, `rust_crypto` backend — RustCrypto
   family, consistent with the `rsa`/`p256` test-only crates rather than
   adding a second crypto stack like `aws-lc-rs`) for JWK parsing, key
   material, and algorithm-pinned validation. `reqwest` (already a workspace
   dependency via `nostos-cloud`'s Stripe client) for the JWKS fetch.

**Known asymmetry (intentional, not a regression):** the JWKS path validates
`exp` (via `jsonwebtoken`'s default `Validation`, which requires and checks
it); the legacy HS256 path still does not (Phase 0 laxity, documented above,
left untouched so existing behavior/tests are unaffected). Neither path
checks `aud`/`role` — `Principal` only ever lifts `sub`, so there is nothing
those claims would currently gate.

**References:** `crates/nostos-infra/src/jwks.rs`,
`crates/nostos-infra/src/auth.rs`, `docs/plans/flutter-supabase-plug-and-play-launch.md` (W2).

## Addendum: static-bearer single-principal mode (2026-08-30)

**Context.** The two existing modes bracket the deployment space awkwardly for
a single-tenant self-host that wants working push notifications:
`NOSTOS_SYNC_AUTH=none` mints the anonymous principal, and the push-token
registry REFUSES the anonymous one (ADR-0037 §3 — there is no identity to
stamp a token row with, and an open register route lets anyone poison the
registry). The only authenticated mode, `supabase-jwt`, requires operating a
Supabase project — wrong for a personal device pair or a lab rig that just
needs its push receipt to land.

**Decision.**

1. New `NOSTOS_SYNC_AUTH=bearer` mode + `StaticBearerAuth` adapter
   (`crates/nostos-infra/src/auth.rs`): one configured shared secret
   (`NOSTOS_SYNC_BEARER_TOKEN`, required non-empty — the server fails fast
   without it) resolves every connection presenting it to the same fixed,
   NON-anonymous principal (account `local`, tenant `local`).
2. The push-token registry works unchanged on top of it: registration stamps
   `local`/`local`, and a bearer that doesn't match gets the same 401 as
   before. No new route, no registry change.
3. **Timing posture:** presented token and configured secret are both
   SHA-256-hashed and only the fixed-length digests are compared — the
   comparison's shape is identical for every input, so handshake timing does
   not leak secret bytes.
4. **Honest limits (documented on the adapter):** one shared credential per
   deployment — no per-device identity, no per-device revocation, rotation
   re-provisions every client. Anything multi-tenant stays `supabase-jwt`.

**References:** `crates/nostos-infra/src/auth.rs` (`StaticBearerAuth`),
`crates/nostos-server/src/main.rs` (config arm),
`docs/plans/b2-sync-first-phase1.md` (phase-1b consumer).
