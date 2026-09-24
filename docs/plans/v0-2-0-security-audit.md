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

### 1. Remote process abort via `where_sql` — CRITICAL (`814b8e2`)

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

### 2. JWKS refetch storm during an outage — HIGH (`0e221cd`)

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

### 3. Unbounded WebSocket message size — MEDIUM (`fe53ec3`)

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

### 4. Bearer token written into request spans — MEDIUM (`8f03405`)

`/sync` accepts `?token=<jwt>` because browsers cannot set `Authorization` on a
WS handshake — a legitimate need. But all three `TraceLayer::new_for_http()`
sites used `DefaultMakeSpan`, which records the full URI, writing a **live
credential** into every request span and from there to stdout and any log
aggregator. Replaced with `redacted_request_span`, which records the path only.
Reverse proxies keep their own access logs, so this does not close the whole
class — it stops Nostos from being the component that leaks it.

### 5. `StaticBearerAuth` compare — LOW (`8f03405`)

`got != self.digest` early-exits on the first differing byte. Genuinely low risk
(SHA-256 digests, not invertible, so a prefix oracle yields no token bytes) but
the doc comment above it promised timing carried no information. Now
`subtle::ConstantTimeEq`, so the claim is true rather than nearly true.

## Open — needs a decision, not a patch

### The fail-open default pair — DECIDED AND FIXED (`52f1561`)

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

**Implemented as recommended.** `exposes_anonymous_sync(bind)` gates the `"none"`
arm of the auth match: `sync_auth=none` + a non-loopback bind → `anyhow::bail!`
with a message naming all three ways out. An unparseable bind returns `false`
(not this guard's error to report; boot bails on it later with the canonical
message — and a bind the server can't parse is one it never listens on).

**Boot-verified, four cases, not just unit-tested:**

| bind | auth | hatch | result |
|---|---|---|---|
| `0.0.0.0:8877` | none | — | **refuses**, exit 1 |
| `0.0.0.0:8878` | none | `=1` | listens |
| `0.0.0.0:8879` | none | `=true` | listens |
| `127.0.0.1:8880` | none | — | listens (local dev untouched) |

**Two shipped configs paired anonymous with `0.0.0.0` and would have stopped
booting.** Both were found by grepping every launch site, not by assuming:

- `docker/docker-compose.stack.yml` sets `NOSTOS_BIND: 0.0.0.0:8800` and no
  `NOSTOS_SYNC_AUTH`. Now carries `NOSTOS_INSECURE_ANONYMOUS: "1"` with a comment
  saying why it is legitimate there and that a real deploy drops the line.
- **`nostos dev`** — the flagship onboarding command — defaults `bind` to
  `0.0.0.0` (so a physical phone on the LAN can reach the laptop) and
  deliberately emits *no* `NOSTOS_SYNC_AUTH` without a Supabase secret
  (`config.rs`, pinned by a test). It now emits the hatch explicitly in that
  branch only: `nostos dev` **is** the "I am developing" signal. A production
  deploy runs the `nostos-server` binary directly, never this path, and still
  hits the refusal. Both branches are pinned by tests — hatch present without a
  secret, absent with one.

`scripts/sdk-e2e.sh` binds `127.0.0.1:8801` (unaffected); `release.yml` only
builds binaries.

#### The escape hatch did not work when first written

`NOSTOS_INSECURE_ANONYMOUS=1` **failed to parse.** clap's stock `bool` accepts
only `true`/`false`, so `=1` — the universal shell/compose convention, and what
this server's *own refusal message* tells the operator to set — died with
`invalid value '1' for '--insecure-anonymous'`. The compose file, the CLI hatch,
and the error text all said `=1`; every one of them would have failed.

The unit test on the guard passed the whole time. Only booting the actual binary
caught it. A `parse_env_bool` value-parser now accepts `1/0`, `true/false`,
`yes/no`, `on/off`, and hard-errors on anything else (a typo'd hatch must never
silently read as `true`).

## Open — confirmed gaps, not yet fixed

> **Verification 2026-09-02 (code cross-check):** findings 2, 3, 6, 8, 9, 10,
> 11 are CLOSED-VERIFIED at HEAD with named tests. **Finding 4 is only
> half-closed** — `nbf`/`iss` are enforced on the JWKS path but not on the
> HS256 path (`auth.rs:262`), and neither path has a test. Per-finding table:
> "Verification 2026-09-02" under §3 of "Left deliberately". **Closed the
> same day in `899530e`** (HS256 `nbf`/`iss` enforced; JWKS now also requires
> `iss` when the allowlist is set; tests on both paths) — the heading above
> is accurate again.

> **Update 2026-09-02 — "fix all" pass COMPLETE. All ten findings in this
> section are fixed** (1–11 less the numbers already closed above), across
> commits `ac78ae5`, `0abeee0`, `b8b3d14`, `a69014f` and the Batch A commits.
> See `docs/plans/close-all-open-security-findings.md` for the batch plan and
> the design decisions read off the source.
>
> Per-finding: **1** slot eviction on by default (1 GiB); **2** per-principal
> session cap; **3** snapshot row cap that rejects rather than truncates;
> **4** `nbf` validated + opt-in `iss` allowlist; **5** `exp` required on HS256;
> **6** bounded JWKS staleness; **7** opt-in metadata protection; **8**
> failure-only admin throttle; **9** indistinguishable cross-tenant rejection;
> **10** opt-in WS origin allowlist; **11** three-valued predicate logic.
>
> **Two carried forward deliberately, not silently:**
>
> - **`nostos pull` cannot send a token.** `ProjectConfig` has no field for one,
>   and adding a credential store is a feature, not a security fix. Against a
>   server with `NOSTOS_PROTECT_METADATA=1` the CLI now fails with an error
>   naming the knob instead of a bare 401. **Follow-up — done in `b56fffe`:**
>   `nostos pull --token <TOKEN>` (env fallback `NOSTOS_TOKEN`) sends
>   `Authorization: Bearer <token>` on `GET /schema`; the 401 message names the
>   flag and the env var. Still no token field in `.nostos/config.json`.
>   `--token` beats `NOSTOS_TOKEN`, the value is redacted in `Debug`/`Display`,
>   and it is only sent over `https://` or to loopback — plain `http://` to a
>   non-loopback host needs `--allow-insecure-token`.
> - **CORS is unchanged.** `build_cors_layer` still returns
>   `CorsLayer::permissive()` on empty `NOSTOS_CORS_ORIGINS` — the documented
>   local-dev default, with `NOSTOS_CORS_ORIGINS` as the existing production
>   knob. Finding 7 should **not** be described as "CORS tightened".
>
> **Also recorded:** on `NOSTOS_SYNC_AUTH=none` the `/schema` gate is theatre,
> because `AllowAnonymous` accepts the empty token. The server warns at boot
> rather than implying protection it does not have.
>
> Two of this section's own pointers were wrong and are corrected there
> (a third, finding 7's `main.rs:1556-1558` for CORS, is now push-table
> parsing; the real code is `build_cors_layer`):
>
> - Finding 3's `snapshot_source.rs:143` is `prepare_columns`, a metadata-only
>   prepared statement that fetches zero rows. The unbounded reads are the
>   `client.query` calls in `snapshot` and `snapshot_stream`.
> - Finding 3 also had a half this section never mentioned: EVERY snapshot
>   failure is a server-side `warn!` after which the subscribe continues with
>   live fan-out only. A row cap alone would therefore have been invisible to
>   the client — the same "first sync is quietly wrong" shape as finding 7. The
>   cap now rejects the subscribe outright (table path) or emits a
>   `stream_error` frame (stream path). **The residual is a new finding: every
>   OTHER snapshot error is still silent to the client.**


Ordered by exploitability.

1. ~~**`ack` is unvalidated**~~ — **PARTLY WRONG AS ORIGINALLY WRITTEN, now
   fixed (`c856129`).** Correcting the record, because an audit that overstates
   a finding costs the next reader real time:
   - *"No monotonicity check found"* was **false**. `TokioEventSink::record_ack`
     (`router.rs:157`) has always been monotonic via a `compare_exchange_weak`
     loop — a lower ack is ignored. So *"acking low forever holds the slot
     back"* is not reachable by acking low: the sink simply won't go backwards.
     A client that never acks **at all** does hold the slot, but that is the
     already-documented ADR-0016 problem (a legitimately slow client looks
     identical), not an ack-validation bug.
   - *"A client can ack an LSN it never received"* was **true**, and is what
     got fixed: `record_ack` now clamps to the sink's `delivered_lsn`.
   - **It was never a leak.** Slot advance folds the *minimum* acked LSN across
     live sessions (`store.rs:233`), so an inflated ack cannot flush the slot
     past another session's data. Acking high only jams this session's own
     `admit` acked-range guard shut — the client silently stops receiving. That
     is self-harm, so the clamp is **defense-in-depth, not a security fix**,
     and is labelled that way in the code.
   - Fixing it required making `delivered_lsn` a true high-water mark
     (`fetch_max`, not `store`). Its doc always said *"highest LSN delivered"*
     but a snapshot row carries a lower base LSN than live traffic already
     delivered, so the ceiling could regress and clamp a legitimate ack down.
   - One test was found to be **fiction**:
     `stream_snapshot_after_acked_live_traffic_still_delivers` claimed to
     simulate "live traffic already delivered + acked at LSN 500" while only
     calling `record_ack(500)` — a state no real client can reach. It now uses
     `seed_acked_lsn`, which sets both cursors.

   **Closed (ADR-0043, commits `ac78ae5` + `be853dd`):** `NOSTOS_SLOT_MAX_LAG`
   now defaults to `1073741824` (1 GiB); `0` still means unbounded but the server
   logs a startup `warn!` naming the knob and the risk. Safe to default because
   enforcement only removes the slowest *session* (`fanout.rs` →
   `store.remove`) — the replication slot is never dropped, so the worst case
   is one reconnect + resync for a client a gigabyte behind. Pinned by
   `slot_max_lag_tests` in `nostos-server`; real-PG e2e green with the default.
   Eviction does nothing for an *abandoned* slot (server gone) — that is bounded
   only by Postgres `max_slot_wal_keep_size`, which `nostos doctor` now checks.
   See "Left deliberately" §2 below.
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

## The path audit — where the real bug was

The plan was a **shape** matrix: every subscription shape (`Any`, `Eq`, `Ne`,
ordered leaves, `And`/`Or`/`Not`, `where_sql`) crossed with two tenants,
asserting zero cross-tenant delivery.

**That matrix would have passed every cell, and proved nothing.** The tenant
clause is ANDed at the ROOT (`build_predicate` ends with `p.and_eq(tenant)`),
and `PredicateExpr::and` builds `And([self, other])`. So foreign-tenant
rejection is a property of `And`, not of the shape of the other conjunct. Twelve
shapes re-prove what `rules_scope_is_anded_with_tenant_scope` already proves.

The axis that can actually fail is **path**, not shape — which is also the real
shape of a well-known sync-rules CVE (CVE-2026-30870): rules *not applied* on one path, rather
than misapplied. Enumerating every path that can put a row in front of a client
found a live bug on the first one.

### 6. Op-log replay bypassed the ruleset — HIGH (fixed)

**The leak.** Two paths deliver rows, and only one enforced authorization:

| path | filter applied |
|---|---|
| live fan-out | `predicate.matches` — rules scope + tenant clause |
| op-log replay (reconnect) | **tenant only** |

`OpLogSource::replay_after(tenant_id, after_lsn)` runs
`WHERE tenant_id = $1 AND lsn > $2` — no table, no predicate. The rows went
straight to `sink_concrete.deliver_awaiting(ev)`, and the sink's `admit` gate
(`router.rs`) checks only open / acked / dedup. Nothing on that path consulted
the session predicate.

So a client that reconnects with a `resume_lsn` received **every row its tenant
wrote** since that LSN, including:

- rows from tables its own ruleset refuses to sync (a direct `subscribe` to
  them is rejected `NotSynced`);
- rows its row-level scope hides — e.g. under `scope = "owner_id = claims.sub"`,
  another user's rows in the same tenant.

Cross-tenant isolation held (the SQL does filter tenant). This is
**within-tenant privilege escalation**, and in a deploy whose rules exist to
separate users inside an org, that is the authorization boundary.

**Proven before fixed.** `replay_never_delivers_rows_from_an_unsynced_table`
failed on the unpatched tree with the `notes` row delivered to a `tasks`
subscriber. The test asserts `replay_calls == 1` first, so a broken epoch gate
that skipped replay entirely would fail loudly instead of passing vacuously.

**The fix** (`replay_admits` in `transport.rs`): re-apply the session predicate
on the replay path — table check, then the predicate over the row's JSON
payload, failing closed on a payload that won't decode.

Three details worth keeping:

1. **Deletes are table-check only.** A replayed delete carries no old image
   (`oplog.rs` drops it), so there are no columns to match. Failing closed would
   drop the delete permanently for an offline client, leaving a row that never
   goes away — the stale-row bug ADR-0014's reconcile boundary exists to
   prevent. That is a worse outcome than leaking a pk inside the client's own
   tenant and own subscribed table. Ceiling and upgrade path (log the scope
   columns into `nostos_oplog` at write time) are in the `ponytail:` comment.
2. **The `!events.is_empty()` guard was now wrong.** A non-empty replay can
   filter to zero. The old arm returned `Ok(())` and skipped the snapshot, so a
   fully-filtered replay would leave the client with neither replay nor
   snapshot — a silent gap. The count is now taken *after* filtering.
3. **The gate lives at the delivery site**, not in `OpLogSource`. The port takes
   `tenant_id` and knows nothing about predicates.

`replay_applies_the_rules_scope_not_just_the_table` is the one that
distinguishes a real fix from a table-name-only fix: under a ruleset scoped to
`status = 'open'`, a `status='closed'` row must not arrive while the `open` row
still does.

### 7. Stream snapshot skips the rules scope — HIGH (fixed, `4ae4259`)

Same class, second path. `register_stream` builds the session predicate with
`build_stream_predicate` (rules scope AND bound template AND tenant), but then
calls:

```rust
snap.snapshot_stream(&table, &bound, ..., principal.tenant_scope(tenant_column))
```

It passes `&bound` — the raw bound template — **not** the rules-scoped expr. So
a stream's initial snapshot is filtered by the template and the tenant, but not
by the ruleset's own scope, while live fan-out for the same stream is. Under a
ruleset with a row-level scope, the stream snapshot over-delivers exactly that
scope's worth of rows.

The naive fix (pass `predicate.expr`) would bake the tenant clause into the SQL
expression as well as the dedicated `tenant` argument, which breaks
deliberately-global tables that lack the tenant column
(`scope_if_column_present` skips the clause today). The correct fix passes
`rules_expr.and(bound)` and leaves the tenant travelling in its own argument.

**Fixed 2026-09-01.** `build_stream_predicate` now returns BOTH the session
predicate and the snapshot expr (`rules ∧ bound`, tenant deliberately absent),
so the two paths cannot be derived separately again — deriving the same
authorization twice is how this bug happened. `bound` is now *moved* into the
function rather than cloned, so the call site has no unscoped copy left to
reach for.

**Proven, not assumed.** `stream_snapshot_applies_the_rules_scope_not_just_the_template`
seeds three rows under scope `status = 'open'` + template `owner_id = :owner`.
Row `l2` is the load-bearing one: the template admits it, only the rules scope
hides it. Pre-fix the test yields `["l1", "l2"]`; post-fix `["l1"]`. Verified by
temporarily restoring the old behaviour, watching it fail, then reverting.

#### The trap this fix had to clear first

The obvious patch would have **broken every default deployment.** `PredicateExpr::and`
built `And([self, other])` unconditionally, and the SQL compiler
(`snapshot_source::compile_expr`) deliberately *refuses* `PredicateExpr::Any` —
a match-all marker reaching SQL means a widened snapshot. The zero-config `all`
sync mode decides `Allow(PredicateExpr::any())`, so `rules_expr.and(bound)`
would have produced `And([Any, template])` → compile error → swallowed by the
transport → downgraded to live-fan-out-only → **the client's first sync silently
returns nothing.** Exactly the `products`-catalog starvation already documented
in `snapshot_source.rs`.

So `and` now collapses `Any` (and `or` absorbs it), matching what `Predicate::and_eq`
/ `or_eq` already did one screen below. Semantics for `matches()` are unchanged
(`Any AND x ≡ x`); the change is that the tree stays compilable. Blast radius
measured: 132 domain + 214 infra tests, zero failures.

## Verification status — actually run, 2026-09-01

| suite | result |
|---|---|
| `make ci` (fmt + clippy `-D warnings` + full tests) | **green**, 0 failed |
| `nostos-infra --lib` | **211 passed, 0 failed** |
| real-Postgres e2e (`NOSTOS_E2E_PG=1`, `--test-threads=1`) | **296 passed, 2 failed, 0 skipped** |

Items 1 (predicate bound) and the boot-time tenant guard are now **verified**:
`0 skipped` confirms the pg suite really ran rather than self-skipping.

### Re-verified after the fixes (2026-09-02)

| suite | result |
|---|---|
| `make ci` | **`MAKE_CI_EXIT=0` — 972 passed, 0 failed, 0 ignored**; 78 test binaries, 0 `FAILED`; zero clippy warnings, zero fmt diffs |
| boot guard, 4 cases against the real binary | refuses the insecure pair; starts on `=1`, on `=true`, and on loopback-without-hatch |
| real-Postgres e2e — **all 15 `e2e_pg_*` binaries**, `NOSTOS_E2E_PG=1`, `--test-threads=1` | **44 passed, 0 failed, 0 ignored** |
| `e2e_pg_snapshot` (the 2 formerly-failing tests) | **2 passed, 0 failed** — `bench_apply` unpublished |
| `e2e_pg_sync_streams` (incl. `cross_tenant_param_abuse_never_leaks`) | **5 passed, 0 failed** |

The pg suite had to be run in two foreground batches: a whole-suite background
run was externally killed twice before finishing a single binary, and a partial
run is not a result. Per-binary counts are non-zero and `0 ignored`, which is
what proves the tests actually ran rather than self-skipping on a missing
`NOSTOS_E2E_PG`.

**This retires the "296 passed / 2 failed" line above.** Nothing in the pg suite
fails as of 2026-09-02.

The `make ci` numbers are read off the `test result:` lines and the recorded
`MAKE_CI_EXIT`, not off a shell exit code — a wrapper reported "exit code 0"
twice in this session for runs that had actually failed or not run at all.

### The 2 pg failures are pre-existing, not from this work

`e2e_pg_snapshot.rs`: `concurrent_writes_during_snapshot_appear_exactly_once`
and `fresh_slot_yields_snapshot_rows_then_live_stream`.

Attribution was measured against a TRUE baseline, not just the pre-fix file.
`git checkout 1a0a792 -- transport.rs` was the first check, but `1a0a792` is
itself this session's HEAD and already carries the earlier fixes (predicate
bound `814b8e2`, WS caps `fe53ec3`) — reproducing there does not prove
"pre-existing". The decisive run reverts **all** of `crates/` to the
pre-session commit `73b5793` (8 files, 510 deletions): both tests still fail.

They also fail 2/2 on re-run, so they are deterministic in the current DB
state, not flaky. Neither test uses `resume_lsn`, so the replay branch this
work touches is never entered.

**ROOT CAUSE FOUND 2026-09-01 (`e27bc2e`). Not a product bug — a cross-suite
test-isolation leak.**

`crates/nostos-client/tests/e2e_pg_apply_throughput.rs:141` runs
`ALTER PUBLICATION nostos_pub ADD TABLE public.bench_apply` and **never removes
it**. The bench leaves ~40,000 rows behind. `nostos_pub` is shared, so every
later test that opens a *fresh* replication slot snapshots those 40k rows too.
Both failing tests collect into a fixed budget — `collect_events(&mut repl, 8,
..)` and `.., 32, ..` — so the budget fills with bench rows before the test's
own row arrives.

The failure text says so plainly once you actually read it rather than the
summary of it: `fresh_slot` reports **"got 8 events"** — *exactly* its budget of
8. `concurrent_writes` reports **"LOST rows ... 135"**. Nothing was lost or
undelivered; the collection window was full of another test's data.

This explains every property that made it look mysterious: deterministic
(40k rows are stably there), reproduces on the pre-session baseline (it is
database state, not code), and unrelated to `resume_lsn` (neither test uses it).

**Confirmed by experiment, not inference:** `ALTER PUBLICATION nostos_pub DROP
TABLE bench_apply;` then re-run → `2 passed; 0 failed` immediately.

The earlier "27 rows across all six published tables" measurement is what sent
the first investigation down a blind alley — it counted six tables. `nostos_pub`
had **eight**; `bench_apply` was the one that mattered and was never in the
`pg-init` fixture to begin with. A count that excludes the pathological case
is worse than no count, because it retires the hypothesis it should have raised.

Fixed by giving the bench a `teardown_bench_table()` that unpublishes and
truncates. It runs on the success path only — a panicking bench still
re-poisons the suite; that ceiling and the one-line manual antidote are named
in a `ponytail:` comment on the helper.

### Two ways a test run lied this session

Both worth knowing, because each reported success while running nothing:

1. A background `cargo test > $DIR/log` where `$DIR` did not exist: the redirect
   failed, and the harness still reported exit 0.
2. `timeout 600 cargo test ...` — `timeout` is GNU coreutils, absent on macOS.
   Exit 127, reported as completed.

An exit code is not a test result. Only a `test result:` line is, and only when
the skip count is also checked — the pg suite self-skips silently without
`NOSTOS_E2E_PG=1`.

A third trap: reverting a file to "the commit before my fix" proves nothing when
that commit is also yours. A baseline has to predate the whole session.

## Operational hazard: the disk

The 238 GiB volume hit 100% mid-session. A Python `open(path, "w")` truncates
before it writes, so the full disk left `crates/nostos-infra/src/transport.rs` at
**0 bytes**; `git checkout` could not restore it either, because git could not
write `.git/index.lock`. Recovered by deleting `target/debug/incremental`, then
restoring from git — nothing was lost, but only because the file was committed.

Every later file write in this session used write-temp-then-`os.replace`, which
is atomic and cannot truncate the original on failure. The volume is still at
96%; `target/` is the bulk and is regenerable.

## Recommended next test, not yet written

The highest-value remaining test is the **tenant-isolation matrix**, modelled on
a well-known sync-rules CVE (CVE-2026-30870): sync rules were silently *not applied* for certain
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

---

## Left deliberately — not silently

Three things were in scope for "fix all the gaps" and were **not** changed. Each
is a decision with real blast radius, written out so it can be approved in a
sentence rather than rediscovered later.

### 1. HS256 tokens without `exp` stay permanent credentials

The one-line change is `required_spec_claims = {"exp"}` on the HS256 verifier,
aligning it with the JWKS path (which already requires `exp`).

Not done, because it is a **breaking auth change, not a hardening tweak**:
Supabase *service-role* tokens carry no `exp`, so this would 401 every one of
them at the next deploy — silently, from the operator's point of view, since
the token itself looks unchanged. ADR-0029 §Decision-4 records the current
behaviour as deliberate. "Fix all the gaps" authorises fixing gaps; it is not
by itself a decision to invalidate credentials that are working in production
today.

If it should ship, the safe shape is a config flag defaulting to today's
behaviour, flipped to required in a major version — say the word and it is a
small change plus test-fixture updates.

### 2. `NOSTOS_SLOT_MAX_LAG` defaults to `0` (WAL-bloat eviction OFF) — RESOLVED

**Decision (ADR-0043, 2026-09-02):** default is now **1 GiB**
(`1073741824`), shipped in `ac78ae5`; `0` = unbounded with a startup warning,
`nostos doctor` `max_slot_wal_keep_size` check, docs and pinning tests in
`be853dd`. Original reasoning kept below for the record.

This — not the ack frame — is the genuine disk-exhaustion exposure on the
Postgres primary. A client that simply never acks holds `restart_lsn` back and
WAL accumulates without bound (ADR-0016).

Not silently picked at audit time, because any non-zero default **disconnects
real users**: the eviction cannot distinguish a malicious idle socket from a
phone on a bad train connection, and the failure mode of guessing too low is
"your app drops sync on the commute". Why 1 GiB is acceptable anyway: eviction
never drops the slot, only the session — the client reconnects and resumes
(op-log replay inside the ADR-0025 window, snapshot-reconcile outside it). One
resync for a client a gigabyte behind is a far smaller blast radius than a full
primary disk for everyone.

Observed on the dev database while investigating: four abandoned slots
(`nostos_slot`, `nostos_slot_arxa_kit`, `atlet_rt_sim_slot`, `atlet_demo_slot`)
each retaining **117–120 MB** of WAL, plus six leftover `e2e_snap_*` slots.
That is the mechanism working exactly as described, on a laptop, with nobody
attacking anything. **Note:** no `NOSTOS_SLOT_MAX_LAG` value fixes those —
there is no server left to evict anyone. Abandoned slots are bounded only by
Postgres `max_slot_wal_keep_size` (ADR-0043 §3), which is why `nostos doctor`
now flags `-1`.

### 3. Findings 2, 3, 4, 6, and 8–11 in "Open — confirmed gaps"

Unchanged this pass. Per-principal connection caps, the unbounded snapshot,
`iss` validation, the JWKS bounded-staleness window, and the rest are real but
each needs a policy call (what limit, what window, whose deploy breaks) rather
than a patch. None is a silent-authorization-bypass of the class fixed above:
findings 6 and 7 were, which is why they were done first.

#### Verification 2026-09-02 — doc claims cross-checked against HEAD `4a5a9fa`

Read-only pass pinned to commit `4a5a9fa`: each finding's claimed fix commit
was located with `git log -S<symbol>`, every cited line was re-read from
`git show 4a5a9fa:<path>` (the working tree of `main.rs` had drifted under
concurrent edits; all `main.rs` lines below are the `4a5a9fa` numbers), and the
covering test named. Rule applied: CLOSED-VERIFIED only where a test that would
fail on regression is named; code-only would be CLOSED-BUT-UNTESTED. Nothing
was run (host CPU-contended). Verdicts:

| # | Finding (one line) | Fix commit | Code at HEAD | Test | Verdict |
|---|---|---|---|---|---|
| 2 | No per-principal connection cap | `4b7386d` | `store.rs:116` `PrincipalCapExceeded`; `session.rs:100` `with_per_principal_cap`; `main.rs:193` `NOSTOS_PER_PRINCIPAL_SESSION_CAP` | `store.rs::per_principal_cap_tests` (3: `one_account_cannot_consume_every_global_slot`, `a_refused_connect_does_not_burn_a_global_slot`, `presence_index_is_not_double_counted_by_the_capped_path`) | CLOSED-VERIFIED |
| 3 | Unbounded snapshot | `4b7386d` | `snapshot_source.rs:123` `limit_clause` (`LIMIT cap+1`), `:128` `reject_if_over_cap`, called on both `snapshot` (`:287`) and `snapshot_stream` (`:360`); `transport.rs:1284,1448` map `TooLarge` to a refusal | `limit_fetches_one_row_past_the_cap_so_a_breach_is_detectable`, `at_or_under_the_cap_is_accepted_and_over_it_is_refused`, `the_default_cap_clears_the_apply_throughput_bench` (`snapshot_source.rs:973-1010`). Transport-level `TooLarge` frame path is untested. | CLOSED-VERIFIED |
| 4 | `nbf`, `aud`, `iss` not validated **on either verifier path** | `ac78ae5` | JWKS path only: `jwks.rs:153` `validate_nbf = true`, `:154-155` opt-in `set_issuer`; `main.rs:143` `NOSTOS_JWT_ISSUERS`. **HS256 path `auth.rs:262` `verify_supabase_hs256` still checks only `exp` — no `nbf`, no `iss`; `NOSTOS_JWT_ISSUERS` is never applied to it.** | None. No test anywhere exercises a future-`nbf` token or a wrong-`iss` token (grep `nbf`/`with_issuers` in tests: zero hits). | **NOT-ACTUALLY-CLOSED** (half: JWKS done, HS256 untouched; both halves untested) |
| 6 | Rotated keys stay valid through an outage | `ac78ae5` | `jwks.rs:82` `DEFAULT_JWKS_MAX_STALE` 30 min, `:96` `with_max_stale`, `:236-247` refuses to serve past ceiling; `main.rs:151` `NOSTOS_JWKS_MAX_STALE_SECS` → `:570` `with_jwks_max_stale` | `a_cache_past_its_staleness_ceiling_stops_serving_during_an_outage` (`jwks.rs:832`) | CLOSED-VERIFIED |
| 8 | No admin-token rate limiting | `b8b3d14` | `admin_auth.rs:84` `failure_delay` (linear, capped), `:115` `check` sleeps on failure only, resets counter on success; `main.rs:469` refuses to start on token < `MIN_ADMIN_TOKEN_LEN` | `failure_delay_escalates_then_stops_at_the_cap`, `check_rejects_missing_and_malformed_headers`, `check_accepts_correct_bearer_token` (`admin_auth.rs:191-227`). Startup short-token bail at `main.rs:469` has no test. | CLOSED-VERIFIED |
| 9 | Cross-tenant existence oracle | `c930997` | `write_back.rs:249` `CROSS_TENANT_REJECTION`; all six `Forbidden(` sites (`:483,632,855,987,1155,1294`) return that one string | `e2e_pg_writeback.rs`: `cross_tenant_upsert_conflict_is_rejected`, `cross_tenant_delete_is_rejected_row_survives`, `cross_tenant_patch_is_rejected_row_unchanged`, `cross_tenant_insert_is_stamped_to_callers_tenant` (pg-gated, `NOSTOS_E2E_PG=1`) | CLOSED-VERIFIED |
| 10 | No `Origin` check on WS upgrade | `b8b3d14` | `transport.rs:450` `origin_allowed` (empty list = off, absent header = native client, exact match, non-UTF-8 refused); `main.rs:425` `NOSTOS_WS_ORIGINS` → `:984` `with_allowed_origins` | `empty_allowlist_is_off_and_admits_everything`, `configured_allowlist_admits_listed_and_refuses_unlisted`, `a_native_client_sending_no_origin_still_connects`, `match_is_exact_not_a_prefix_or_suffix` (`transport.rs:2053-2091`); `main.rs:2549-2569` origin-list parsing | CLOSED-VERIFIED |
| 11 | `Not` over an absent column over-delivers | `0abeee0` | `predicate.rs:256-312` three-valued eval; Unknown survives `Not`, Kleene `And`/`Or`, collapses to no-deliver at the top | `not_of_missing_eq_is_unknown_and_does_not_deliver` (`:766`), `unknown_survives_negation_at_every_depth` (`:794`) | CLOSED-VERIFIED |

**Gap to dispatch — finding 4, HS256 half.** `crates/nostos-infra/src/auth.rs:262`
`verify_supabase_hs256` decodes claims and checks `exp` only. It needs `nbf`
rejection (with the same `JWT_LEEWAY_SECS`) and the `NOSTOS_JWT_ISSUERS`
allowlist applied when non-empty, plus tests on both paths: a future-`nbf`
token rejected, a wrong-`iss` token rejected when the allowlist is set, any
`iss` accepted when it is unset. The close-all plan's own table (row 4) says
"both verifier paths", so this is a doc/code mismatch, not a scoping choice.

> Re-verified 2026-09-02: `ac78ae5` covered the JWKS path only — HS256 ignored
> `nbf`/`iss`, and JWKS accepted a *missing* `iss` even with an allowlist
> (`jsonwebtoken` 10.4 `set_issuer` compares only when the claim is present);
> both closed in `899530e` with tests on both paths (`auth::tests::hs256_*`,
> `jwks::tests::jwks_*`). Finding 4 → CLOSED-VERIFIED.

---

## Operational hazard: a killed test run poisons the next one

A `--test-threads=1` pg run that is interrupted leaves its replication slots
behind. Observed this session: an externally-killed suite left **20 inactive
slots**, including `e2e_stream1_27716`…`e2e_stream6_27716` (the PID is in the
name). The next run then failed three `e2e_pg_sync_streams` tests —
`lazy_stream_snapshot_then_live_delta`,
`stream_on_rules_denied_table_errors_non_fatally`, and
`unsubscribe_stops_flow_and_two_streams_dedup` — all with "rows never arrived".

That looks *exactly* like a fan-out regression, and it landed in the same file
as a fresh change to the stream path, which is the worst possible coincidence.
What ruled the change out before touching anything: those tests run under
`SyncMode::All` and `Toggles` with `scope: None`, so `rules_expr` is `Any` and
`rules_expr.and(bound) == bound` — the new code is provably identical to the old
for them. Dropping the orphaned slots made all five pass.

Antidote, worth running before any pg suite:

```sh
docker exec nostos-postgres psql -U nostos -d nostos -t -c \
  "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
   WHERE NOT active AND slot_name LIKE 'e2e_%';"
```

Two independent shared-state hazards bit this suite in one session
(`bench_apply` in the publication, orphaned slots). Both produce failures that
point at the sync path and mention nothing about state. The general lesson: on
this suite, **verify the database is clean before believing a failure is code.**
