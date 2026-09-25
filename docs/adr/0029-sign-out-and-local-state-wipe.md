# ADR-0029: Sign-out and local-state wipe

**Status:** Accepted. §Decision-1/3/4 shipped 2026-08-03. **§Decision-2 RESOLVED 2026-08-05:
ratified "full-wipe is the v1 cross-principal isolation policy"** — the per-principal outbox
retention recommended below is **deferred**. Rationale: (a) the current full-wipe (`Storage::clear`
wipes `nostos_data` + checkpoint + outbox in one transaction) *is* the cross-principal isolation —
provably no leak; (b) the marker-specified "outbox-internal principal tag" is **inert on the
sign-out path** because `Storage::clear` already wipes the outbox in that same transaction, so it
would ship as unobservable dead code; (c) making per-principal retention *meaningful* requires
decoupling the outbox wipe from `Storage::clear` (changing the deliberate one-transaction
no-half-clear crash-safety guarantee) + principal column/tagging + a client-side principal flow
(nostos-client currently holds a token, not a principal) — a real design change that trades the
provable isolation for a data-preservation benefit on the rare multi-user-device path, where a fresh
snapshot is cheap and correct anyway. Per-principal retention remains a future enhancement wanting
its own ADR. The trait surface (`clear()`) is stable regardless.
**Date:** 2026-07-31. **References:** the [multi-SDK pomodoro fixture matrix
plan](../plans/multi-sdk-pomodoro-fixture-matrix.md); ADR-0025 (defaulted methods degrade),
ADR-0027 (dead-letter), ADR-0013 (server-authoritative write-back).

## Context

No SDK clears local SQLite on sign-out — **Flutter included**. `Nostos.close()` only closes the
engine and the state stream; its docstring (`sdk/nostos_flutter/lib/src/nostos.dart:314`) says
explicitly "Does not delete the local SQLite file". The server never checks JWT `exp`
(`crates/nostos-infra/src/auth.rs:74-78`, deliberate Phase-0 scope: "GoTrue mints short-lived tokens
and the gateway handles expiry"); authentication runs once at WebSocket upgrade with no re-check, so
an open socket outlives its token. No test anywhere exercises a real Supabase sign-in. A
multi-user pomodoro on one device is exactly the shape that leaks: user A signs out, user B signs in,
B sees A's rows and A's unsynced writes replay under B's token.

## Decision

1. **Add `clear()` to `Storage` and `Outbox`** (`nostos-core`, sync, WASM-clean — no tokio):
   - `Storage::clear()` wipes rows AND resets the checkpoint to 0 AND the epoch. A stale checkpoint
     makes the next principal resume past the snapshot and see an empty DB permanently — the
     resume-without-snapshot unsoundness class.
   - `Outbox::clear()` wipes pending writes AND the dead-letter queue (ADR-0027).
   - **Both are REQUIRED, not defaulted.** A no-op default would be a silent cross-user leak — the
     same "defaults degrade" trap as ADR-0025's other defaulted methods.
2. **Pending writes on sign-out (RECOMMENDATION, deferred 2026-08-05):** persist per-principal and
   refuse to replay across a principal change. The alternatives both fail — keep-and-replay =
   cross-user write attribution + tenant violation; discard = silent loss of offline work.
   Per-principal retention is the only option that neither loses data nor misattributes it. It is an
   outbox-internal policy (a principal tag + refuse-on-mismatch), NOT a new trait method — invisible
   to WS1's Worker protocol.

   **Resolution (2026-08-05):** DEFERRED — see the Status line. v1 ships the "discard" interim
   (full-wipe) as the ratified cross-principal isolation. The recommendation above stays on record
   as the design for a future ADR that decouples the outbox wipe + adds principal tagging/flow.
3. **Expose `setToken` + `signOut`** in the 8 non-Flutter bindings. `set_token` already exists in
   `nostos-client` (`client.rs:351`) and Flutter (`nostos.dart:336`); every other binding takes an
   opaque token with no swap primitive.

   **Amendment (2026-08-03):** the "non-Flutter" scope was unsound on the `signOut` half — the
   exclusion rationale (`set_token` already existed in Flutter) covered token-swap only, never the
   local-state wipe. `Nostos.close()` / `NostosDatabase.close()` do NOT wipe (their docstrings say so),
   so Flutter leaked across a principal switch exactly as the other 8 bindings did before D3.
   Corrected: Flutter now exposes `signOut` too — `NostosHandle::sign_out`
   (`sdk/nostos_flutter/rust/src/api/nostos.rs`) mirrors the kotlin/swift abort→quiesce→
   `clear_local_state`→drop→clear-token ordering, surfaced through `NostosEngine` / `Nostos` /
   `NostosDatabase.signOut()`. Proven by `sdk/nostos_flutter/test/signout_test.dart` (a file-backed
   reopen sees no prior-principal row). All 9 SDK bindings now wipe on sign-out.
4. **Server `exp` enforcement lands AFTER #3, never before.** Enforcing `exp` while only Flutter has
   a refresh trigger disconnects the other 8 SDKs ~1h after login with no recovery — a silent problem
   turned into a loud 8-platform outage.

   **Implemented (2026-08-03):** all 9 SDK bindings now expose `setToken`, so the gate is met and the
   HS256 auth path enforces `exp` (the JWKS/RS256 path already did, via `jsonwebtoken`). `SupabaseClaims`
   gains an optional `exp` (`nostos-infra/src/auth.rs`, `nostos-cloud/src/auth.rs`); a token with no `exp`
   never expires (JWT convention — preserves Phase-0 behavior), a present+past `exp` is rejected at auth
   with a 60s skew leeway. **Scope note:** enforcement is at (re)connect; an already-open socket is ALSO
   dropped mid-flight when its token expires — `commit 969ec36` arms a one-shot `exp` deadline in the
   `/sync` writer `select!` (close code `4401 "nostos: token expired"`), alg-agnostic via `token_exp()`, so
   the live socket is torn down at expiry and the SDK's `setToken` + reconnect re-establishes with the
   refreshed token (test `auth_sync.rs: live_socket_is_closed_after_token_exp`). The "future hardening"
   caveat that previously appeared here is superseded by `969ec36`.

## Consequences

- Required-method parity on the storage traits moves **7 → 9**. This is the surface WS1's Worker
  `postMessage` protocol must marshal: a `clear` command fronting both clears **atomically** (one
  Worker transaction — half a clear is a leak).
- `signOut` becomes a first-class SDK lifecycle step, not "just close the socket."

## The test that matters

User A signs in, writes, signs out; user B signs in on the same device. **B must not see A's rows,
and A's unsynced writes must not be attributed to B.** Plus a resume assertion: B receives a
snapshot, not an empty database (proving the checkpoint was cleared to 0).
