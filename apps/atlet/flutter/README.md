# Atlet Flutter

Atlet is a visual training and shop app for exercising Nostos with a real
hosted database. It has a customer experience for sessions, products, cart,
checkout, and order history, plus an admin experience for catalog, orders, and
users. Nostos owns the local SQLite cache and durable outbox; cloud Auth is used
for identity. Direct sync is the default.

## Run with Appwrite Cloud

From this directory:

```sh
mkdir -p build/ios/SourcePackages build/macos/SourcePackages
fvm flutter pub get
fvm flutter run -d macos \
  --dart-define=ATLET_PROVIDER=appwrite \
  --dart-define=APPWRITE_ENDPOINT=https://fra.cloud.appwrite.io/v1 \
  --dart-define=APPWRITE_PROJECT_ID=6ab741900038c74d1086
```

Sign in with an Appwrite Auth account in the ADS `nostos` project. The admin
account must belong to the `atlet_admins` team. The app obtains a short-lived
user JWT and calls the `atlet_sync` Rust Function. Its API key stays in the
Function; do not pass one to Flutter. The checked-in
[Appwrite setup](../README.md#appwrite-cloud-setup) explains migrations and
the Rust cloud runners.

The macOS UI has been tested against the hosted project with one admin and two
customers. Its Appwrite web transport and Appwrite server mode are still being
built. The current Appwrite direct implementation is native. The same app has
a Supabase mode; run it with `SUPABASE_URL` and `SUPABASE_ANON_KEY` Dart defines
and leave `ATLET_PROVIDER` unset. `NOSTOS_MODE=server` selects the existing
Supabase server path; direct is the default.

## Repeat the real cloud test

From the repository root, create the ignored `apps/atlet/.env.cloud` from
`apps/atlet/appwrite/credentials.example`, then run:

```sh
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --scenario order
```

This drives the visible UI: an admin creates a product, customer A adds it to
the cart and checks out while offline, customer B cannot see A's order, and the
admin ships and delivers it. The final customer view verifies the three durable
order events. The runner gives each invocation a fresh local SQLite file and
keeps credentials out of command arguments.

For ordinary Flutter checks:

```sh
fvm flutter analyze
fvm flutter test
```

The SourcePackages directories above work around Flutter 3.47's SwiftPM
plugin-copy issue on a fresh macOS checkout.
