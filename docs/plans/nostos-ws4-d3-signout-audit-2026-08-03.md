# WS4-D3 sign-out audit — 9 SDK verification (ADR-0029)

**Date:** 2026-08-03 · **Branch:** `feat/multi-sdk-fixture-matrix` (36 commits ahead of `main`)
**Method:** 5-agent read-only reasoning swarm (UniFFI / WASM / native families + React Native + fresh-context core re-verify) on glm-5.2, then adversarial verification of every consequential finding by the lead.

## TL;DR

WS4-D3 ported `signOut` + `setToken` to 8 of 9 SDKs. The audit confirms the **foundation is sound** but finds **one real P0**: **Flutter has no `signOut` at all** — `close()` drops the session without wiping, so rows/checkpoint/epoch/outbox/dead-letter survive in the durable SQLite file (cross-user leak on a shared device). ADR-0029 Decision 3 excluded Flutter on a *setToken-only* rationale that never covered signOut. One other agent-flagged "defect" (tauri) **dissolved under adversarial check**; the WASM "race" was **downgraded to a non-leak ordering nit**. Two ADR-0029 decisions remain open and operator-owned (#2, #4); the RN-iOS TurboModule is an M-sized gap, not the "mechanical" task the README implies.

## Foundation — CONFIRMED sound (fresh-context re-verify)

`SyncClient::clear_local_state` (`crates/nostos-client/src/client.rs:608`) takes one engine lock and atomically runs `Storage::clear` then `Outbox::clear`:
- `SqliteStorage::clear` (`sqlite.rs:793`) — ONE transaction: `DELETE cairn_data` → checkpoint→0 (`UPDATE cairn_meta`) → epoch→0 (`INSERT OR REPLACE`) → `DELETE cairn_outbox` (covers pending + dead-letter; `dlq` is a flag on the same table).
- `Outbox::clear` (`sqlite.rs:1070`) — redundant belt-and-suspenders (outbox already wiped inside `Storage::clear`'s tx).
- `InMemoryStorage::clear` (`in_memory.rs:184`) — resets rows + checkpoint; epoch is a no-op trait default (correct for the in-memory model).
- No half-clear window: single `Arc<Mutex<ApplyEngine>>` held across both clears inside one `spawn_blocking` (no await interleave). A crash mid-clear rolls back the whole wipe.
- **Tests:** `sqlite.rs:2039/2063/2087`, `in_memory.rs:614` assert all four wipes. **Soft spot:** the seam test `client.rs:1566` asserts only rows + pending (not checkpoint/epoch) — covered at the storage layer, not the seam. Optional hardening: add `checkpoint()==0` + `epoch()==0` asserts there.

The binding contract (`client.rs:601-607`): **`signOut()` must abort/quiesce its run loop FIRST, then call `clear_local_state`** — clearing under a live apply/flush loop races (post-clear frame re-populates storage; post-clear flush re-queues the outbox).

## Per-SDK verdict

| SDK | signOut | setToken | test | verdict |
|---|---|---|---|---|
| **kotlin** | ✅ `lib.rs:604` quiesce→wipe→drop→clear-token (awaited) | ✅ `lib.rs:564` live-swap + cache | ✅ `:923` file-wipe, `:982` no-op, `:998` set_token | clean |
| **swift** | ✅ `lib.rs:624` | ✅ `lib.rs:583` | ✅ `:969/:1029/:1048` | clean |
| **flutter** | ❌ **MISSING** — `nostos.rs:641 close()` is `*guard=None`, no wipe | ✅ `nostos.rs:564` | ❌ no `test/` | **P0 — leak** |
| **node** | ✅ `lib.rs:511` abort+await run+watch→clear | ✅ `lib.rs:485` cache+forward | ✅ `:755` (set_token test: none) | clean |
| **dotnet** | ✅ `lib.rs:591` | ✅ `lib.rs:638` | ✅ `:947/:1017/:1044` (best) | clean |
| **tauri** | ✅ `lib.rs:320` | ✅ `lib.rs:364` (see note) | ✅ `:785` | clean (P1 dissolved) |
| **web** | ⚠️ LOW `worker.js:244` clear-then-close | ✅ `worker.js:270` reconnect | ❌ none | non-leak nit |
| **capacitor** | ⚠️ LOW `web.ts:399` clear-then-close | ✅ `web.ts:381` cache-only | signOut ✅ `push-echo.spec.cjs:364`; setToken ❌ | non-leak nit |
| **react-native** | ✅ TS+Android (delegates to kotlin `sign_out` via UniFFI) | ✅ live-swap + store | ⚠️ JS test mocks the wipe; `NostosTurboModuleTest.kt` has **zero** signOut coverage | clean code, test debt; **iOS gap** |

## P0 — Flutter signOut — ✅ RESOLVED 2026-08-03 (this session)

**Shipped & verified:** `NostosHandle::sign_out` (`sdk/nostos_flutter/rust/src/api/nostos.rs`) mirrors the kotlin/swift abort→quiesce→`clear_local_state`→drop→clear-token ordering (the explicit `abort()+await()` before the wipe is essential — `Session::drop` only `abort()`s). Surfaced through `NostosEngine`/`RustNostosEngine` (`engine.dart`), `Nostos.signOut` (`nostos.dart`), and `NostosDatabase.signOut` (`nostos_database.dart`, also cancels the Supabase auth listener so a refresh can't `setToken` a wiped engine). frb regenerated (`lib/src/rust/api/nostos.dart:168 Future<void> signOut()`). ADR-0029 Decision 3 amended. **Proof:** `sdk/nostos_flutter/test/signout_test.dart` — 2/2 green under `flutter test` (file-backed reopen sees no prior-principal row; idempotent no-op). Rust: `cargo check`/`clippy -D warnings`/`fmt` clean; Dart `flutter analyze` clean.

(The original finding text is preserved below for the record.)

## P0 — Flutter has no signOut (real cross-user leak) — original finding

**Evidence (lead-verified):** grep across `sdk/nostos_flutter/` (excl. `frb_generated`) for `signOut|sign_out|clear_local_state|wipe|logout` → zero API matches. The rust api (`rust/src/api/nostos.rs`) exposes `connect/subscribe/watch/write/query/set_token/disconnect/resume/close` — **no `sign_out`**. `close()` body:
```rust
pub async fn close(&self) {
    let mut guard = self.session.lock().await;
    *guard = None; // Drop aborts run_task + all watch pumps.
}
```
Its own doc says "a subsequent `subscribe()` … reopens a fresh session against the same durable store" — i.e. rows/checkpoint/epoch/outbox persist. On a shared device the next principal sees the prior user's data. This is exactly the leak ADR-0029 was written to prevent.

**Root cause:** ADR-0029 Decision 3 — "Expose setToken+signOut in the 8 non-Flutter bindings" — justified the exclusion with "set_token already exists in … Flutter." That rationale covers **setToken only**, not signOut. The exclusion is unsound on the signOut half and must be reopened.

**Ready plan (tooling confirmed: `flutter_rust_bridge_codegen` + `dart`/`flutter` on PATH; frb config at `sdk/nostos_flutter/flutter_rust_bridge.yaml`):**
1. Add `pub async fn sign_out(&self) -> Result<(), String>` to `rust/src/api/nostos.rs`, mirroring kotlin `lib.rs:600-642`: lock session → `guard.take()` → abort+await `run_task` → abort+await each `watch_tasks` → `session.client.clear_local_state().await` → (session drops) → clear `self.token` (`RwLock<Option<String>>`, the seed field). Use `disconnect` (`nostos.rs:574`) as the partial template; add watch-task quiesce + wipe + token clear.
2. `flutter_rust_bridge_codegen generate` → regenerates Dart `signOut` binding.
3. Amend ADR-0029 Decision 3: Flutter is no longer excluded; record that `set_token`'s pre-existence never covered signOut.
4. **Test (the part that needs a decision):** kotlin's pure-Rust wipe test (`lib.rs:923`) works because kotlin's `connect()` creates the session. Flutter's session is created by `subscribe()`, which is bound to an frb `StreamSink` — so a pure-Rust test can't easily populate a session. Options: (a) first Dart integration test in `test/` doing connect→subscribe→write→signOut→reopen→assert-empty (correct, but new harness); (b) a `#[cfg(test)]` seam that constructs a `Session` directly (weaker, tests the wipe path without the frb stream). ADR-0029 calls this "the test that matters" — option (a) is the honest choice.

## Dissolved / downgraded findings (adversarial check)

- **tauri `set_token` "can't seed pre-connect" — NOT a defect.** Tauri's `connect()` (`lib.rs:209`) takes `token` as a **parameter** (→ `SyncClientConfig`), with no cached `self.token` field. The multi-user flow there is `signOut() → connect(url, token_b, db_path)`; `setToken`-before-connect is meaningless. Erroring pre-connect is correct-by-design. node/dotnet/kotlin/swift *also* take token at connect — their `set_token` cache+forward is defensive live-swap, not a required pre-connect seeding. No fix.
- **WASM (web + capacitor) "clear-then-close race" — NOT a leak; LOW.** The wasm apply pump does run in Rust independent of the JS `onChange` detach (`NostosSocket` owns `Rc<SocketInner>`, `transport::on_message` applies directly). BUT the engine is **in-memory** (`lib.rs:213` "survives the apply loop but NOT a page reload"; ADR-0017 addendum **rejected** SQLite-WASM durability), and after signOut `sock=null` orphans the engine while reconnect creates a **fresh** `NostosSocket.connect` (`worker.js:292`). Post-clear re-applied rows land on a GC'd orphan — invisible to the next principal. The ADR-0029 quiesce-before-clear ordering is technically violated but the consequence it guards against cannot occur. Optional 2-line hardening: `offChange()` before `clearLocalState()` to also close the benign stale-snapshot push window in `[clear, offChange)`.

## Test debt (surface, not auto-fixed)

- RN: `__tests__/signout.test.ts` mocks `NativeNostos` (facade wiring only — the wipe is invisible to it); `NostosTurboModuleTest.kt` has zero signOut/setToken coverage. The Rust crate's own tests cover the logic but never the RN delegation seam.
- web: no signOut/setToken test. capacitor: no setToken test. tauri: no setToken test.
- `client.rs:1566` seam test narrow (above).

## RN-iOS TurboModule — M-sized scope (operator decision)

- No `ios/` directory exists; iOS has zero native code (Android has a full Kotlin TurboModule over UniFFI 0.28 + `nostos_kotlin` cdylib).
- **Recommended:** reuse `sdk/nostos_swift` UniFFI bindings (`sign_out`/`set_token` already parallel-implemented at `lib.rs:624/583`; xcframework exists at `sdk/nostos_swift/xcframework/NostosSim.xcframework/`, **simulator slice only**). Needs: `ios/NativeNostos.mm` shim + `NostosReactNative.podspec` (vendored_frameworks) + `ios` in `package.json files`. `codegenConfig` lives in `src/NativeNostos.ts` (RN 0.79+ convention) — not a blocker.
- **Hidden M-work:** `sdk/nostos_swift/swift/Package.swift` self-documents that generated sources + modulemap are "NOT wired into this SPM target yet — `swift build` here will NOT resolve NostosClient." Upstream SPM/binary-target wiring is required before RN-iOS can consume it cleanly. Plus an `ios-arm64` device slice for production (not for verification).
- **Toolchain:** Xcode + `uniffi-bindgen-cli 0.28` + `pod install` + `@react-native/codegen`; a **booted iOS simulator is required** to verify the quiesce-then-clear contract end-to-end (compile-only gate proves only "it builds"). Defer is defensible — the contract is already correctly enforced in shared Rust.

## Open ADR-0029 decisions (operator-owned)

- **§Decision-2 — per-principal outbox retention:** currently `Outbox::clear` discards ALL pending writes on sign-out (correct for cross-user isolation; loses the outgoing principal's unsynced offline work). The ratified policy would tag each outbox row with a principal id and refuse-on-mismatch instead of deleting, layered outbox-internally (no trait change). Today marked `ponytail:` pending ratification at `sqlite.rs:1071`.
- **§Decision-4 — server `exp` enforcement: ✅ IMPLEMENTED 2026-08-03.** All 9 SDKs have `setToken`, so the gate is met. HS256 auth now enforces `exp` (`nostos-infra/src/auth.rs` + `nostos-cloud/src/auth.rs`: `SupabaseClaims` gained optional `exp`; no-`exp` = never expires, present+past rejected with 60s leeway). JWKS/RS256 already enforced it. Tests: `hs256_expired_token_rejected`, `hs256_future_exp_token_accepted`; `make ci` green. **Out of scope (future hardening):** dropping an already-open socket mid-flight on expiry (auth runs once at WS upgrade; `setToken`+reconnect handles refresh).

## Recommended priority

1. **Flutter `signOut` (P0)** — close the leak; small feature (rust + frb regen + ADR amendment + Dart test). Needs greenlight on the test-harness choice (a vs b above).
2. **RN-iOS TurboModule** — M; reuse nostos_swift; gate on the SPM-wiring hidden work + a sim.
3. **Ratify §Decision-2** (per-principal outbox) — operator decision; then implement.
4. **§Decision-4 exp enforcement** — after Flutter signOut + RN-iOS close the setToken-everywhere gate.
5. Test-debt backfill + optional `client.rs:1566` hardening — batch after the above.

Gated on operator go for #1's implementation (and the a/b test choice).
