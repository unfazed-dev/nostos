# Atlet Flutter on Appwrite Cloud — design (2026-09-26)

## Decision and scope

The first Appwrite reference app is the existing full visual Atlet Flutter app. It uses the ADS organization's `nostos` Appwrite Cloud project (`6ab741900038c74d1086`, `fra`) and a dedicated `atlet` TablesDB database. Appwrite support is a Nostos backend path: Flutter continues to read and write through Nostos's local SQLite, outbox, watches, and apply engine. The provider selection must not substitute a plain online Appwrite client for sync.

Direct mode remains the default. For Appwrite, direct means device → Appwrite-hosted Rust Function → TablesDB, with no always-on `nostos-server`. The Function is analogous to Supabase direct mode's generated RPC. Server mode later reuses the same source and authorization contract behind `nostos-server`. The selected provider is fixed for a signed-in session; changing provider signs out, closes sync, and opens a separate local database to prevent cross-provider data mixing.

This is the first phase of the broader ten-SDK Atlet program. Flutter must establish the contract before other SDK ports. Supabase and Appwrite are distinct hosted databases; clients using the same provider share one cloud project and schema.

## Appwrite data and security

Numbered, immutable schema migrations live in `apps/atlet/appwrite/migrations/`. A Rust migration runner checks each migration's hash against a `schema_migrations` table and applies missing steps in order. The same manifests can be inspected and applied through the Appwrite MCP during initial development. No secret is checked in. The runner needs a server API key only when applying migrations, never in the Flutter binary.

Migration 0001 contains the Flutter tables (`sessions`, `products`, `cart_items`, `orders`, `order_events`, `attachments`), `user_profiles` for display names, and the sync tables (`sync_clock`, `sync_changes`, `schema_migrations`). Migration 0002 adds `sync_mutations`, keyed by a stable client mutation ID, so an ambiguous HTTP retry cannot apply the same write twice. The Appwrite row ID is mirrored as the `id` field in Nostos frames. Sessions and cart rows belong to one customer. Products are public; customers see their own orders and events; an admin sees all orders, products, and users, but no customer's private training sessions. The Function validates the Appwrite session and every write; API keys stay inside the Function. An `atlet_admins` team identifies admins. Direct SDK writes to synced tables are disabled so they cannot bypass the journal.

## Durable sync protocol

Realtime and push are wake-up hints only. The Function serves a durable feed with decimal-string `seq`, `head`, and `scanned_through` values, plus table, primary key, operation, and full row. Every accepted data change, its change row, and its mutation ID ledger row commit in one TablesDB transaction. A single `sync_clock` row is the conflict point for assigning a global sequence: concurrent writers that stage against the same clock value retry after a transaction conflict. A pull first reads the committed clock, then scans journal entries at or below it. The Function filters private entries and returns the highest **scanned** sequence even when no visible rows remain; the client applies visible rows before durably saving that cursor. The existing Supabase `nostos_core::PullCursor` cannot represent an empty filtered page and must not be reused unchanged. The first version keeps the full journal and bootstraps from sequence zero, avoiding an unproved multi-page snapshot handoff. Retention and snapshot compaction require a later proven protocol migration.

The Rust cloud probe passed basic atomicity, concurrent conflict retry, reconnect catch-up and tombstones on 2026-09-26: four concurrent writers, six retried conflicts, contiguous sequence through head 13, then journaled cleanup of three earlier manual fixtures through head 16. This validates the TablesDB transaction primitive. Function-crash replay, authorization, private-page cursor advancement, offline devices, and sustained stress remain acceptance gates. This gate follows the [research note](../research/atlet-appwrite-tablesdb-sync-2026-09-26.md).

## Flutter acceptance

One admin and at least two customers sign in with real Appwrite Auth accounts. The admin manages products, orders, and users in a visible UI. Each customer sees the shared catalog and only their own sessions, cart, orders, and order events. An offline customer session edit and an offline cart/order action appear immediately in local Nostos state, queue durably, and converge after reconnection; admin changes then appear on another customer device. Sign-out removes credentials and cannot expose the previous account's private local data. The app supports provider-specific direct and server modes with direct selected by default; unsupported combinations fail visibly until implemented.

Push is validated with real Appwrite Messaging targets on supported devices/browsers. A missed push must never be the only way data becomes current; reconnect/foreground polling drains the feed. The Rust Atlet runner executes the same scenario with `--backend appwrite` and records project, app build, SDK, device, seed, timings, errors, and convergence counts.

## Delivery gates

The migration runner and cloud probe precede the Flutter backend switch. The Flutter Appwrite path must pass unit, integration, real-cloud multi-user, offline/restart, permissions, and push checks before any PR is mergeable. Existing arxa CI contexts stay required; hosted suites run through an explicit cloud environment/manual trigger with scoped secrets and a unique fixture namespace. No new SDK port starts until the Flutter cloud path is validated.

## Validation record, 2026-09-26

- Cloud project `6ab741900038c74d1086` has migrations 0001 and 0002 applied; the drift check found neither missing.
- Rust native smoke covered offline reopen, remote echo, idempotency, and account isolation. The Function smoke covered private sessions, catalog ACL, and paid → shipped → delivered events.
- The Flutter macOS visual tests passed for admin, customer A, and customer B. The order scenario passed with customer A's offline cart and checkout, admin fulfilment, three durable events, and customer B's isolation. The hosted order `b52fd581-3c1e-4424-902f-dc72f20a2a5e` is delivered; its product fixture was deleted through the Function.
- The order scenario found that TablesDB truncates timestamps to milliseconds. The Function now normalizes order creation and compares immutable timestamps at that precision; regression tests and Function Clippy pass. The active deployment is `6ab770ab9a11c94015eb` (Rust 1.83, locked dependencies, user profiles, access scope).
- The admin Users tab, three profile visibility rules, disable/reactivate flow, native four-device convergence and role-scope cache reset have passed their focused tests. `scripts/check.sh atlet-cloud` passed against the active Function. Each Flutter visual run now writes a credential-free JSON evidence file.
- Appwrite Messaging has no provider in this project. FCM or APNs credentials and a supported physical target are needed for the push acceptance gate. Flutter web and Appwrite server mode remain unimplemented.
