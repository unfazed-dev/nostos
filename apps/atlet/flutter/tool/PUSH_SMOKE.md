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
   `GoogleService-Info.plist` → drop at `apps/atlet/flutter/ios/Runner/`.
   Enable Push Notifications + Background Modes (remote-notification) in
   Xcode signing; upload an APNs auth key under Project settings → Cloud
   Messaging → APNs. `ios/Runner/Info.plist` already carries
   `UIBackgroundModes: [remote-notification]`.
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
- the nostos_flutter **native Rust lib must build for Android** (its
  `hook/build.dart` cargo step). At the time of writing that hook fails on
  this host (`cargo build did not produce …/rust/target/release/
  libnostos_flutter_rust.so` — sdk-side, tracked separately); the smoke
  cannot run until it builds.

## Run

```bash
cd /Volumes/developer_ssd/Developer/nostos   # repo root not required, but tidy
apps/atlet/flutter/tool/push_smoke.sh
```

Expected: three lines — `device ready: user=…`, `server: cairn_push_sent_total 0 → 1`,
`device: PUSH_SMOKE_RECEIVED table=sessions lsn=…`, then
`PASS  real-rail FCM doorbell: PG row → nostos-server → FCM → device`.

Logs: `/tmp/atlet-push-smoke-server.log` (nostos-server),
`/tmp/atlet-push-smoke-app.log` (flutter test).

## In-app pilot wiring (what the app does with a doorbell)

`--dart-define=ATLET_PUSH_PILOT=1` turns on `lib/push/push_pilot.dart`:
`Firebase.initializeApp()` + background handler at boot; token registration
on every nostos engine start and on `onTokenRefresh`; foreground doorbell →
`resumeSync()`; background doorbell → isolate cold-open of the same SQLite
file (delta applies from the durable LSN checkpoint — the
`nostosDoorbellBackgroundHandler` doc comment names its token-staleness
ceiling). Without the flag the module is inert: no Firebase init, no
registration, builds/analyze/tests unaffected.
