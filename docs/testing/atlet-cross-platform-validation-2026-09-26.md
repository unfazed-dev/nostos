# Atlet cross-platform validation — 2026-09-26

**Status: not signed off.** This is an inventory and reproducible local baseline,
not a claim that every Atlet client or push rail has passed end to end.

| Target | What exists | Evidence from this run | Remaining acceptance |
|---|---|---|---|
| Flutter web | `apps/atlet/flutter`, browser assets, `atlet-push-sw.js` | Release builds passed with push pilot off and on. Headless Chromium loaded the built page and rendered Sign in with no page errors. Four focused Chrome test files passed (13 tests). | Real Supabase sign-in, home/shop data, offline replay, and browser push delivery need test credentials and a live service. The full Chrome test sweep had 74 passes and 27 failures: native `dart:io` temp-directory tests plus one asset-loader timeout. |
| JS/TS Atlet web | Design JSX/HTML and a draft RN+web adapter plan; no runnable Atlet JS/TS app package | The underlying `@nostos-sync/web` SDK passed 9 Playwright tests, with one feature-gated conformance test skipped. Its packed tarball also passed the Node Wasm smoke test after the packaging fix in this branch. | Build the Atlet TS adapter and web app, then run the frozen adapter conformance scorecard against the live project. SDK tests are not an Atlet app test. |
| Push | Flutter mobile FCM harness, Flutter web VAPID subscription code and service worker, web SDK synthetic push tests | Web service worker passed `node --check`; push-enabled Flutter web release build passed. SDK Playwright exercised a synthetic service-worker push, wake, reconnect, and REST registration contract. | Real encrypted Web Push through a push service, foreground/background behavior, click routing, sign-out deregistration, and the mobile FCM rail need operator credentials and a test device/browser. |
| Tauri | `sdk/nostos_tauri` plugin and generic Tauri fixture; no Atlet Tauri application | `SDK_E2E_STRICT=1 ./scripts/sdk-e2e.sh tauri` passed the plugin and fixture slice (122 seconds, zero skipped slices). Both standalone crates also pass `cargo test --locked --no-run` after the lockfile refresh in this branch. | Build an Atlet Tauri shell or adapter before an Atlet-specific desktop/mobile sign-off. The generic fixture cannot prove Atlet UI, auth, offline writes, or push. |

## Reproduce the local baseline

From `apps/atlet/flutter`, with Flutter 3.47.5:

```sh
# Fresh-worktree workaround: this Flutter toolchain's SwiftPM plugin copy
# expects the ignored parent build directory even when building web.
mkdir -p build/ios/SourcePackages build/macos/SourcePackages
fvm flutter build web --release
fvm flutter build web --release --dart-define=ATLET_PUSH_PILOT=true
fvm flutter test --platform chrome test/adapter_conformance_test.dart test/push_pilot_test.dart test/order_push_test.dart test/widget_test.dart
```

The build reports a `flutter_rust_bridge` WebAssembly dry-run incompatibility;
the JavaScript web release build succeeds. `fvm flutter test --platform chrome`
is not a green web gate yet because several VM-only tests use
`Directory.systemTemp`; `test/shop_test.dart` also times out loading a bundled
asset in the Chrome test harness, although the release build contains the file.

From `sdk/nostos_web`:

```sh
npm ci
npm run build:all
cd ../.. && cargo build -p nostos-infra --example e2e_server
cd sdk/nostos_web && npm test && npm run check:pack
```

The browser SDK result is 9 Playwright passes and one conformance skip. The
web-push Playwright test uses a **synthetic** push event: headless Chromium has
no push service, so this does not validate real notification delivery.

From the repo root, `SDK_E2E_STRICT=1 ./scripts/sdk-e2e.sh tauri` exercises the
generic Tauri plugin and IPC fixture. The tracked standalone lockfiles needed
refreshing: both `cargo test --locked --no-run` commands failed before that
refresh and pass after it. These tests still do not launch an Atlet Tauri app.

## Inputs for live sign-off

- Flutter web: `SUPABASE_URL`, `SUPABASE_ANON_KEY`, a disposable Atlet test user,
  and a reachable `NOSTOS_SYNC_URL` passed as local Dart defines.
- Web Push: matching `ATLET_VAPID_PUBLIC_KEY` and server-side
  `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` / `NOSTOS_WEBPUSH_VAPID_SUBJECT`; grant
  notification permission in a real browser on a secure origin.
- Mobile push: the existing `tool/push_smoke.sh` harness plus its FCM service
  account and an enrolled Android or iPhone test device. The iOS Firebase
  config is present locally, but no full push test was run here.
- Atlet JS/TS and Tauri: implement the missing app clients first. The
  `docs/plans/atlet-wave-2-rn-web-shared-adapter.md` plan is still DRAFT and
  records that no Wave 2 task has started.
