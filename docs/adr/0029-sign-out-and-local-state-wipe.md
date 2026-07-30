# ADR-0029: Sign-out and local-state wipe

**Status:** Proposed — the §Decision-2 pending-write disposition is a tech-lead recommendation
awaiting operator ratification. The trait surface (`clear()`) is stable regardless of that choice.
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
2. **Pending writes on sign-out (RECOMMENDATION, pending ratification):** persist per-principal and
   refuse to replay across a principal change. The alternatives both fail — keep-and-replay =
   cross-user write attribution + tenant violation; discard = silent loss of offline work.
   Per-principal retention is the only option that neither loses data nor misattributes it. It is an
   outbox-internal policy (a principal tag + refuse-on-mismatch), NOT a new trait method — invisible
   to WS1's Worker protocol.
3. **Expose `setToken` + `signOut`** in the 8 non-Flutter bindings. `set_token` already exists in
   `nostos-client` (`client.rs:351`) and Flutter (`nostos.dart:336`); every other binding takes an
   opaque token with no swap primitive.
4. **Server `exp` enforcement lands AFTER #3, never before.** Enforcing `exp` while only Flutter has
   a refresh trigger disconnects the other 8 SDKs ~1h after login with no recovery — a silent problem
   turned into a loud 8-platform outage.

## Consequences

- Required-method parity on the storage traits moves **7 → 9**. This is the surface WS1's Worker
  `postMessage` protocol must marshal: a `clear` command fronting both clears **atomically** (one
  Worker transaction — half a clear is a leak).
- `signOut` becomes a first-class SDK lifecycle step, not "just close the socket."

## The test that matters

User A signs in, writes, signs out; user B signs in on the same device. **B must not see A's rows,
and A's unsynced writes must not be attributed to B.** Plus a resume assertion: B receives a
snapshot, not an empty database (proving the checkpoint was cleared to 0).
