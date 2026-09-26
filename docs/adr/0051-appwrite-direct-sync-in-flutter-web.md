# ADR-0051 — Appwrite direct sync in Flutter web

Date: 2026-09-26
Status: accepted for the Atlet browser reference

## Context

ADR-0050 introduced a cursor-based Appwrite Function and native direct client.
Atlet's Flutter web build already used the shared Rust WASM apply engine with
SQLite-WASM OPFS for Nostos server mode. Its Appwrite factory still rejected
web, so the same visual app could not exercise hosted direct sync in Chrome.

## Decision

The Flutter web Worker keeps its existing message protocol to Dart and adds an
Appwrite HTTP transport. That transport schedules push-before-pull and carries
the current user JWT only in Worker memory. It sends requests to the deployed
`atlet_sync` Function; the Function remains the cloud database writer and
authorizes each user. The Rust `NostosEngine` WASM surface owns the durable
principal, mutation IDs, cursor, page validation, local apply, outbox, and
acknowledgements. The browser uses the same OPFS SQLite store and Flutter
screens as server mode. The app packages SQLite-WASM and the generated Nostos
WASM assets on its own origin.

Principal binding includes endpoint, project, Function, user, and server
confirmed scope. An identity or scope change clears rows, outbox, and cursor
before the new scope is visible. A disabled account response clears local
state and stops sync. Sign-out aborts in-flight HTTP requests, wipes the store,
and reports an error if the wipe fails. A disconnected client may still queue
local writes, which the Worker pushes on resume. The browser acceptance gate
requires OPFS persistence across a reload and a second Nostos client seeing
the hosted commit; memory fallback is a failure for that gate.

The app shows the Worker's actual storage mode, including temporary memory
fallback. Sign-out waits for the Worker's wipe acknowledgement before ending
the cloud session; a failed wipe leaves sign-out available for retry. The
browser opens a SharedWorker broker with one private MessagePort per tab. The
authenticated host tab starts a DedicatedWorker for the OPFS SQLite engine and
transfers a private MessagePort to the broker: browser sync file handles are
available only in a dedicated worker. No JWT or row
payload crosses an origin-wide BroadcastChannel. A browser without
SharedWorker falls back to one dedicated tab; a second tab fails closed when
the OPFS Web Lock is owned elsewhere.

For Appwrite, the broker verifies each tab's JWT with
[GET /account](https://appwrite.io/docs/references/cloud/server-rest/account)
using that token alone, without browser cookies, before it can receive cached
rows. The dedicated engine verifies its initial JWT again before binding a
principal to OPFS. Distinct valid JWTs for the same active user can join and
refresh independently; each tab's lease ends at its JWT expiry or 12 minutes,
whichever comes first. Failed validation revokes that port and rejects later
requests promptly. Different users are rejected before any snapshot or query
response is forwarded. Server mode still requires the same bearer token for
additional tabs because it has no separate token introspection API. The cloud
browser gate checks account isolation, same-user second-tab snapshots, token
rotation, and that a raw BroadcastChannel listener sees no private traffic.
An expired tab may still request a local wipe for the session it joined, even
when Appwrite is offline. If the engine Worker dies, the broker reopens OPFS
and validates a fresh bearer before forwarding reads or writes. Sign-out
retires the old host Worker; the next account retries OPFS lock acquisition
while that Worker shuts down.

## Verification and limits

The Rust host tests cover principal changes, role changes, hidden-only pages,
bad cursors, stable mutation IDs, acknowledgements, and revoked writes. The
Rust `appwrite_flutter_web_smoke` launcher builds the Flutter JS target, drives
Chrome, and writes credential-free evidence. The arxa `atlet-web-cloud` job
serializes its three real accounts with the native Appwrite cloud job.

This decision covers the Appwrite direct path in Flutter web. Server-mode
Flutter web remains on its existing WebSocket transport. Physical push,
Appwrite team promotion, and ports of the Atlet UI to other Nostos SDKs have
separate acceptance gates in the cross-SDK plan.
