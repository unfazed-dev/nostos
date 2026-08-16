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

- **server**: `cairn_push_sent_total` on `GET /metrics` increases after the
  row insert (`cairn_push_failed_total` / `cairn_push_enqueued_total` are
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
`server: cairn_push_sent_total 0 → 1`,
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
