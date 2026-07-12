# SDK Parity — Final Three (RN, Capacitor, .NET) → 10/10

**Started:** 2026-07-12. **Owner:** Claude (tech lead). **Bar (operator-approved
"complete all the rest"):** bring Nostos to PowerSync-parity breadth (10/10 platforms).
Each new SDK ships a public API (`connect`/`subscribe`/`query`/`write`) mirroring the
existing 7, **compile-verified + offline smoke**, with **live-E2E vs the shared axum spine
where the device runtime is present, honest SKIP-with-reason where it isn't**. `make sdk-e2e`
extended to all 10.

Research was docs-first (primary sources only), 2026-07-12 — three parallel slices. The
load-bearing verdict per platform is below, each with the primary-source citation that
decides it.

## Capacitor — web-only plugin (SMALL, low-risk)

Capacitor **v8** is current (2026; `8.4.1` latest, `8.0.0` released 2025-12-08). The
webview is a full browser engine — **WASM + WebSocket run unmodified in WKWebView (iOS)
and Android WebView (Chromium)** (caniuse: Safari iOS ✅ 11–26.5, Chrome Android ✅;
PowerSync's own Capacitor SDK is built on top of its Web SDK + `@capacitor-community/sqlite`).

`sdk/nostos_capacitor` = a `registerPlugin` **web-only** plugin re-exporting `@nostos-sync/web`'s
**browser live path** (`NostosSocket` via the `--target web` `pkg-web` build) — **no native
`android/`/`ios/` source for the scaffold.** Storage: in-memory KV matches `@nostos-sync/web`'s
current bar; production swap to `@capacitor-community/sqlite` is later.

- **E2E:** Playwright on the Capacitor **`web` platform** vs the spine (proves PUSH+ECHO
  through the Capacitor shell). SKIP native-webview-on-sim — it would only re-prove
  "WKWebView supports WASM+WS," already conclusively evidenced.
- **Risks:** CSP `wss://` allow on the `capacitor://` (iOS) / `http://localhost` (Android)
  origin; bundle `.wasm` as a fetched static asset.

## .NET — UniFFI-CS over the existing UniFFI surface (MEDIUM, no runtime E2E)

Binding = **UniFFI-CS (Nord fork `NordSecurity/uniffi-bindgen-cs`)**, pinned
`v0.9.2+v0.28.3` (tracks UniFFI 0.28 — the exact version `nostos_swift`/`nostos_kotlin`
already use). This **reuses the existing `#[derive(uniffi::Object)]` / `#[uniffi::export]`
surface verbatim** — one Rust interface, four foreign bindings. `cbindgen`+P/Invoke is the
more mature tool but loses the reuse (parallel hand-marshalled C ABI). PowerSync's .NET SDK
is **pure C# (zero Rust, no DllImport)** → it is *no precedent* for Rust→.NET; Nostos's
thin-FFI-per-SDK bet (ADR-0015) makes UniFFI the consistent answer.

- **Risks (honest, not hidden):** UniFFI-CS is **pre-1.0** (Nord's own README: "young …
  major version currently 0"), 9-month release cadence; `AllowUnsafeBlocks=true` on the
  **C# side only** (Rust `forbid(unsafe_code)` preserved — UniFFI macro glue is the
  ADR-0015 machine-generated exception); 2³¹-byte cap on strings/byte[]/lists; **no
  documented production MAUI deployment** (mechanical TFM compatibility, not
  battle-tested). The version pin (Rust `uniffi = "=0.28.3"` + Nord tag) is load-bearing.
- **E2E: SKIP-with-reason** — `dotnet` is not installed on this macOS host. Deliverable =
  Rust scaffold + cross-compile verify (`aarch64-apple-darwin`, `aarch64-apple-ios`,
  `aarch64-apple-ios-sim`, `aarch64-linux-android`) + **committed generated C# binding
  source for review.** `x86_64-pc-windows-msvc` link fails from macOS (no Windows
  SDK/`lld-link`) → defer Windows to a Windows/dotnet CI runner.

## React Native — Turbo Native Module over UniFFI (LARGE — the long pole; scope change)

**Hermes does NOT ship `global.WebAssembly` in any shipping RN release through 0.84**
(Q1 2026). Primary sources: the RN 0.84 blog (2026-02-11, the release that made Hermes V1
default) has **zero** "WebAssembly"/"WASM" mentions (sandbox-grep verified); Hermes issue
#429 (opened 2020-12-04) is still **OPEN** with no linked PRs — *"it does not look like
Hermes has support for running WASM via a `global.WebAssembly`."* Polyfill/JSC fallbacks
are not production-viable (maintainer: *"JS-only polyfills … bound to be slow"*; JSC
*"not very future-proofed as we're switching to Hermes eventually"*). **So `@nostos-sync/web`'s
WASM core is a dead end for RN.**

`sdk/nostos_react_native` = a **TS facade (mirroring `@nostos-sync/web`)** backed by a **Codegen
Turbo Native Module** that calls the **already-shipped** `nostos_swift` (iOS, UniFFI
staticlib) and `nostos_kotlin` (Android, UniFFI `.so` in an `.aar`) bindings. PowerSync
validates this exact shape (pure-TS sync facade + native JSI SQLite backend via
op-sqlite / react-native-quick-sqlite).

- **Scope flag (Gate 1):** RN is **not** a thin facade — it is a **2-platform native-module
  effort** (multi-hour bootstrap: Rust iOS+Android cross-compile + UniFFI bindgen +
  TurboModule/JSI glue). The Swift/Kotlin SDKs already proved cross-compile + bindgen, so
  only the RN TurboModule layer is genuinely new. `subscribe()` = **poll `query()`**
  (matches nostos_swift/nostos_kotlin; event-emitter push is Phase-2 polish).
- **E2E:** feasible on this box (RN CLI 20.2 + 2 iOS sims + Android emus). Primary =
  **Android Kotlin TurboModule + Android-emu E2E (`10.0.2.2`)**; **iOS Swift TurboModule +
  sim E2E** = fast-follow.
- **ADR-0020** (next free number) will record the RN native-module decision (hard-to-reverse
  + surprising-without-context + real-tradeoff — the facade alternative was killed by Hermes).

## Sequencing

1. **Capacitor + .NET** (parallel, high-confidence, independent) — land + verify first.
2. **RN** (the long pole) — focused effort after the first two are verified; lands
   incrementally: TS facade + offline smoke = MUST; Android TurboModule + emu E2E = SHOULD;
   iOS TurboModule + sim E2E = NICE. Checkpoint if the usage window tightens.

## Outcome — COMPLETE (10/10, 2026-07-12)

All three shipped + independently verified (each agent's green report reproduced):

- **Capacitor** (`631ab1d`) — web-only v8 plugin over `@nostos-sync/web`'s browser path;
  Playwright PUSH+ECHO E2E (re-run: `[cap-e2e] PUSH_OK`/`ECHO_OK`, 1 passed).
- **.NET** (`5658515`) — UniFFI-CS Nord `v0.9.2+v0.28.3` over the nostos-client
  UniFFI surface; fresh host build + 5/5 tests + clippy `-D warnings` clean;
  iOS/iOS-sim/Android cross-compile artifacts confirmed (iOS-sim freshly
  re-cross-compiled); generated `nostos.cs` (1629 lines) committed. C# runtime E2E
  **SKIP** (no dotnet on host).
- **React Native** (`029eba7` Wave A + `e5b796e` Wave B) — Turbo Native Module over
  UniFFI (ADR-0020; Hermes has no WASM); reuses `nostos_kotlin` wholesale (untouched).
  Jest 7/7 + Android emu live PUSH+ECHO E2E re-run: `VERDICT: PUSH_OK=1 ECHO_OK=1
  xml_failures=0`. iOS TurboModule = fast-follow.

**10/10 platform presence; 9/10 live-E2E-verified** (.NET SKIP — no dotnet; RN-iOS
pending). `make sdk-e2e` extended to all 10 (host slices always run; device slices
SKIP-with-reason where no runtime). ADR-0020 records the RN architecture.

## Verification discipline (process lesson, reaffirmed)

Independently compile + test + smoke **every** delegated SDK slice — agents' green
self-reports are unverified until reproduced (the consolidation caught the Kotlin cold-start
flake + Web flush bug this way; the parity push caught Tauri/Swift compile errors the agents
never finished building). Re-run each SDK's E2E myself before marking it 🟢.
