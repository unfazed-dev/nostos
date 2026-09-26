# Atlet — Nostos reference app

Atlet is the full visual training and shop app used to check Nostos against a
real hosted database. The same Flutter UI supports Supabase and Appwrite Cloud.
Its Nostos client keeps a SQLite cache and durable outbox on the device; an
offline write appears locally and syncs when connectivity returns. Direct mode
is the default. See [the Flutter app](flutter/README.md) for its Supabase setup.

## Appwrite Cloud setup

The ADS organization's `nostos` project in `fra` hosts the shared `atlet`
database. Numbered schema changes live in `appwrite/migrations/`; the Rust
Function source lives in `appwrite/function/`. Migration 0001 creates the app
tables and change journal; 0002 adds the idempotent mutation ledger. Never edit
an already applied migration. Only the Function holds a server API key. The
Flutter app signs in through Appwrite Auth and sends its short-lived user JWT
to the Function.

Copy `appwrite/credentials.example` to the ignored `apps/atlet/.env.cloud`,
fill in the three demo accounts, and restrict it to your user (`chmod 600`).
The three account IDs expected by the Rust runners are `atlet_admin_demo`,
`atlet_user_a_demo`, and `atlet_user_b_demo`; put the admin in the
`atlet_admins` team. The runner passes credentials through a temporary mode
0600 Dart define file and deletes it afterward. Keep build artifacts private
because debug builds can contain those test credentials.

From the repository root, the repeatable cloud checks are:

```sh
cargo run -p atlet-harness --bin appwrite_smoke
cargo run -p atlet-harness --bin appwrite_smoke -- --users-workflow
cargo run -p atlet-harness --bin appwrite_smoke -- --order-workflow
cargo run -p atlet-harness --bin appwrite_native_smoke
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --role customer_a
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --role customer_b
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --role admin
cargo run -p atlet-harness --bin appwrite_flutter_smoke -- --device macos --scenario order
```

The Flutter test drives the real UI: sign-in, an offline SQLite session write,
cloud echo, cleanup, and sign-out for each customer; catalog create/delete and
sign-out for the admin. The order scenario drives one offline purchase through
customer A, checks customer B cannot see it, and has the admin ship and deliver
it in the UI. The native runner checks offline reopen, echo, and concurrent
convergence across two customer A devices, customer B, and an admin, including
private isolation and a tombstone. All three accounts use the same hosted
project. These commands require network access and the ignored credentials
file; they never use a local Appwrite container.
Each Flutter invocation writes a credential-free JSON result to the ignored
`apps/atlet/.results/` directory with the project, commit, device, scenario,
run ID, duration, exit status, and the convergence counts emitted by the UI
test. Use `--evidence-dir` to put it elsewhere. A missing convergence marker
fails the run.

For schema operations, set `APPWRITE_PROJECT_ID` and a scoped, server-only
`APPWRITE_API_KEY` in the shell. The check is read-only unless `--apply` is
present:

```sh
cargo run -p atlet-harness --bin appwrite_migrate
cargo run -p atlet-harness --bin appwrite_migrate -- --apply
cargo run -p atlet-harness --bin appwrite_probe -- --writers 4
```

The transaction probe creates unique fixture products, checks concurrent
commits and reconnect, and deletes its fixtures through the journal. Its
throughput is not a benchmark. The Function deployment can be packaged by
`appwrite_deploy`; use its default dry run before `--apply` and supply the
Function-scoped API key only to the apply process.

## Current coverage

Appwrite direct sync passes the native runner and the macOS Flutter UI checks
for one admin and two customers, including offline checkout, admin fulfilment,
three durable order events, and customer isolation. The order scenario deletes
its product fixture and retains the delivered order as demo history.
Supabase remains available through the same Flutter UI. Appwrite Flutter web,
Appwrite server mode, physical-device push, and the other SDK app ports still
need cloud acceptance before this is a complete cross-SDK reference. The
[design plan](../../docs/plans/atlet-appwrite-flutter-design-2026-09-26.md)
tracks those gates.
