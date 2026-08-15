# @nostos-sync/capacitor

A [Capacitor v8](https://capacitorjs.com/) plugin that runs
[`@nostos-sync/web`][web]'s live browser sync path inside the app webview, plus a
**beta** native push bridge (ADR-0037): OS push-token registration
(APNs/FCM) and a foreground-push event.

> **Beta (push):** the push surface — `registerPushToken` /
> `deregisterPushToken` / `registerForPushNotifications` / the
> `pushToken`+`foregroundPush` events, and the native `ios/`+`android/`
> source — is experimental (plan task 6.3, ADR-0033 discipline). The sync
> surface below it is the previously shipped v0.1 web path, unchanged.

[web]: ../nostos_web

## What it does

- Loads the wasm-pack `--target web` build of `nostos-ffi-wasm` (`@nostos-sync/web`'s
  `pkg-web/nostos_ffi_wasm.js`) inside the webview.
- Drives the wasm `NostosSocket` — a live WebSocket sync session built on
  `web_sys::WebSocket` + `Window::localStorage`. The socket subscribes to a
  server-side table, applies inbound frames through the pure apply-engine,
  ACKs per committed batch, and persists the resume LSN to localStorage so a
  reload resumes from where it left off.
- Exposes the PowerSync-shaped API in the Capacitor plugin convention:
  `connect`, `subscribe`, `write`, `query`, `watch`, `checkpoint`,
  `rowCount`, `close`, `configure`, `reconnect`.
- **Beta push bridge**: the native sides obtain the OS push token (iOS APNs)
  and emit foreground-push events; the JS side registers the token with the
  nostos server's REST surface.

This mirrors the wiring proven by
[`sdk/nostos_web/e2e/browser_live.spec.cjs`](../nostos_web/e2e/browser_live.spec.cjs),
which drives the same two-direction PUSH + ECHO round-trip against the shared
SDK E2E spine (`crates/nostos-infra/examples/e2e_server.rs`).

## Install

```bash
npm install @nostos-sync/capacitor
```

`@capacitor/core` v8 is a peer dependency. `@nostos-sync/web` is a regular
dependency (linked via `file:../nostos_web` inside this monorepo; on publish it
becomes a normal version constraint).

## Usage

```ts
import { Nostos } from "@nostos-sync/capacitor";

// Point at the wasm glue asset. In a bundled Capacitor app, this is the
// bundled asset URL (e.g. "assets/nostos_ffi_wasm.js"). The default
// "/pkg-web/nostos_ffi_wasm.js" suits dev servers that serve the asset at
// that path.
await Nostos.configure({ wasmUrl: "assets/nostos_ffi_wasm.js" });

// Open the live WS sync session and subscribe to "tasks".
const { rowCount, checkpoint } = await Nostos.connect({
  url: "wss://sync.example.com/sync",
  token: "<auth-token>",
  table: "tasks",
  whereSql: "priority > 5",
});

// Server pushes flow into the wasm engine. Read them back:
const { rows } = await Nostos.query({ table: "tasks" });

// Client writes are echoed back through the server's fan-out so the writer
// sees its own write on the same socket.
await Nostos.write({
  table: "tasks",
  op: "upsert",
  pk: "t1",
  payload: { title: "hello", status: "open", priority: 5 },
  clientWriteId: "w1",
});

await Nostos.close();
```

## Reactive `watch()`

`watch()` pushes the app a fresh row-set whenever the table changes — initial
snapshot first, then deltas — built on the change stream, not polling.

```ts
const sub = await Nostos.watch({ table: "tasks" }, ({ kind, rows }) => {
  // kind === "initial" on the first emit, "delta" after each inbound commit.
  render(rows);
});
// …later
sub.unsubscribe(); // idempotent
```

**Ceiling (honest):** the `delta` path needs a JS-facing change-callback seam on
the wasm `NostosSocket` (Rust `on_change`, landing as JS `onChange` or similar)
that does not yet exist in the shipped `pkg-web` build — today the socket's only
callbacks are its internal WS handlers. The web-SDK port has the seam in flight.
Until it lands, `watch()` delivers the **initial snapshot only** — the same bar
`@nostos-sync/web`'s own `watch()` sets. The delta hook probes the candidate seam names
and activates with no API change the moment the seam ships. This is not polling.

## Push notifications (beta — ADR-0037)

Doorbell semantics: push is a **hint**, sync is the transport. Registering a
token only tells the server where to knock; the data always arrives over the
sync connection, which resumes from the durable LSN checkpoint. Row data
never transits Apple/Google servers.

### Register / deregister (JS, all platforms)

```ts
import { Nostos, NostosWeb } from "@nostos-sync/capacitor";

// The REST pair rides the SAME server + JWT the sync session uses (derived
// from the connect() url; the token passed to connect()/setToken()). There
// is no second credential source, and the server stamps tenant/account
// itself — the SDK never attests identity fields.
await Nostos.registerPushToken("fcm", "<os-push-token>");   // platform: "apns" | "fcm" | "webpush"
await Nostos.deregisterPushToken("<os-push-token>");

// On iOS/Android, call these on the NostosWeb instance you sync with — the
// REST half lives in the webview (see "Why no native sync source?" below):
const nostos = new NostosWeb();
await nostos.connect({ url: "wss://sync.example.com/sync", token, table: "tasks" });
await nostos.registerPushToken("apns", apnsToken);

// signOut() deregisters every session-registered token automatically
// (ADR-0037 §3 — a leaked registration would push the previous principal's
// data to the next user).
```

Wire contract (pinned by `test/push_rest.spec.cjs`, same pins as the Flutter
and Node SDKs): `POST /push-tokens` with `{"platform":…,"token":…}` and
`Authorization: Bearer <sync-jwt>` → `204`; `DELETE /push-tokens/{token}`
(percent-encoded) with the same auth → `204`.

### Getting the OS push token (native bridge)

```ts
import { Nostos } from "@nostos-sync/capacitor";

await Nostos.addListener("pushToken", ({ platform, token }) => {
  // iOS: platform is "apns", token is the hex APNs device token.
  nostos.registerPushToken(platform, token);
});
await Nostos.addListener("pushTokenError", ({ message }) => log(message));

// iOS: registers with APNs (no permission prompt — silent pushes need only
// registration). The token arrives via the "pushToken" event above.
await Nostos.registerForPushNotifications();

// Android: registerForPushNotifications() is unimplemented by design —
// obtain the FCM token app-side (see below) and call
// registerPushToken("fcm", token) yourself.
```

### Foreground bridge → wake

A push that arrives while the app is foregrounded is surfaced as a JS event
(the OS presentation is suppressed on iOS — a foregrounded live socket has
already applied the data, so the handler typically just reconnects or
re-reads):

```ts
await Nostos.addListener("foregroundPush", async ({ payload }) => {
  // The wake path: close + re-open the session; the wasm socket resumes
  // from the durable localStorage checkpoint, so nothing is lost.
  // (ponytail: watch() listeners are dropped by close() — re-register.)
  await nostos.reconnect();
});
```

On Android the event fires only if your app forwards it (Firebase stays
app-side): call `NostosPlugin.emitForegroundPush(dataMap)` from your
`FirebaseMessagingService.onMessageReceived`.

### What stays app-side (explicit)

The plugin vendors no Firebase/Apple push SDKs beyond the bridge skeleton:

- **iOS**: the `aps-environment` entitlement + Push Notifications capability;
  the APNs key/team config on the server; visible-notification authorization
  (`UNUserNotificationCenter.requestAuthorization`) if the app shows alerts;
  the app-id provisioning. The plugin only registers and forwards.
- **Android**: the full Firebase setup (`google-services.json`,
  `firebase-messaging` dependency, `FirebaseMessagingService` + its manifest
  entry, `POST_NOTIFICATIONS` runtime permission for visible notifications on
  Android 13+). Obtain the FCM token in your service and register it from JS.
- **Web**: no OS push in the webview — web push is the Service Worker work
  (plan task 6.2); `registerForPushNotifications()` rejects there.

### Honest limitations (beta)

- **No silent-background wake.** A Capacitor app follows the native OS rules:
  iOS silent pushes are budgeted/opportunistic and discarded when force-quit;
  Android defers under Doze. The wake path is: the OS surfaces the
  notification (or budget-permits the silent push) → the app foregrounds or
  reconnects → `reconnect()`/`connect()` resumes from the checkpoint. Push is
  a nudge; sync reconciles (ADR-0037 §2).
- The native sides are skeleton-shaped and were not compiled in CI (no
  Capacitor app project exists in-repo): the Swift side is syntax-checked
  only, the Kotlin side is reviewed-by-construction against the Capacitor
  plugin template. Both follow the @capacitor/push-notifications wiring.
- Registered tokens are tracked in memory for sign-out deregistration only —
  a webview reload forgets them; the server-side rails prune stale rows
  (APNs 410 / FCM `UNREGISTERED`).
- On iOS the plugin claims `UNUserNotificationCenter.delegate`; a second push
  plugin claiming it (e.g. @capacitor/push-notifications) will conflict.

## Storage (ceiling + upgrade path)

Today the wasm engine holds applied rows in an in-memory KV — the same bar
`@nostos-sync/web` sets. Only the checkpoint (LSN) survives a reload; a reconnect
replays from that LSN. **Production storage arrives with
`@capacitor-community/sqlite`** (the upgrade path tracked in ADR-0017): the
wasm `Storage` trait will be implemented against the SQLite plugin so rows
persist across launches. Until then, treat this plugin as a live-replication
proof, not a durable store.

## Why no native sync source?

Capacitor's iOS WKWebView origin is `capacitor://localhost` and Android's is
`http://localhost`; both serve a full browser engine. WASM instantiation and
`WebSocket` work identically to a desktop browser, so reusing the existing
`@nostos-sync/web` browser path is strictly simpler than a native bridge that just
bounces sync calls over it. The bridge exists for things the browser engine
cannot do — push registration and foreground notification delivery are
exactly that, which is what the (beta) native sides implement and all they
implement.

## Verify

Unit tests (pure node, mocked fetch — pin the REST wire shape):

```bash
cd sdk/nostos_capacitor
npm install
npm run build         # tsc → dist/
npm run test:unit     # node --test test/push_rest.spec.cjs
```

The example app under `example-app/` has a Playwright E2E
(`example-app/e2e/push-echo.spec.cjs`) that spawns the SDK E2E spine, opens
the example page in a headless browser, and proves PUSH + ECHO. Run:

```bash
cd example-app
npm install           # playwright + capacitor-core already hoisted by parent
npx playwright test --config=playwright.config.cjs
```

Success = the run prints `[cap-e2e] PUSH_OK`, `[cap-e2e] ECHO_OK`, and
`[cap-e2e] WATCH_OK` and exits 0. The spine binary must exist at
`target/debug/examples/e2e_server` (build it with
`cargo build -p nostos-infra --examples`).

## License

Apache-2.0, end to end.
