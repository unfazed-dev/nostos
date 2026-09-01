# v0.2.0 security audit — findings, fixes, and what's still open

Triggered by "smoke, pressure, stress + e2e test nostos security and whether it
even has one" during the v0.2.0 release hold (2026-09-01). Three parallel
research passes: web best-practice/CVE survey, auth implementation audit, and an
untrusted-input surface map. Everything below was verified against the code or
run, not taken on report.

## Does Nostos have a security model? Yes — and the reasoning is sound

`docs/SECURITY-MODEL.md` gets the load-bearing part right: Postgres RLS is
bypassed **twice** in this architecture — logical replication has no session, and
write-back runs on a privileged connection — so Nostos itself must be the
authorization layer. Anyone reaching for "we'll just use RLS" has already lost.

Enforcement that exists and works:

- ADR-0010 (auth/principal), ADR-0011 (server-enforced predicates), ADR-0018
  (write-path tenant enforcement).
- Tenant derives from the JWT `sub`, server-side. `RESERVED_CLAIMS`
  (`auth.rs:65`) blocks `tenant_id` from both the `extra` map and the typed
  field. **No client-supplied path reaches the tenant filter** — audited, confirmed.
- Client-attested filters on the tenant column are dropped and replaced with
  `tenant = principal.tenant_id`, including on `where_sql` subscriptions.
- Claim caps reject oversize rather than truncating (`auth.rs:344-353`).
- Admin token compare is constant-time (`subtle::ct_eq`, `admin_auth.rs:66`),
  `MIN_ADMIN_TOKEN_LEN=32`, and unset env → **404, fails closed**.
- Live sockets close on token expiry (Close 4401, `transport.rs:640-644`, ADR-0029).
- Tenant-isolation tests already exist: `auth_sync.rs`, `e2e_pg_tenant_guard.rs`,
  `e2e_pg_snapshot_tenant_scope.rs`, `ws_contract.rs`,
  `e2e_pg_sync_streams.rs:524`.

Two things I suspected and was **wrong** about, recorded so the guess isn't inherited:

1. **JWT algorithm confusion is not exploitable here.** `auth.rs:199-214` reads
   `alg` from the header but uses it to select a *verifier*, not key material:
   HS256 → the configured secret only; RS256/ES256/EdDSA → JWKS only; anything
   else rejected. A JWKS reader cannot forge an HS256 token because the HS256 arm
   never touches JWKS material. `alg: none` cannot even be expressed —
   jsonwebtoken 10.4's `Algorithm` enum has no such variant, so `decode_header`
   fails first. The design comment at `main.rs:389-395` is accurate.
2. **`where_sql` is not a SQL injection surface.** `parse_predicate_expr`
   produces a `PredicateExpr` data structure; it is never concatenated into a
   query. This matters because CVE-2026-40906 (ElectricSQL, CVSS 9.9) is exactly
   that bug in a directly comparable product — a permissive AST-to-SQL compiler
   with a catch-all branch. Nostos's design avoids the class rather than
   patching instances of it.

## Fixed this pass

### 1. Remote process abort via `where_sql` — CRITICAL (`3b23b04`)

`crates/nostos-domain/src/predicate_compile.rs` is recursive descent with **no
depth bound**. Confirmed by running it, not by reading it:

```
depth 10,000 → parses fine
depth 50,000 → fatal runtime error: stack overflow, aborting
```

50k `"NOT "` is 200 KB — trivially inside the (then 16 MiB) frame ceiling. A
stack overflow in Rust is a **SIGABRT, not a panic**: `catch_unwind` does not
contain it, the tokio worker does not isolate it, the whole process dies and
fan-out stops for every tenant. Under the default `NOSTOS_SYNC_AUTH=none` it
needs no credential at all. The repro used the 8 MiB main thread; tokio workers
get 2 MiB, so the real cliff in production is roughly a quarter of that.

Two bounds were needed, because either alone leaves a hole:

- `MAX_DEPTH = 64` — bounds recursion, i.e. stack.
- `MAX_TOKENS = 4096` — bounds the *flat* variant. `a=1 OR a=1 OR …` is millions
  of nodes at depth **one**, so a depth cap sails straight past it. That shape is
  arguably worse: `PredicateExpr::matches` re-walks every node for every
  replicated event inside the **shared** fan-out loop
  (`nostos-application/src/fanout.rs:251-265`), so one client's oversized filter
  taxes delivery for everyone. Nodes ≤ tokens, so one check at the entrypoint
  bounds both.

`MAX_TOKENS` is the number most likely to be wrong, since it bounds the *width*
of a legitimate filter. Checked before settling on 4096: the grammar has **no
`IN` operator**, so the "client syncs its 2,000 project IDs" shape that would
blow a width cap cannot be expressed. The widest expressible filter is a chain of
comparisons at ~4 tokens each, so 4096 admits roughly a thousand terms — far
past anything a subscription filter should carry. Revisit this constant if `IN`
is ever added to the grammar.

Regression test: `crates/nostos-domain/tests/predicate_dos.rs` — deep `NOT`, deep
parens, wide flat `OR`, plus a "realistic predicates still parse" guard so the
bounds can't be tightened into a functional regression. Note the failure mode:
if this regresses the test binary does not fail, it *dies*, and the harness
reports a signal instead of an assertion.

### 2. JWKS refetch storm during an outage — HIGH (`bcb4b38`)

The `MIN_REFETCH_INTERVAL` guard sat **inside** `if is_fresh` (`jwks.rs:135`).
`fetched_at` only advances on a *successful* fetch, so during a JWKS outage the
cache reads "stale" forever and the rate limit was skipped entirely — every
inbound token drove its own fetch, under the write lock, behind a 10s client
timeout. That serializes all authentication into a self-inflicted outage and
points an amplifier at the IdP. An attacker spraying random `kid`s gets it free.
Same shape as GHSA-qw3h-qqm9-jrw8 (RabbitMQ) and CVE-2026-48524 (PyJWT). The
module doc at `jwks.rs:13-15` claimed the limit was "independent of `ttl`" — the
opposite of what the code did.

**First fix was wrong and is worth recording.** Making the rate limit
unconditional broke `ttl_expiry_triggers_refetch` and
`key_rotation_old_kid_rejected_new_kid_accepted` — because it also blocked
legitimate TTL-driven refresh, and key rotation is itself a security property.
Trading a DoS for a rotation hole is not a fix. The real hole is narrower: with a
*healthy* endpoint one stale fetch succeeds, the cache goes fresh, and the
unknown-`kid` path bounds the attack to one fetch per TTL. Only the **failure**
path is unbounded. Final fix adds `last_fetch_failed` and backs off only when the
cache is stale *and* the previous attempt errored.

New tests: `failing_jwks_backs_off_instead_of_refetching_per_request` and
`jwks_recovers_after_outage_once_backoff_elapses`, plus a `set_failing()` toggle
on the test fixture — which previously could not simulate an outage at all, which
is precisely why the gap survived. The pre-existing rate-limit test used a
10-minute TTL, so it only ever exercised the fresh-cache branch.

### 3. Unbounded WebSocket message size — MEDIUM (`6239e1e`)

`ws.on_upgrade()` was called on the bare `WebSocketUpgrade`, so axum 0.7 defaults
applied: **64 MiB message / 16 MiB frame**, per connection. At a 1k-client device
cap that admits a ~64 GB worst-case buffer ceiling, reachable by any client that
authenticates and then sends large frames. Now 4 MiB message / 1 MiB frame.
Everything a client sends is small JSON; blobs go out-of-band on the attachment
plane (ADR-0034). Checked and *not* an issue: `permessage-deflate` is not
enabled (`tokio-tungstenite 0.23`, no deflate feature), so there is no
decompression-bomb path. Also checked: `nostos-client` sets no `WebSocketConfig`
of its own, so these server-side inbound caps cannot clip a client that was
relying on a larger one.

**Not verified locally:** the real-Postgres e2e needs Docker, which was not
running on this machine. The predicate bound touches compilation exercised by
`e2e_pg_tenant_guard.rs`, `e2e_pg_snapshot_tenant_scope.rs`, and
`e2e_pg_sync_streams.rs`; the WS caps sit on the path every `nostos-client` test
drives. CI's `real-Postgres logical-replication e2e` job is the gate for both —
do not treat these fixes as verified until that job is green.

### 4. Bearer token written into request spans — MEDIUM (`7c1ce44`)

`/sync` accepts `?token=<jwt>` because browsers cannot set `Authorization` on a
WS handshake — a legitimate need. But all three `TraceLayer::new_for_http()`
sites used `DefaultMakeSpan`, which records the full URI, writing a **live
credential** into every request span and from there to stdout and any log
aggregator. Replaced with `redacted_request_span`, which records the path only.
Reverse proxies keep their own access logs, so this does not close the whole
class — it stops Nostos from being the component that leaks it.

### 5. `StaticBearerAuth` compare — LOW (`7c1ce44`)

`got != self.digest` early-exits on the first differing byte. Genuinely low risk
(SHA-256 digests, not invertible, so a prefix oracle yields no token bytes) but
the doc comment above it promised timing carried no information. Now
`subtle::ConstantTimeEq`, so the claim is true rather than nearly true.

## Open — needs a decision, not a patch

### The fail-open default pair (USER DECISION PENDING)

`NOSTOS_BIND` defaults to `0.0.0.0:8800` (`main.rs:44`). `NOSTOS_SYNC_AUTH`
defaults to `none` → `AllowAnonymous`, which injects **no tenant filter**. Out of
the box, `nostos-server` is an unauthenticated sync server on every interface.
`SECURITY-MODEL.md` says dev-only and there is a startup `warn!`, but nothing
enforces it, and a warning scrolls past in container logs.

Industry precedent is one-sided: Vault's `-dev` binds `127.0.0.1` even when
enabled; Mem0 refuses to boot without a JWT secret. The counter-examples
(n8n instances left on defaults, agent frameworks defaulting to a synthetic
full-access principal) are exactly the "deployed to cloud without switching
modes" scenario.

Recommendation: **refuse to start when `sync_auth=none` and the bind address is
not loopback**, unless an explicit `NOSTOS_INSECURE_ANONYMOUS=1` is set. Keep both
current defaults; only the *pair* is fatal. Costs local dev nothing, cannot be
missed, keeps an escape hatch. Rejected alternatives: defaulting the bind to
loopback silently breaks existing Docker deployments; defaulting to
`supabase-jwt` makes the quickstart require a Supabase project.

## Open — confirmed gaps, not yet fixed

Ordered by exploitability.

1. **`ack` is unvalidated** (`wire.rs`, `transport.rs`). A client can ack an LSN
   it never received. Slot advance uses `min_acked_lsn` across sessions, so
   acking *high* cannot skip other sessions' data — but acking *low forever*
   holds the replication slot back, and unbounded WAL growth is a disk-exhaustion
   attack on the Postgres primary. No monotonicity or in-flight check found.
   Needs a test either way.
2. **No per-principal connection cap.** `DeviceCapReached`
   (`application/src/session.rs:26`) is a *global* licensed-session count, so one
   tenant opening `cap` sockets locks out every other tenant. Per-socket caps
   exist; per-principal does not.
3. **Unbounded snapshot.** `snapshot_source.rs:143` is `SELECT * FROM
   {quoted_table}` with no `LIMIT` and no cursor cap, ×32 tables/socket.
4. **`nbf`, `aud`, `iss` are not validated** on either verifier path.
   `validate_aud=false` is load-bearing — Supabase tokens carry
   `aud:"authenticated"` and leaving the default would reject all of them — but
   `iss` is unchecked, so a token from *any* issuer with a matching key passes.
   Library defaults otherwise apply (`required_spec_claims={"exp"}`, `leeway=60`,
   `validate_nbf=false`).
5. **An HS256 token minted without `exp` is a permanent credential.**
   `auth.rs:252` enforces `exp` only when present, and the handshake arms no
   close deadline without one — so such a token syncs forever and survives
   revocation. **This is deliberate, not an oversight:** the comment at
   `auth.rs:248-251` ties it to ADR-0029 §Decision-4 and states it preserves
   Phase-0 behaviour that the existing tests depend on (their tokens carry no
   `exp`). The JWKS path cannot produce this — `exp` is a required spec claim
   there. Aligning HS256 with JWKS is a one-line change plus test fixture
   updates, but it is a **breaking auth change** and belongs to the owner, not
   to an audit pass.
6. **Rotated keys stay valid through an outage.** `jwks.rs:158-162` — a failed
   fetch only warns and leaves `cache.keys` intact, then serves from it. Rotation
   is observed only via a *successful* fetch, so a key the IdP revoked keeps
   verifying for the whole outage. Fail-closed for unknown kids, fail-open for
   rotated ones. The back-off fix above changes fetch *frequency*, not this
   behaviour. The trade is real in both directions: failing closed on a JWKS
   outage converts an IdP blip into a total auth outage. The usual resolution is
   a bounded staleness window — serve stale keys for N minutes, then refuse —
   which is a policy choice with a availability cost, so it is listed here rather
   than applied.
7. **Unauthenticated metadata routes.** `GET /schema`, `GET /rules`, `GET
   /metrics` have no auth (acknowledged in-code as a v2 item). Leaks publication
   metadata, the active ruleset, and operational counters. `CorsLayer::permissive()`
   when `NOSTOS_CORS_ORIGINS` is empty (`main.rs:1556-1558`).
8. **No admin-token rate limiting.** Mitigated by `MIN_ADMIN_TOKEN_LEN=32`, but
   whoever can `PUT /rules` owns the entire tenant model.
9. **Cross-tenant existence oracle.** `write_back.rs:467-469` returns `row {pk}
   in {table} belongs to a different tenant` to the client verbatim —
   distinguishable from "row absent", so pk enumeration reveals which rows exist
   under other tenants. Existence and ownership, not contents.
10. **No `Origin` check on WS upgrade.** Not classic CSWSH — auth is a bearer
   token, not an ambient cookie, so a cross-origin page gets an uncredentialed
   socket. Real exposure is `AllowAnonymous` deployments, where any page on the
   internet can open the socket. Folds into the fail-open decision above.
11. **`Not` over an absent column over-delivers** (`predicate.rs`, documented
    deliberately). Three-valued-logic gap. Cannot shed tenant — the tenant leaf
    is a sibling `And` — so it widens delivery only inside the attacker's own
    tenant.

## Recommended next test, not yet written

The highest-value remaining test is the **tenant-isolation matrix**, modelled on
CVE-2026-30870 (PowerSync): sync rules were silently *not applied* for certain
query shapes — authorized-looking config, unauthorized delivery, no error. That
is a coverage gap, not a missing feature, so review cannot find it.

Enumerate every subscription shape the wire protocol accepts — plain table,
`where_sql`, sync stream, empty filter, filter on a column absent from the
entitlement predicate, resubscribe, rules reload mid-flight, reconnect-with-resume
— and for each, assert as tenant A that **zero** tenant-B rows are ever
delivered. Diff the delivered row set against the tenant boundary rather than
asserting the query looks right.

Worth doing under churn specifically: `chaos.rs::conservation_under_churn`
already drives connect/disconnect concurrently with `fan_out`, and that is the
shape that produced the DashMap deadlock fixed earlier in this release. Check
whether `rules_reload.rs` / `rules_mode_switch.rs` reload *concurrently with*
in-flight fan-out or only between quiescent phases — if quiescent, a predicate
swapped mid-flight is untested.

Second: whether row-level entitlement is re-evaluated when a row mutates *out
of* scope mid-connection. If a privileged connection flips a row's tenant column
while tenant A is subscribed, does A get a retraction? Supabase Realtime caches
channel policies for the connection lifetime and documents exactly this gap; no
vendor in this category publishes clear guidance on the row-mutates-out-of-scope
case. Treat it as a designed-for property, not an assumed one.
