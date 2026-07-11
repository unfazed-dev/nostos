# nostos_flutter

Plug-and-play local-first sync for Flutter, backed by [Nostos](https://nostos.run)
(Postgres logical replication → Rust fan-out server → on-device SQLite,
Apache-2.0 end to end). Rust owns SQLite and the sync loop
(`nostos-client`'s `SyncClient`); this package wraps it with
[flutter_rust_bridge](https://pub.dev/packages/flutter_rust_bridge)'s
native-assets backend, so `flutter pub add nostos_flutter` is the only manual
step — no codegen, no Xcode/Gradle wiring, no client-side schema artifact.

## Quickstart

Run a server (zero setup — synthetic data, no auth, no Postgres):

```bash
cargo run -p nostos-server   # ws://127.0.0.1:8800/sync
```

```dart
import 'package:nostos_flutter/nostos_flutter.dart';

final nostos = await Nostos.connect(url: 'ws://127.0.0.1:8800/sync');
await nostos.subscribe('tasks'); // optional: where: "status = 'open'"

nostos.watch('tasks').listen((rows) {
  // rows: List<Map<String, dynamic>> — the full current row set for
  // 'tasks', re-emitted after every applied change.
});

await nostos.write('tasks', op: 'upsert', pk: '1', payload: {'title': 'buy milk'});
```

`subscribe`/`watch` model **one active subscription per `Nostos` instance** —
this mirrors `nostos-client`'s `SyncClient`, which binds one table at
construction. Calling `subscribe` again replaces the previous subscription.
Need more than one table at once? Use a second `Nostos.connect(...)` instance
for now (ponytail — a future multi-table `nostos-client` session removes this
constraint).

### Supabase

```dart
final session = Supabase.instance.client.auth.currentSession!;
final nostos = await NostosSupabase.connect(
  nostosUrl: 'ws://127.0.0.1:8800/sync', // your `nostos dev` URL
  supabaseUrl: 'https://<project-ref>.supabase.co',
  accessToken: session.accessToken,
);
```

`NostosSupabase.connect` does **not** depend on the `supabase_flutter`
package — pass `accessToken` from whatever auth source you use. It's a thin
wrapper over `Nostos.connect`; `supabaseUrl` is accepted for
forward-compatibility (see ponytail in `lib/src/nostos.dart`) but not yet used
to derive anything — point `nostosUrl` at wherever your `nostos-server`
actually runs. `supabase_flutter`'s session token auto-refreshes; re-call
`connect`/`subscribe` with the new token when `onAuthStateChange` fires
(transparent pass-through of a refreshed token is not yet wired — v1).

## API

- `Nostos.connect({required String url, String? token, String? sqlitePath})`
  → `Future<Nostos>`. Opens the durable local store; no network yet.
  `sqlitePath` defaults to a per-`url` file under the platform's
  application-support directory (via `path_provider`).
- `nostos.subscribe(String table, {String? where})` → `Future<void>`. `where`
  is the safe-SQL-subset predicate the server compiles (ADR-0012), e.g.
  `"status = 'open' AND priority >= 3"`.
- `nostos.watch(String table)` → `Stream<List<Map<String, dynamic>>>`. Emits
  immediately with the durable on-disk snapshot (visible offline), then again
  after every applied change.
- `nostos.write(String table, {required String op, required String pk, Map<String, dynamic>? payload})`
  → `Future<int>` (the local outbox id). `op` is `"upsert"` or `"delete"`.
  Durable the instant it returns — the applied write round-trips back
  through `watch` like any other replicated change (`nostos-client`'s
  ADR-0013 outbox contract).
- `nostos.connectionState` → `Stream<NostosConnectionState>`
  (`connecting` / `connected` / `reconnecting` / `disconnected`).
  Reconnect (with backoff) is automatic; this stream is a UI signal, not
  something you need to drive.

### Wire types

Row values arrive natively typed (ADR-0019): a Postgres `boolean` column is a
Dart `bool`, `int2`/`int4` are `int`, and so on — `payload`/`watch()` rows are
`Map<String, dynamic>` with real JSON types, not the earlier all-string
shape. Two deliberate exceptions, both precision-preserving:

- **`int8`, `oid`, `numeric`, and `money` arrive as `String`, not `num`.**
  `int8` can exceed 2^53 (JS/`dart2js` `int`'s exact-integer ceiling — Flutter
  Web is in scope, so this isn't hypothetical), and `numeric` needs arbitrary
  precision a `double` can't hold. **Never `num.parse`/`double.parse` these
  as if they were pre-parsed numbers** — use `int.parse` for `int8`/`oid`
  (exact in that range) or a `Decimal` type for `numeric`/`money`.
- **`bytea` arrives as a base64 `String`** (Postgres's own hex `\x...` text
  form, re-encoded) — decode with `base64Decode` from `dart:convert`.

Timestamps (`timestamp`, `timestamptz`, `timetz`) arrive as RFC 3339 UTC
strings (`...Z`) — parse with `DateTime.parse`. See ADR-0019 for the full
per-type mapping table and the `int8`-as-string rationale.

## Platforms

| Platform | Status |
|---|---|
| macOS | Verified — `flutter test integration_test/nostos_server_test.dart -d macos` runs a real packaged `.app` against a real `cargo run -p nostos-server` (see that test's header comment) |
| iOS / Android | Build config present (native-assets targets declared, plugin scaffold generated). The Rust glue crate is confirmed to cross-compile clean for both (`cargo ndk -t arm64-v8a build --release` and `cargo build --target aarch64-apple-ios --release`, W6). **Not yet verified**: the native-assets build hook actually firing during a real `flutter build ios`/`flutter build apk` — no device/simulator runner was exercised this pass. `.github/workflows/release.yml`'s flutter-android/flutter-ios jobs build the release artifacts; real end-to-end hook verification happens the first time that workflow runs against a real tag push. |
| Windows / Linux | Fast-follow (per the launch plan) — not in `hook/build.dart`'s `_manifestKey()` yet, so both always take the cargo-build fallback. |
| Web | Punted — Rust owns SQLite via `rusqlite`, not `sqlite3.wasm`; a web build needs a different storage backend entirely, out of scope here. |

## Packaging mechanism

`rust/` is a **standalone** Rust crate (its own empty `[workspace]` table —
detached from the root `nostos` Cargo workspace) that path-deps on
`../../../crates/{nostos-client,nostos-core,nostos-domain}`. `hook/build.dart`
implements the prebuilt-binary pattern proved in
`docs/plans/w4-packaging-fallback.md`'s W0a spike:

1. Reads `hook/prebuilt.json` — a checked-in manifest keyed by target
   (`macos-universal`, `android-arm64-v8a`, `ios-device-arm64`, ... — see
   that file's `_comment` for the full list), not an env var: build-hook
   subprocesses don't inherit the invoking shell's environment, confirmed
   empirically in the spike. The key for the current build is computed from
   `input.config.code.{targetOS,targetArchitecture,iOS.targetSdk}` (the
   native-assets protocol's own target descriptor — not `Platform.isX`,
   which reflects the build-hook-subprocess's *host* OS and gives the wrong
   answer whenever host and target differ, e.g. any iOS cross-compile).
2. If that key has a `url`: downloads it, verifies the sha256 (via
   `package:crypto`, pure Dart — portable to Windows, unlike shelling out to
   `shasum`), registers it as the native code asset. **No Rust toolchain
   required on the consuming machine** in this path.
3. On any failure (or when the key is unset/missing — the current state,
   since no GitHub Release exists yet) falls back to `cargo build --release`
   in `rust/`. **This fallback is the active path today, for every
   platform.** `.github/workflows/release.yml` (W6) publishes real
   per-target artifacts and fills in `hook/prebuilt.json` — see that
   workflow's `update-manifest` job for the exact flow (it opens a PR
   against this file rather than pushing directly, since the values ship
   inside the published pub.dev package and warrant a human look first).

`flutter_rust_bridge` is pinned to an **exact** version
(`2.13.0-beta.5`), not a caret range: `2.12.0` silently lacks
`--integration-backend` entirely (confirmed in the spike), and the
native-assets backend is beta-versioned — an exact pin avoids a confusing
"unknown argument" failure on a contributor's stale global
`flutter_rust_bridge_codegen` install.

`flutter config --enable-native-assets` is required (Flutter 3.44/Dart
3.12-era; native assets are still an opt-in flag) — run it once per machine
before building a consumer app.

## Testing

- `flutter analyze` — clean.
- `flutter test` (this package's `test/`) — unit tests against a fake
  `NostosEngine` (`lib/src/engine.dart`), zero native library involved. This
  is the seam to use for testing your own app's `Nostos` usage without
  spinning up Rust: implement `NostosEngine` and pass it to
  `Nostos.withEngine(...)` (`@visibleForTesting`).
- `example/integration_test/nostos_server_test.dart` — the real end-to-end
  proof: `flutter test integration_test/nostos_server_test.dart -d macos`
  from `example/`, against a genuine `cargo run -p nostos-server` subprocess.
  Requires disabling the macOS App Sandbox for **debug builds only**
  (`example/macos/Runner/DebugProfile.entitlements`) — a sandboxed app
  cannot spawn arbitrary subprocesses; `Release.entitlements` (what you'd
  actually ship) is untouched.
- `cargo test -p nostos-client` — the additive `nostos-client` changes this
  package needed (`rows_for`, `subscribe_changes`, `with_storage`) have their
  own unit + integration tests there, independent of Flutter.

## Known gaps / ponytails

- **Connection state is a heuristic, not an exact signal.** `SyncClient::run_once`
  blocks for an entire session and only returns on error or clean close —
  there's no mid-call hook for "the WS handshake + subscribe succeeded"
  without a further `nostos-client` change (deliberately avoided to keep that
  crate's additive surface minimal). See `rust/src/api/nostos.rs`'s
  `NostosConnectionState` doc for the exact grace-window heuristic used.
- **Non-JSON payloads decode to `{"_raw": "<hex>"}`.** Real Postgres-sourced
  rows are always a JSON object; `cargo run -p nostos-server`'s zero-setup
  `fake` replicator (used by the integration test) emits deterministic
  filler bytes instead — this fallback is what keeps `watch()` from throwing
  on that path.
- **`write()`/`subscribe()` enforce single-table-per-instance** by comparing
  against the last `subscribe`d table and throwing `StateError` on a
  mismatch — see the API section above.

## Releases

Tag-triggered (`git tag vX.Y.Z && git push origin vX.Y.Z`) via
`.github/workflows/release.yml`, which also builds the `nostos`/`nostos-server`
CLI binaries (`packaging/homebrew/nostos.rb` consumes those). One workflow run:

1. Cross-compiles this crate for every published target (macOS universal,
   Android arm64-v8a/armeabi-v7a/x86_64, iOS device + simulator), publishes
   each as a GitHub Release asset, and separately wraps the iOS slices into
   an `.xcframework.zip` (a convenience artifact for manual/non-Flutter iOS
   integration — the native-assets hook itself consumes the raw per-slice
   dylibs, not the xcframework).
2. Opens a PR filling in `hook/prebuilt.json` with the resulting
   URLs/hashes (`update-manifest` job) — reviewed and merged by a human,
   *then* `dart pub publish` is run manually for that version. This ordering
   is required, not just cautious: the manifest ships inside the pub.dev
   package, so it must be correct *before* publish, but the artifact hashes
   only exist *after* the same tag's builds finish.

As of this writing the pipeline is authored and locally validated (target
lists, crate cross-compiles, `actionlint`) but has never run for real — this
repository has no GitHub remote yet. The first real tag push is its first
real test.
