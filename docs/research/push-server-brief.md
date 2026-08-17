# Push-notification server research brief (APNs / FCM)

Facts only; all sources fetched 2026-08-17. Crate stats from the crates.io API.

## APNs token-based (.p8) — what a server must implement
- Holds: APNs Auth Key private key (`.p8` file), 10-character **Key ID**, **Team ID**.
- Provider token = JWT: header `alg: ES256` (ECDSA P-256 + SHA-256), `kid` = Key ID; payload `iss` = Team ID, `iat` = seconds since Epoch (UTC), no more than one hour from current time.
- Each provider token has a **one-hour validity interval**; refresh **no more often than once every 20 minutes**.
- Transport: **HTTP/2 + TLS 1.2+** to `api.push.apple.com:443` (production) or `api.development.push.apple.com:443` (development); port 2197 usable on either.
- Per-request `apns-topic` header = app bundle ID.
- https://developer.apple.com/documentation/usernotifications/establishing-a-token-based-connection-to-apns
- https://developer.apple.com/documentation/usernotifications/sending-notification-requests-to-apns
- https://developer.apple.com/documentation/usernotifications/establishing-a-connection-to-apns

## FCM HTTP v1 — what a server must implement
- Holds: service-account JSON (or Application Default Credentials); mints short-lived OAuth2 access tokens with scope `https://www.googleapis.com/auth/firebase.messaging`.
- Token lifetime: Google returns `expires_in: 3600`; JWT `exp` max 1 h after `iat` → hourly refresh loop.
- Send: `POST https://fcm.googleapis.com/v1/projects/PROJECT_ID/messages:send` with `Authorization: Bearer <token>`.
- Legacy HTTP + XMPP APIs deprecated 2023-06-20; **shutdown began 2024-07-22** — legacy server keys are dead.
- https://firebase.google.com/docs/cloud-messaging/auth-server
- https://firebase.google.com/docs/cloud-messaging/send/v1-api
- https://firebase.google.com/docs/cloud-messaging/migrate-v1
- https://developers.google.com/identity/protocols/oauth2/service-account

## Competitor behavior
- **PowerSync**: no push-notification delivery documented; SDK reference documents sync only ("push real-time updates" = sync wording, not device push). https://docs.powersync.com/client-sdks/reference/react-native-and-expo
- **ElectricSQL**: no push-notification docs found. https://electric-sql.com/docs
- **Supabase**: no first-party push; official guide is a DIY Edge Functions example calling FCM/APNs via Expo from user code. https://supabase.com/docs/guides/functions/examples/push-notifications
- **Firebase**: FCM itself is the first-party push layer; app servers still call the v1 API themselves. https://firebase.google.com/docs/cloud-messaging

## Rust crate landscape (crates.io API)
| crate | scope | newest | last release | total dl | dl (90d) | repo |
|---|---|---|---|---|---|---|
| a2 | APNs, async HTTP/2 | 0.10.0 | 2024-05-05 | 245,207 | 58,012 | github.com/walletconnect/a2 |
| fcm | FCM | 0.9.2 | 2022-07-27 | 171,339 | 7,050 | github.com/panicbit/fcm-rust (no release since 2022; pre-dates legacy-API shutdown) |
| google-fcm1 | FCM v1 (generated) | 7.0.0+20251212 | 2026-01-01 | 211,900 | 45,069 | github.com/Byron/google-apis-rs (actively regenerated) |
