# Push smoke — real-rail FCM doorbell for the Atlet pilot (ADR-0037)

`tool/push_smoke.sh` drives the full real rail, no mocks:

```
atlet on device ── POST /push-tokens (FCM token) ──▶ nostos-server
      │ pauseSync (goes offline: doorbells target offline accounts only)
psql INSERT INTO sessions ──▶ docker PG (logical replication)
      ▼
nostos-server fan-out ── doorbell {table, lsn} ──▶ FCM HTTP v1 ──▶ device
```

Assertions, both sides of the rail:

- **server**: `nostos_push_sent_total` on `GET /metrics` increases after the
  row insert (`nostos_push_failed_total` / `nostos_push_enqueued_total` are
  printed for diagnosis on failure);
- **device**: the integration test (`integration_test/push_smoke_test.dart`)
  receives the data message via `FirebaseMessaging.onMessage` and prints
  `PUSH_SMOKE_RECEIVED table=… lsn=…` (exit 0).

The harness self-skips (exit 0, `SKIP <reason>`) whenever an operator-owned
input is absent — same convention as `NOSTOS_E2E_PG` / `NOSTOS_E2E_FCM`.

## Why the Android emulator is the automated path (design decision)

A headless/automated flow **cannot** receive a real FCM push on an iOS
simulator, and `firebase_messaging` has no macOS implementation at all:

| Target | FCM reality |
|---|---|
| Android emulator | fully real: FCM token, data messages, background isolate |
| iOS simulator | **no real APNs token** — FlutterFire docs: "APNs only works with real devices"; Xcode 14+/iOS 16+ simulators only support *locally*-injected pushes (`xcrun simctl push`), which never transit FCM. `getToken()` fails, FCM sends return `UNREGISTERED` (firebase-ios-sdk#9968) |
| macOS desktop | `firebase_messaging` unsupported (no macOS implementation) |

So the harness's automated leg is the Android emulator (`10.0.2.2` reaches
the host's nostos-server), and the **truly-real iOS leg is a physical device**
with a one-line mode switch (same script, same assertions):

```bash
PUSH_SMOKE_DEVICE=ios \
PUSH_SMOKE_DEVICE_ID=$(flutter devices --machine | … physical id …) \
NOSTOS_SYNC_URL=ws://<mac-LAN-IP>:8080/sync \
  apps/atlet/flutter/tool/push_smoke.sh
```

(`PUSH_SMOKE_DEVICE_ID` comes from `flutter devices`; the server binds
`0.0.0.0` in ios mode so the device can reach it over LAN.)

## What you must provide (nothing here is committed)

### 1. Firebase project (FCM)

1. console.firebase.google.com → create project (Spark tier is enough).
2. Add an **Android app** with package id `internal.atlet.atlet` → download
   `google-services.json` → drop at `apps/atlet/flutter/android/app/google-services.json`.
3. (iOS leg only) Add an **iOS app** with the Runner bundle id → download
   `GoogleService-Info.plist` → drop at `apps/atlet/flutter/ios/Runner/`
   (it is wired into the Xcode project as a bundle resource; if you recreate
   it via the Management API instead of the console, remember
   `configFileContents` returns base64). Everything else is already in the
   repo: `Runner.entitlements` (`aps-environment`),
   `UIBackgroundModes: [remote-notification]` in Info.plist, iOS 15 floor.
   Operator-owned reality checks (all bit live on 2026-08-16):
   - **Paid Apple Developer team required** — free personal teams cannot
     provision `aps-environment`; the build fails on the wildcard profile.
   - Apple may demand the updated **Program License Agreement** be accepted
     before issuing profiles ("PLA Update available" signing error).
   - **APNs auth key (.p8)** must be uploaded in the console under Project
     settings → Cloud Messaging → the Apple app's card (direct URL:
     `/project/<pid>/settings/cloudmessaging/ios:<appId>`). Without it, sends
     to the iOS token fail (1 failed per send) while Android still succeeds.
   - **Debug builds can't launch from the home screen on iOS 14+** (JIT
     restriction) — install a `--profile` build for interactive use; the
     `flutter test` legs are fine (Flutter tooling launches them).
4. Project settings → Service accounts → **Generate new private key** →
   that JSON is the FCM rail credential. Point the env at it (file path or
   raw JSON both work):

   ```bash
   export NOSTOS_FCM_CREDENTIALS_JSON=/path/to/firebase-service-account.json
   ```

Both platform files are gitignored-by-absence (operator-owned, like
`apps/atlet/services/.env`); the Gradle `google-services` plugin only
applies when the json exists, so config-less builds stay green.

### 2. Supabase (auth + JWT verification)

The doorbell only fires for **authenticated, offline** accounts — the smoke
server runs `NOSTOS_SYNC_AUTH=supabase-jwt`, so it must verify the same JWTs
the app obtains from your Supabase project:

```bash
export SUPABASE_URL=https://<PROJECT_REF>.supabase.co
export SUPABASE_ANON_KEY=<anon/publishable key>
export NOSTOS_SUPABASE_JWT_SECRET=<Project Settings → API → JWT secret>
```

The signed-in user defaults to the seeded conformance user
(`flutter@atlet.dev` / `atlet-flutter-2026`, same as the SigninScreen
prefill; create with `apps/atlet/supabase/scripts/create_sdk_users.sh`).
Override with `ATLET_SMOKE_EMAIL` / `ATLET_SMOKE_PASSWORD` dart-defines.

Note: only *auth* goes to hosted Supabase; the replicated data lives in the
local docker PG — the smoke needs no Supabase DB access.

### 3. Local environment

- Docker running (the harness starts `docker/docker-compose.yml` if the PG
  at `localhost:5433` is down);
- a **booted, unlocked Android emulator** (keep the screen on — the
  foregrounded app must receive the data message);
- `flutter` + `cargo` on PATH. First `cargo run -p nostos-server` may build
  for a few minutes; the repo's `target/` cache usually covers it;
- the nostos_flutter native Rust lib builds for Android via its
  `hook/build.dart` cargo step (cargo-ndk; needs `ANDROID_HOME` + NDK).

## Run

```bash
cd /Volumes/developer_ssd/Developer/nostos   # repo root not required, but tidy
apps/atlet/flutter/tool/push_smoke.sh
```

Expected, leg 1 (silent doorbell): three lines — `device ready: user=…`,
`server: nostos_push_sent_total 0 → 1`,
`device: PUSH_SMOKE_RECEIVED table=sessions lsn=…`, then
`PASS  real-rail FCM doorbell: PG row → nostos-server → FCM → device`.

Expected, leg 2 (ecommerce order lifecycle, below): `device checked out:
order=…`, two vendor lines, then
`PASS  order lifecycle: checkout → shipped → delivered pushes`.

Logs: `/tmp/atlet-push-smoke-server.log` (nostos-server),
`/tmp/atlet-push-smoke-app.log` (leg 1 flutter test),
`/tmp/atlet-push-smoke-order.log` (leg 2 flutter test).

## Leg 2 — ecommerce order lifecycle (the SDK reference model)

Leg 2 drives the **real atlet app UI** through its ecommerce feature and
proves visible push notifications over the whole order lifecycle — the
pattern every other nostos SDK (Swift/Kotlin/RN/Capacitor/…) should copy:

```
atlet app (foregrounded test)                      harness plays the vendor
  Shop → product → Add to cart → Cart → Checkout
    → Pay  ── nostos write ──▶ PG `orders` row (status=paid)
  pauseSync (offline; pushes target offline accounts)
                            UPDATE orders SET status='shipped'
                               └─▶ replication ─▶ visible push ─▶ FCM ─▶ device
                            UPDATE orders SET status='delivered'
                               └─▶ same again
```

Server config (the whole "feature"): one `NOSTOS_PUSH_TABLES` entry —

```
orders:action:order_status:Atlet order update:Your order {id} is {status}
```

`{col}` statically interpolates the triggering row (docs/api/push.md), so the
same template renders "Your order 3f2a… is shipped" and "… is delivered"
without any per-status config.

`action` mode (vs plain `visible`) adds a client-registered notification
category (`order_status`): iOS banners carry its action buttons — "Track
order" / "Mark received", lock screen included with the app killed (see
Runner/AppDelegate.swift). Android receives a data-only message and the app
renders the notification locally with its action button (lib/push/
push_pilot.dart `showActionNotification` — system-rendered FCM notifications
cannot carry buttons). Plain `visible` remains the zero-client-code mode.

Two behaviors that trip first-timers, both by design:

- **Presence gate.** Push sends re-check account presence at flush time —
  an online account gets nothing (its WebSocket already carries the data).
  The test therefore re-pauses the engine after every received push, and the
  harness sleeps past the 2s coalescer window between lifecycle steps.
- **Foreground vs background.** While the app is foregrounded, notification
  messages arrive via `FirebaseMessaging.onMessage` and are NOT shown in the
  tray (FlutterFire documented behavior). Backgrounded or killed, the same
  message lands in the system tray. The automated leg asserts the foreground
  path — keep the emulator foregrounded while it runs; backgrounding it makes
  the marker asserts fail even though the tray notifications themselves
  appear. The tray path is the same FCM rail, exercised by any real build of
  the app against the same server config.

## In-app pilot wiring (what the app does with a doorbell)

`--dart-define=ATLET_PUSH_PILOT=true` turns on `lib/push/push_pilot.dart`:
`Firebase.initializeApp()` + background handler at boot; token registration
on every nostos engine start and on `onTokenRefresh`; foreground doorbell →
`resumeSync()`; background doorbell → isolate cold-open of the same SQLite
file (delta applies from the durable LSN checkpoint — the
`nostosDoorbellBackgroundHandler` doc comment names its token-staleness
ceiling). Without the flag the module is inert: no Firebase init, no
registration, builds/analyze/tests unaffected.

## The order banner has nothing to do with FCM

`_wireOrderBanner` in `lib/main.dart` watches `watchOrders()` and posts a
LOCAL notification through the `atlet/notify` MethodChannel whenever a row's
`status` changes under it. No Firebase, no APNs, no server push — which is why
it is the only notification leg that works on an iOS simulator, where
`getToken()` cannot succeed at all (above). It used to sit behind
`ATLET_PUSH_PILOT`, so a default build showed the user nothing when their
order shipped; it is unconditional now.

Three things had to be true before a banner actually appeared, and none of
them announce themselves when false (2026-09-23):

1. **Authorization.** Nothing in a default build ever called
   `requestAuthorization` — the only `requestPermission()` lived inside the
   opt-in pilot. Without the grant iOS drops the notification silently and
   even `xcrun simctl push` refuses with `UNErrorDomain 2003: Source is not
   authorized`. `AppDelegate.didFinishLaunching` asks for it now.
2. **The delegate.** A foreground notification is suppressed unless a
   `UNUserNotificationCenterDelegate` asks for it — and FlutterFire's
   messaging plugin claims that delegate during `GeneratedPluginRegistrant`
   whether or not the pilot is on, so "claim it only if nobody else has it"
   never fired. `ForegroundBanner` now takes the delegate and FORWARDS to the
   previous one, which is what the old "do not steal the delegate" note was
   actually protecting.
3. **A sync that happens at all.** See below.

### The doorbell is dead on this Supabase project

`realtime.messages` is partitioned by `inserted_at` and this project has ZERO
partitions, so every insert fails `23514 no partition of relation "messages"
found for row` — and `realtime.send` swallows that by design. The trigger
fires, the ring is never delivered, and nothing anywhere reports an error.
Creating the partitions needs `supabase_admin` (`42501: permission denied for
schema realtime` from the MCP role), so it is a dashboard/platform action.

`DirectClient::run` now syncs at least once per `SYNC_FLOOR` (60s) regardless,
so a silent doorbell costs latency instead of correctness. Before the floor,
the only sync a device ever did was the one at startup.

### Watching a run

```sh
bash tool/atlet_watch.sh [udid] [bundle-id]
```

Polls the device's own SQLite every 2s and prints one line per CHANGE — cart
count, order status transitions, dead-lettered writes with the server's error,
plus `order banner:` lines lifted from the app log. Reads the live db (WAL
admits concurrent readers) and re-resolves the data-container path every poll:
`simctl install` can hand the app a new container, and a path captured at
startup then points somewhere nothing writes, which looks exactly like "quiet".

Round trip proven 2026-09-23 on the simulator: `update public.orders set
status = 'shipped'` → `ORDER 983979e8… delivered -> shipped` on the device 15s
later → `order banner:` in the log → the OS banner on screen.

## The History tab is the receipt

A banner is gone the moment it is dismissed, so "did the push fire?" used to be
answerable only from a log. Migration 0007 makes every status an order reaches a
row in `public.order_events` (written by a trigger on `public.orders`, synced
with the same `nostos.log_change('user_id','sub')` stamp as everything else), and
the History tab stacks them newest-first.

Round trip proven 2026-09-23:

```sh
# server
update public.orders set status = 'delivered' where id = '983979e8-…';
# device, within SYNC_FLOOR (60s)
sqlite3 "$(xcrun simctl get_app_container "$UDID" internal.atlet.atlet data)/Documents/nostos_direct.sqlite" \
  "select previous_status||'->'||status from order_events order by created_at desc limit 1"
# -> shipped->delivered
```

…and at the same moment the app logs `order banner: 983979e8 shipped -> delivered`,
iOS shows the banner, and History grows a `983979e8 · shipped → delivered` row.
The row is the durable half: it survives a dismissed banner, an app restart, and
a reinstall.

## The tap: notification → the event it came from

Every banner now carries the routing keys in its payload's `data` map (never in
the visible text — `data` is the only half FCM delivers in all three app states,
per Firebase's "receive messages" guide), and a tap lands on the detail view for
exactly that event:

```
{"title": "Atlet order update",
 "body": "Order 983979e8 is shipped",
 "category": "order_status",
 "data": {"nostos_route": "/history/<event-id>",
          "deep_link": "atlet://history/<event-id>",
          "event_id": …, "order_id": …, "status": …,
          "previous_status": …, "occurred_at": …}}
```

Where it lives: `lib/push/order_push.dart` builds it, `lib/main.dart`
(`openHistoryEvent`) is the single destination, and both platforms forward taps
over the same `atlet/notify` channel — iOS sets `content.userInfo` and returns
it from `didReceive`, Android puts the keys in the `PendingIntent` extras. A tap
that arrives before Dart is listening (cold start) is buffered natively and
drained by Dart's one `take_pending_tap` call.

Three ways to trigger it:

```sh
# 1. the real thing: flip a status, wait <=60s, tap the banner
update public.orders set status = 'shipped' where id = '983979e8-…';

# 2. the URL (iOS asks "Open in Atlet?" first — that prompt is simctl's, not ours)
xcrun simctl openurl "$UDID" "atlet://history/<event-id>"

# 3. a real APNs push, payload shaped like the `data` map above
xcrun simctl push "$UDID" internal.atlet.atlet payload.json
```

The app logs `notification tap: /history/<event-id>` on arrival, and the detail
view shows the payload verbatim (copy button) plus every delivery attempt this
session — which is the difference between "no banner was asked for" and "the OS
refused it".

Verified 2026-09-23: banner posted with `route=/history/d0b2b75d-…` in the log.
The tap itself is still a human step on this machine — Xcode 27 ships no
Simulator.app for AppleScript and `idb ui tap` needs the SimulatorKit framework
Xcode 27 no longer installs, so nothing here can press a button on the device.
