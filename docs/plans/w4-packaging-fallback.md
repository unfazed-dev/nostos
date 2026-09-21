# W4 Packaging — de-risk spike results (W0a)

Date: 2026-07-12. Spike scope: prove the Flutter↔Rust packaging path the real
`nostos_flutter` SDK (W4) will be built on, before building it. Spike dir (not
committed): `/private/tmp/claude-501/.../scratchpad/w0a-spike/` — see layout
at the bottom.

## Verdict: **WORKS**. Use frb's native-assets backend + a checked-in prebuilt manifest.

No fallback to Cargokit is needed. Both required capabilities are proven end
to end on real toolchain versions, including inside a genuinely packaged
macOS `.app` (not just the `flutter test` host harness).

**Platform scope of this evidence: macOS arm64 only.** The spike machine
had `cargo-ndk` and `zig` NOT installed (per the environment brief), so
Android and Linux cross-compiles were never attempted — only the host
`aarch64-apple-darwin` target was built and run. What's proven here is the
*mechanism* (native-assets backend build hook, prebuilt-download-with-
fallback pattern, code-asset registration, real-app loading). Whether the
same hook cross-compiles cleanly for iOS/Android/Linux/Windows is
unverified and remains W6's job (the CI release matrix).

## Environment used

- Flutter 3.44.0 stable, Dart 3.12.0
- `flutter_rust_bridge_codegen` **2.13.0-beta.5** (crates.io, published
  2026-07-11 — the plan's cited `≥2.13.0-beta.2` floor was correct; the
  2.12.0 stable release on this machine did **not** expose
  `--integration-backend` at all, confirmed via `--help`)
- `flutter config --enable-native-assets` (required — see Friction below)
- rustc/cargo 1.95.0, target `aarch64-apple-darwin`

## Step 1 — frb native-assets backend: WORKS

```
cargo install flutter_rust_bridge_codegen --version 2.13.0 --force   # 2.13.0 stable since 2026-09 (was 2.13.0-beta.5)
flutter config --enable-native-assets
flutter create --platforms=macos hello_frb_na
cd hello_frb_na
flutter_rust_bridge_codegen integrate --integration-backend native-assets --skip-fvm-install
# add `rust_add(a: i64, b: i64) -> i64` to rust/src/api/simple.rs
flutter_rust_bridge_codegen generate
```

This scaffolds `rust/` (the crate), `hook/build.dart` (wraps
`FlutterRustBridgeNativeAssetsBuilder(cratePath: 'rust')` from
`flutter_rust_bridge_hooks`), and Dart bindings under `lib/src/rust/`.
`flutter test` on a unit test calling `rustAdd`/`greet` triggers the hook,
which runs `cargo build` under `native_toolchain_rust` and registers the
resulting `.dylib` as a Dart code asset — confirmed via `flutter test -v`
showing `Building native assets for macos_arm64` → cargo compile → `xcrun
lipo`/`codesign` → asset copy.

**Strongest proof — real packaged app, not just `flutter test`:** frb
scaffolds an `integration_test/simple_test.dart` that pumps the real
`MyApp` widget. Running it against an actual device target —

```
flutter test integration_test/simple_test.dart -d macos
```

— builds a genuine `hello_frb_na.app` bundle (`flutter build` internally)
and the test passes, calling into Rust with **zero manual configuration**.
This is the path an end developer actually exercises (`flutter run`
/ `flutter build`), and it needs nothing beyond the standard scaffold.

## Step 2 — Prebuilt-binary mode: WORKS

`FlutterRustBridgeNativeAssetsBuilder` always calls `cargo build`; there is
no built-in "consume a prebuilt artifact" mode. Wrote a custom
`hook/build.dart` (raw `package:hooks` + `package:code_assets`, ~110 lines)
that:

1. Reads `hook/prebuilt.json` (checked-in manifest: `{"url": ..., "sha256":
   ...}` — one per release, not an env var; see Friction below for why).
2. Downloads the artifact via `dart:io HttpClient`, verifies sha256 via
   shelling out to `shasum -a 256` (no extra pub dependency needed). This is
   unix-only (macOS/Linux); a real W4 hook targeting Windows would need
   `package:crypto`'s `sha256.convert()` instead — not a concern for this
   spike (Windows is v1-punted per the plan) but worth fixing before W6's
   Windows leg.
3. Registers it with
   `output.assets.code.add(CodeAsset(package: input.packageName, name: ...,
   linkMode: DynamicLoadingBundled(), file: ...), routing: ToAppBundle())`
   — the exact pattern `native_toolchain_rust`'s own `build_runner.dart`
   uses.
4. On any failure (HTTP error, hash mismatch) falls back to `cargo build
   --release` in `rust/` and copies that artifact instead.

Verified four scenarios (local `python3 -m http.server` serving a real
cdylib, all via `flutter test integration_test/ -d macos` — real app
bundle, no env var):

| Scenario | Crate buildable? | URL valid? | Result |
|---|---|---|---|
| Normal | yes | yes | downloads, cargo **not** invoked |
| Rust-less machine (simulated) | **sabotaged** (invalid syntax appended) | yes | still passes — proves cargo genuinely skipped, not just untried |
| Offline / bad URL | yes | 404 | falls back to cargo, still passes |
| Tampered artifact | yes | yes, wrong sha256 | mismatch detected, rejected, falls back to cargo, still passes |

This is the mode that matters for W4 shipping: end-dev machines without
Rust installed get a downloaded binary; CI/release machines with Rust get
a correctness fallback.

## Chosen W4 approach

**frb native-assets backend, with a custom prebuilt-binary `build.dart`**
modeled on the spike's hook (download → sha256-verify → register code
asset → fall back to `cargo build`). Do **not** use Cargokit — it's
upstream-archived (per the plan's research) and the native-assets path is
proven working on the exact toolchain the plan already committed to.

## Friction notes (against the 5-minute-quickstart promise)

1. **`flutter config --enable-native-assets` was set before any scaffolding
   in this spike**, and it's a global (not per-project) config flag. We did
   not test the negative case (whether `integrate`/`generate`/`test` fail
   without it on 3.44), so this note records what was *done*, not a
   confirmed hard requirement. Given the plan's own research already flags
   native-assets as experimental-flag territory, treat it as likely-required
   until disproven, and either have `nostos init` shell out to `flutter
   config --enable-native-assets` automatically or call it out as step 0 in
   the SDK install docs — cheap insurance either way.
2. **`flutter_rust_bridge_codegen` on `2.12.0` (the version this machine
   had preinstalled from a normal `cargo install`) silently lacks
   `--integration-backend` entirely** — no error, just no flag. The W4
   `pubspec.yaml`/README must pin `flutter_rust_bridge: ^2.13.0-beta.5` (or
   later stable once one ships) explicitly; do not let a dev's stale global
   `flutter_rust_bridge_codegen` install produce a confusing "unknown
   argument" failure.
3. **`flutter test` (plain unit-test harness, not `integration_test`) does
   not resolve the native-assets output directory** — frb's generated
   `RustLib.init()` still uses the legacy manual-dlopen loader
   (`loadExternalLibraryRaw` in `flutter_rust_bridge`'s `_io.dart`), which
   guesses `rust/target/release/lib$stem.dylib` or a `.framework` bundle
   path — neither matches where native-assets actually writes
   (`build/native_assets/macos/`). Workaround:
   `FRB_DART_LOAD_EXTERNAL_LIBRARY_NATIVE_LIB_DIR=<project>/build/native_assets/macos/`.
   **This only matters for our own test suite** (W4's CI unit tests calling
   into Rust directly) — confirmed the real app path (`flutter run`/
   `flutter build`/`integration_test` on a device) needs no such
   workaround. Document the env var for W4's own test setup; it is not
   end-developer-facing.
4. **Build-hook subprocesses do not inherit the invoking shell's
   environment variables** (verified empirically — `Platform.environment`
   was empty in the hook for vars exported in the parent shell). This
   pushed the prebuilt-binary design toward a checked-in
   `hook/prebuilt.json` manifest rather than env-var configuration, which
   is arguably the more correct design anyway (one URL+hash pinned per
   release, not environment-dependent).

## Fallback path (documented per plan requirement, unused)

If native-assets had blocked: frb's default integration backend is
**Cargokit** (`--integration-backend cargokit`, the default when the flag
is omitted). Cargokit's `precompiled_binaries` mechanism
(`cargokit/precompiled_binaries.md` upstream) supports the same
URL+hash-pinned prebuilt-artifact model via a `cargokit.yaml`
`precompiled_binaries:` block pointing at a signed URL, with the same
fallback-to-local-cargo-build behavior when unset/unreachable. It generates
per-platform Xcode/Gradle/CMake build-phase scaffolding instead of a Dart
build hook. Since it's noted upstream-archived (2026-03) and native-assets
is proven working here, this path is not pursued further — recorded only
to satisfy the plan's "document even if unused" requirement.

## Spike directory layout (scratch, not committed)

```
w0a-spike/
├── hello_frb_na/          # Step 1: frb native-assets backend, cargo-built
│   ├── rust/               #   crate with greet() + rust_add()
│   ├── hook/build.dart     #   generated FlutterRustBridgeNativeAssetsBuilder
│   ├── integration_test/simple_test.dart  # real-app proof
│   └── test/widget_test.dart              # host-harness proof (needs env var)
├── hello_frb_prebuilt/    # Step 2: custom prebuilt-binary hook
│   ├── hook/build.dart     #   download+verify+register+fallback (~110 lines)
│   ├── hook/prebuilt.json  #   {"url": ..., "sha256": ...} manifest
│   └── (same rust/, integration_test/, test/ as above)
└── prebuilt-server/        # `python3 -m http.server 8899`, serves the cdylib
    ├── librust_lib_hello_frb_na.dylib
    └── librust_lib_hello_frb_na.dylib.sha256
```
