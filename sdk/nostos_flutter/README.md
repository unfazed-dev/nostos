# nostos_flutter

Plug-and-play local-first sync for Flutter, backed by [Nostos](https://github.com/unfazed-dev/nostos)
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

// NostosDatabase is the entry point. It fetches the server schema for you, so
// `SELECT * FROM tasks` works immediately — no hand-written Schema.
final db = await NostosDatabase.connect(
  url: 'ws://127.0.0.1:8800/sync',
  sqlitePath: '$dir/cairn.db', // e.g. from path_provider; any writable path works
);
await db.subscribe('tasks'); // optional: where: "status = 'open'"

db.watch('SELECT * FROM tasks ORDER BY _pk').listen((rows) {
  // rows: List<Map<String, dynamic>>, re-emitted after every applied change.
});

await db.write(table: 'tasks', op: 'upsert', pk: '1', payload: {'title': 'buy milk'});
```

`db.watch` takes SQL; `db.collection<T>(…)` gives you a typed handle with
`watch` / `count` / `upsert` / `patch` / `delete`
([ADR-0024](../../docs/adr/0024-client-reactive-facade-and-query-primitive.md)).
Reads run against one SQLite **VIEW** per synced table, projected from the
server schema ([ADR-0028](../../docs/adr/0028-client-read-views-over-opaque-payload.md)),
which is why `execute` is **read-only** — route writes through `write` or a
`Collection`.

`tasks` above is a real queryable name: the read surface is one SQLite VIEW per
synced table, named after the table (a `public.` prefix is stripped), with the
replication key exposed as `_pk`.

> **`Nostos` vs `NostosDatabase`.** `Nostos` is the low-level engine handle and is
> still exported as an escape hatch, but `NostosDatabase` is the supported path and
> the only one documented here. This README taught `Nostos.connect` until
> 2026-07-30 while `USAGE.md` taught `NostosDatabase` — if you followed an older
> copy of this file, `NostosDatabase.connect` is the closest drop-in. One
> difference to note: `Nostos.connect`'s `sqlitePath` was optional (defaulting to a
> per-URL file via `path_provider`); `NostosDatabase.connect` **requires** it, so
> pass a path explicitly or use `NostosDatabase.open`, which derives it from
> `sqliteDir` + the config's filename.

**Multiple tables share one socket.** `subscribe(table)` is the single-table
convenience; `subscribeTables` takes a list and multiplexes them over the same
`/sync` connection (D1 / [ADR-0022](../../docs/adr/0022-flutter-multitable-sync-and-pause-resume.md)),
each with its own optional predicate:

```dart
await nostos.subscribeTables([
  NostosTableSub(name: 'tasks', whereSql: "status = 'open'"),
  NostosTableSub(name: 'projects'),
]);

nostos.watch('tasks').listen((rows) { /* ... */ });
nostos.watch('projects').listen((rows) { /* ... */ });
```

One *subscription set* is active per `Nostos` instance: calling `subscribe` or
`subscribeTables` again **replaces** the previous set (tearing down its
background connection and watch pumps). `watch(table)` throws a `StateError` if
`table` is not in the active set — so subscribe first.

> Corrected 2026-07-30: this section previously said one *table* per instance and
> advised opening a second `Nostos.connect(...)` for a second table. That was
> stale — it predates multi-table subscription, and following it opens a
> redundant socket.

### Supabase

```dart
// Supabase.initialize(...) must already have run and a user must be signed in —
// this factory reads Supabase.instance's current session itself. It throws a
// StateError naming the fix if there is no live session.
final db = await NostosDatabase.supabase(
  nostosUrl: 'ws://127.0.0.1:8800/sync', // your `nostos dev` URL
  sqlitePath: '$dir/cairn.db', // e.g. from path_provider; any writable path works
);
```

Config-driven alternative, if you keep `assets/nostos.json` + `nostos.g.dart`
(`nostos pull && nostos gen`) — this is what `example/` uses:

```dart
final db = await NostosDatabase.open(
  config: config,          // NostosConfig, incl. an optional supabase block
  schema: nostosSchema,     // generated; omit to fetch from the server
  sqliteDir: dir.path,
);
```

> ### Token refresh is handled for you
>
> `NostosDatabase.supabase` subscribes to `onAuthStateChange` and forwards rotated
> tokens into the sync client, so a session that refreshes mid-flight keeps
> syncing. `close()` cancels that subscription.
>
> **This was a real defect until 2026-07-30:** the token was captured once at
> connect, the reconnect loop re-sent it forever, and the server enforces `exp` —
> so sync stopped about an hour after sign-in and never recovered, with nothing
> visible but a flapping connection state.
>
> If you manage auth yourself, call `nostos.setToken(newToken)` on rotation. Use
> that, **not** a re-connect: swapping the token in place leaves your `watch`
> streams open, whereas building a fresh handle ends every one of them.

`NostosDatabase.supabase` does **not** depend on the `supabase_flutter`
package — pass `accessToken` from whatever auth source you use. `supabaseUrl` is
accepted for forward-compatibility (see ponytail in `lib/src/nostos.dart`) but not
yet used to derive anything — point `nostosUrl` at wherever your `nostos-server`
actually runs.

## Push notifications (ADR-0037)

Push is a **doorbell**, not a data channel: a data-only `{table, lsn}` hint
wakes the app, and the sync connection — never the push rail — delivers the
rows, resuming from the durable LSN checkpoint. Register the OS push token
with the server and re-wake sync on arrival:

```dart
// 1. Whenever the OS hands you a (possibly rotated) token:
//    - Android/FCM:  FirebaseMessaging.instance.getToken() / onTokenRefresh
//    - iOS/APNs:     didRegisterForRemoteNotificationsWithDeviceToken
await db.registerPushToken('fcm', fcmToken); // or 'apns' / 'webpush'
// signOut() deregisters session-registered tokens automatically (best-effort).
```

**Wake entry** — what your FCM/APNs background handler does. The FRB engine
handle cannot cross isolates, so a background isolate re-initializes its own
`NostosDatabase` and rides the normal resume path (task 5.1's non-destructive
`disconnect()`/`resume()` siblings; this SDK's `pauseSync()`/`resumeSync()`):

```dart
// FCM background isolate (Android) — same shape for an APNs content-available
// handler on iOS. Static doc code: bring your own firebase_messaging plugin.
@pragma('vm:entry-point')
Future<void> firebaseMessagingBackgroundHandler(RemoteMessage message) async {
  // If a paused db handle exists in this isolate: just db.resumeSync().
  // From a killed app there is no handle — cold-open the durable store; the
  // engine resumes from its on-disk LSN checkpoint and applies only the delta.
  final db = await NostosDatabase.connect(
    url: 'ws://127.0.0.1:8800/sync',
    sqlitePath: '$dir/cairn.db', // SAME file as the foreground session
  );
  await db.subscribe('tasks'); // re-declare tables; delta applies, not a resync
  // Keep the isolate alive until the delta lands, then close():
  await db.waitForFirstSync();
  await db.close();
}
```

Nothing here adds a plugin dependency — `registerPushToken` is a plain REST
call (`POST /push-tokens`, same JWT as `/sync`); `firebase_messaging` /
APNs registration stay the app's own. Errors surface as
`NostosPushTokenException` (non-`204` reply). Tokens registered before a
process restart are not auto-deregistered on a later sign-out — the server
prunes them when the rail reports the token dead (410 / `UNREGISTERED`).

## API

**Entry points** (`NostosDatabase` — use these):

- `NostosDatabase.connect({required String url, String? token, NostosSchema? schema, required String sqlitePath})`
  → `Future<NostosDatabase>`. Fetches `GET {base}/schema` unless `schema` is passed.
- `NostosDatabase.supabase({required String nostosUrl, NostosSchema? schema, required String sqlitePath})`
  → reads the live session from `Supabase.instance`; throws `StateError` if none.
- `NostosDatabase.open({required NostosConfig config, NostosSchema? schema, required String sqliteDir})`
  → config/codegen-driven; what `example/` uses.
- `NostosDatabase.local({required String sqliteDir, required NostosSchema schema, Set<String>? orSetTables, Set<String>? counterTables})`
  → local-only: full read/write/watch/CRDT surface on on-device SQLite with the
  sync loop paused before it can dial. No server, no token, no push rail
  (those calls throw `StateError`). Reopen the same file with a real URL to sync.
- Then: `subscribe` / `subscribeTables`, `watch(sql)` / `getAll(sql)`,
  `write(table:, op:, pk:, payload:)`, `collection<T>(…)`, `syncStatus`,
  `disconnect` / `resume` / `close`, `registerPushToken(platform, token)` /
  `deregisterPushToken(token)` (ADR-0037). `execute(sql)` is a **read-only**
  alias of `getAll` — see its dartdoc before reaching for it.

**Low-level handle** (escape hatch; `NostosDatabase` wraps this):

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
  The harness compiles the server FIRST under its own generous budget
  (default 15min; raise via `NOSTOS_IT_COMPILE_BUDGET_MIN` on slow storage —
  a cold fingerprint walk + compile of the server graph can take tens of
  minutes on an external SSD, observed 2026-08-27), then boots it and
  requires `/healthz` within 2 minutes. That boot window now measures boot
  only: failing it means a genuine server failure, not a missed compile.
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

## Versioning & compatibility

Two pins define a working consumer setup (verified 2026-08-27 on Flutter
3.44.9 / Dart 3.12: `pub get` resolves both exactly, `flutter analyze` clean,
72 unit tests green with the real native-assets dylib loaded through the
build hook — the FFI pairing itself is exercised, not just resolved):

| pin | value | why exact |
|---|---|---|
| `nostos_flutter` | `0.2.0-dev.1` (path dep until first pub.dev publish) | pre-release head; see CHANGELOG |
| `flutter_rust_bridge` | `2.13.0-beta.5` | beta-versioned native-assets backend; 2.12.0 silently lacks `--integration-backend` (see pubspec comment + docs/plans/w4-packaging-fallback.md) |

**Tag truth:** the repo's `v0.1.0` git tag (2026-07-05) predates the entire
`sdk/` tree — no git tag has ever carried this package. "Track nostos's
`v0.1.0` tag" is therefore not a thing a Flutter consumer can do; pin the
commit (or the `0.2.0-dev.1` head) instead. The first flutter-carrying tag is
cut when the release pipeline runs for real — an operator call.

## Reference app (atlet) & adapter conformance

[`apps/atlet`](../apps/atlet) in this repo is the reference consumer, kept
green in CI (analyze + full `flutter test`, 107 tests):

- `lib/adapters/nostos_adapter.dart` — side-by-side with
  `powersync_adapter.dart` behind the frozen `SyncAdapter` contract v1.1
  (`apps/atlet/spec/adapter.md`): init/addSession/deleteSession/
  watchSessions/watchProducts/connected/setConnected/signOut.
- `lib/push/push_pilot.dart` — the push reference: FCM mobile + VAPID web,
  gated by `--dart-define=ATLET_PUSH_PILOT=true`, with the
  `nostosDoorbellBackgroundHandler` background-isolate wake pattern (an FCM
  data-only `{table, lsn}` doorbell → cold-open the durable store →
  `waitForFirstSync` — see the Push section above).

**Conformance caveat (read before adopting):** the adapter-conformance pilot
scorecard (`apps/atlet/spec/conformance-flutter.md`) records that **only
checklist item 5** (no engine-type leakage) genuinely passed — items 1–4 were
never executed against a live backend for either adapter. A live-backend
conformance pass is the **first task** when a client project actually adopts
this SDK; the spec's prerequisites section names the operator-provisioned
environment that entails.

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
