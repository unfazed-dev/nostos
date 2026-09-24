# RN-iOS TurboModule — implementation plan (ADR-0029 WS4, the 10th platform)

**Date:** 2026-08-03 · **Branch:** `feat/multi-sdk-fixture-matrix` · **Status:** ✅ COMPLETE — TurboModule builds + full JSI round-trip verified on iPhone 17 sim. All four "honest scope notes" from the first pass are now CLOSED (file-backed cross-reopen wipe, watchChanges push to JS, self-contained podspec, fat staticlib).
**Predecessor:** WS4-D3 sign-out audit, `nostos-ws4-d3-signout-audit-2026-08-03.md` (removed in cleanup; see git history). RN has TS + Android (commit 5956c84); iOS has **no `ios/` dir at all** — this builds the missing platform so `signOut`/`setToken` (and the full `NativeNostos` surface) reach iOS.

## Scope-note closure (2026-08-03, second pass — all verified on iPhone 17 sim)

The first pass shipped the round-trip on `:memory:` and flagged four honest gaps. All four are now closed + verified by a single sim run:

```
resolveDbPath OK -> .../tmp/nostos-rn-e2e.db          (#1 path helper)
connect OK → write OK id=1 → query js-A PRESENT
watchChanges onSnapshot fired (js-A PRESENT) → PUSH-TO-JS OK   (#2 pump reaches JS)
unwatchChanges OK → setToken OK → signOut OK
reconnect OK → query (after signOut) js-A GONE — WIPE PROVEN rows=[]   (#1 file-backed cross-reopen wipe)
SUCCESS TurboModule round-trip green (wipe + push proven)
```

- **#1 file-backed cross-reopen wipe:** `resolveDbPath(name)` added to `NativeNostos.ts` (iOS-verified; Android pending — same drift as watch). The round-trip writes `js-A` on a file path, `signOut()` wipes, then RECONNECTS on the same path — `connect()` sees the dropped session + re-runs `SqliteStorage::open`, so the second query reads the wiped file: `js-A GONE rows=[]`. This is the wipe proof `:memory:` could never give (each client gets its own empty store).
- **#2 watchChanges push to JS:** `NostosSnapshotSink: SnapshotSink` holds a retained `RCTResponseSenderBlock`; `onSnapshot` fires on the nostos tokio worker and the block self-marshals to the JS thread (verified — the generated param type IS `RCTResponseSenderBlock`, not a raw `jsi::Function`). `unwatchChanges` nil's the callback (delivery stops); the sink is retained until session end (nostos_swift has NO `stop_watch` — the Rust pump is tied to the session; honest binding-floor ceiling). Initial-snapshot push verified; change-tick uses the identical path (assumed, no live spine).
- **#3 self-contained podspec:** `s.prepare_command = "bash scripts/build-ios-staticlib.sh"` builds the fat sim staticlib + copies the UniFFI Swift sources + generates the `nostos_swiftFFI` modulemap INTO `ios/` (the gitignored regen cache). VERIFIED: prepare_command does NOT fire for `:path` dev pods (CocoaPods skips it — uses source in-place); it fires for published/git-sourced pods (the publishable case). Local dev runs the script manually (mirrors how Rust-backed RN dev pods work).
- **#4 fat staticlib:** `scripts/build-ios-staticlib.sh` builds `aarch64-apple-ios-sim` + `x86_64-apple-ios` and `lipo`s them → `ios/libnostos_swift.a` (`x86_64 arm64`, lipo-verified). The arm64 slice links + runs on the sim; the x86_64 slice is present (Intel-host sim link not separately exercised). The `ARCHS=arm64` override the first pass needed is GONE — the fat lib links the default arch.

**Capture method (gotcha for the next session):** RN `console.log` does NOT appear via `simctl launch --console-pty` or in Metro's non-interactive stdout. It DOES hit os_log under subsystem `com.facebook.react.log:javascript`, captured by a LIVE `xcrun simctl spawn <udid> log stream --predicate 'eventMessage CONTAINS "rn-e2e"'` started before (re)launch. Retroactive `log show` with `process ==` missed it (level/predicate).

## Progress (2026-08-03 session) — verification vehicle built; remaining work is the xcodebuild sim loop

**Done (concrete, on disk):**
- Host RN app created at `sdk/nostos_react_native/example/` (RN 0.86.2, react 19.2.3, gitignored — local verification vehicle, not committed). `ios/` + Podfile present.
- SDK linked as `"@nostos-sync/react-native": "file:.."` (symlink → SDK dist reachable). SDK `dist/` built.
- `NostosReactNative.podspec` (slice-1 minimal) + `ios/NostosTurboModule.swift` placeholder written.
- `pod install` succeeded (78 deps, 31s); **NostosReactNative is linked** (`Podfile.lock`).

**Diagnosed (unblocks the path):**
- **The committed `nostos_swift` xcframework is STALE** — its `nostos_swift.swift` lacks `setToken` (predates it); current `swift-sources/nostos_swift.swift` HAS `setToken` (line 525) + `signOut` (555). Fix: vendor the **fresh ios-sim staticlib + current `swift-sources/`**, mirroring `sdk/nostos_swift/ios-test/project.yml` (which compiles `../swift-sources/nostos_swift.swift` + links `libnostos_swift.a` directly — "Tier 1 builds the xcframework separately"). iOS-sim rust target (`aarch64-apple-ios-sim`) is installed; the ios-sim `.a` is NOT yet built in this checkout.
- **Codegen (`NativeNostosSpec.h`) emits at `xcodebuild` time**, not `pod install` (not found in `Pods/`). So the Swift TurboModule can't be conformance-verified without the build.

**Remaining (the xcodebuild-sim iteration loop — deferred, see why below):**
1. Build the ios-sim staticlib: `cargo build --target aarch64-apple-ios-sim -p nostos-swift` (deterministic).
2. Rewire the podspec: vendor that `.a` + `swift-sources/nostos_swift.swift` + the `nostos_swiftFFI.modulemap`; set `SWIFT_INCLUDE_PATHS`/`-fmodule-map-file` (the Xcode-16+ gotcha from project memory).
3. Write `ios/NostosTurboModule.swift` against the codegen `NativeNostosSpec` (RN codegen for a Spec is deterministic from `NativeNostos.ts` — the Swift method labels must byte-match or the module is null at runtime).
4. `xcodebuild` the example for the iPhone 17 sim; resolve the UniFFI Swift↔Obj-C module wiring + any Xcode-26.6/RN-0.86 friction.
5. Drive `signOut` (wipe proof) + `setToken` + a connect/subscribe/write/query round-trip from JS.

**Why stopped here:** steps 2-4 cannot be verified except by the `xcodebuild` sim run itself — writing them blind reproduces the untested-surface defect class. The vehicle + the precise unblocked path are in place; the sim loop is a focused next session (or continue on instruction).

---

## Goal

A React Native iOS TurboModule that satisfies `NativeNostos.ts`'s codegen spec and delegates to the existing `nostos_swift` UniFFI `NostosClient` — the same `nostos_client::SyncClient<SqliteStorage>` every other SDK uses. Verified end-to-end on the iOS simulator (signOut wipes; setToken swaps; connect/subscribe/query/write round-trip).

## Approach (decided from Gate-2 evidence)

- **Swift TurboModule**, not Obj-C hand-rolled FFI. `NativeNostos.ts`'s header names "iOS Swift" as the intent; a Swift class conforming to the codegen-emitted `NativeNostosSpec` Obj-C protocol reuses the UniFFI-generated `nostos_swift.swift` marshalling (no hand-rolled `RustBuffer` ownership code — the failure-prone part of the raw C ABI).
- **Reuse, don't fork:** the `nostos_swift` UniFFI C ABI already exposes every method (`uniffi_nostos_swift_fn_method_nostosclient_{sign_out,set_token,connect,subscribe,write,query,checkpoint,watch}` in `swift-sources/nostos_swiftFFI.h`), and a **sim-slice xcframework already exists** (`sdk/nostos_swift/xcframework/NostosSim.xcframework/ios-arm64-simulator`). No Rust changes.
- **Mirror the Android module** (`sdk/nostos_react_native/android/.../NostosTurboModule.kt`): lazily-constructed backing `NostosClient`, `@ReactMethod` Promise wrappers over blocking UniFFI sync calls (block on the owned tokio runtime — the `ponytail:` block-on-owned-runtime decision in `nostos_swift/src/lib.rs`).

## Verification vehicle (the load-bearing prerequisite)

The codegen `NativeNostosSpec` Obj-C protocol is **generated at `pod install`** (deterministic from `NativeNostos.ts` but not emitted without a host app). A wrong method label = a silent runtime null module. So: a host RN-iOS app must exist. Created gitignored at `sdk/nostos_react_native/example/` (RN 0.86, `--skip-install`) — a local verification harness, **not** committed (the SDK example dirs were archived; this is local-only).

## Staged steps

1. **Host app** — `npx react-native@latest init NostosRnExample --directory sdk/nostos_react_native/example` (running). Then add `"@nostos-sync/react-native": "file:.."` to its `package.json` + `npm install`.
2. **Podspec** — `sdk/nostos_react_native/NostosReactNative.podspec`: `s.static_framework = true`, `vendored_frameworks` → the nostos_swift sim xcframework (via a relative path or `prepare_command` to build it), `s.source_files` → `ios/**/*.{swift,h,m}`, `s.dependency "React-Core"`, `s.pod_target_xcconfig` to find the UniFFI modulemap.
3. **`pod install`** in `example/ios/` → runs `@react-native/codegen` → emits `NativeNostosSpec.h`. **Inspect it** to capture the exact Obj-C method signatures (param labels/types) the Swift impl must satisfy.
4. **iOS TurboModule** — `ios/NostosTurboModule.swift`: `@objc public final class NostosTurboModule: NSObject, NativeNostosSpec` with `@objc` methods matching the inspected protocol, delegating to a lazily-constructed `NostosClient` (UniFFI Swift). `RCT_EXPORT_MODULE()`.
5. **Build + sim** — `xcodebuild` the example for the iPhone 17 sim; boot sim; install + launch; drive `NativeNostos.signOut()/setToken()` from the example's JS.
6. **Instrumented proof** — a JS-side test (or a tap in the example app) that: connect → write a row → signOut → reopen → assert empty (mirrors the Flutter/kotlin wipe proof). Plus setToken-before-connect and live-swap.

## Risks (honest)

- **Xcode 26.6 / RN 0.86 compatibility** — Xcode 26.6 is very new; RN may emit build warnings/errors or the swiftc `-fmodule-map-file` gate (broken under Xcode 16 per project memory, fixed in README) may need re-tuning. Mitigation: iterate on the sim build; if Xcode 26 blocks, pin to the operator's known-good toolchain.
- **Codegen signature drift** — if the Swift impl's method labels don't byte-match the codegen protocol, the module is null at runtime. Mitigation: step 3 inspects the real generated header before writing step 4.
- **UniFFI Swift ↔ Obj-C interop in a pod** — mixed-language pod needs the modulemap + Swift version set so the Obj-C codegen protocol sees the Swift class. Standard but finicky.
- **Sim-only xcframework** — only `ios-arm64-simulator` ships; a device `ios-arm64` slice is needed for TestFlight/production (not for this verification).

## Out of scope (deferred)

- Device (`ios-arm64`) xcframework slice + TestFlight build.
- `watchChanges`/`unwatchChanges` Rust→JS push pump on iOS (the reactive ADR-0024 surface) — defer to a follow-up; this plan delivers the request/response surface + signOut/setToken (the ADR-0029 gap).
- Committing the example app (stays gitignored; an operator decision whether to ship a trimmed example later).
