# Atlet Flutter web on Appwrite Cloud — design (2026-09-26)

## Scope and decision

Phase 2 of `atlet-cross-sdk-cloud-reference-2026-09-26.md` runs the same
Flutter Atlet screens against the same hosted ADS Appwrite project from Chrome.
Direct mode remains the default. The browser keeps its Nostos rows, cursor,
principal, and outbox in the existing SQLite-WASM OPFS store. Appwrite Auth
stays in Flutter; the Worker receives a short-lived user JWT and never saves
it. The hosted `atlet_sync` Function remains the only writer to cloud tables.

Extend the existing Rust WASM `NostosEngine` with a small Appwrite-specific
surface for principal binding, cursor-checked page apply, and durable outbox
entry/ack operations. A browser Worker owns HTTP calls and a bounded five
second sync loop. Its message protocol matches `WebNostosEngine`, so the Dart
database, collections, watches, and visual screens need no second app path.
Keep the provider-specific Worker transport in a separate module to avoid
copying Nostos's cursor or outbox rules into JavaScript.

The alternatives were implementing the Appwrite cursor in JavaScript or
making Dart own the HTTP loop. Both split the transaction and identity rules
from `nostos-core` while OPFS still requires a Worker. Routing through
`nostos-server` would not exercise the requested direct default.

## Browser invariants

1. The Rust engine stores a principal derived from endpoint, project,
   Function, user, and server-confirmed access scope. A different identity
   clears rows, cursor, and outbox before any watcher can read them. A 403
   `account inactive` response clears all local state, emits
   `accessRevoked`, and stops retrying until a new signed-in session binds.
2. Push pending writes before pull, as the native Appwrite client does. Every
   push carries the stored mutation ID. Mark an outbox entry done only after
   the Function acknowledges it; permanent errors enter the dead-letter queue
   with a visible reason; transient errors leave the entry queued. The Worker
   never reconstructs mutation IDs from browser memory.
3. Validate and apply each pull page through `AppwriteCursor::apply`. Save
   `scanned_through` only after visible rows commit. A retry after a crash may
   replay a page, and local row apply must remain idempotent. Pull up to 64
   pages of 100 entries per wake, then schedule the next wake.
4. Offline writes render from the same local store immediately and survive
   browser reload. `disconnect` pauses HTTP without clearing the store;
   `resume` wakes it. `signOut` wipes rows, cursor, outbox, principal, and JWT.
5. A browser with no durable OPFS store reports degraded storage visibly.
   The cloud acceptance runner fails that state rather than counting a memory
   fallback as an offline/restart pass. A SharedWorker broker gives each tab a
   private MessagePort; the authenticated host tab starts one dedicated OPFS
   Worker and transfers an engine port to the broker. The broker validates each
   Appwrite JWT before forwarding private rows; the dedicated Worker validates
   the initial JWT before binding the cached principal. No credential or row
   travels over BroadcastChannel. Without SharedWorker, only one tab can own
   OPFS; another tab fails closed.

## Assets and cloud setup

The Flutter app currently copies the Nostos Worker and WASM files but lacks
the SQLite-WASM module referenced through `../node_modules` by
`sqlite_wasm_glue.js`. Package the pinned SQLite-WASM distribution with the
app under a stable local `web/nostos/` path, preserving its license. Serve all
Worker imports from the app origin; no CDN is required for offline boot.
The ADS project already has `localhost` and `127.0.0.1` web platforms.

## Acceptance

- Rust host tests cover principal changes, cursor gaps, outbox acknowledgement,
  inactive-profile wiping, and no stale write after sign-out.
- A Chrome browser test proves OPFS persistence across reload, offline local
  writes, hosted replay, and cross-account row isolation.
- The full visual admin/customer A/customer B and order browser tests run
  through the Rust `appwrite_flutter_web_smoke` launcher, with one hosted
  project, credential-free evidence, and no skipped test for missing tooling.
- An arxa CI job runs the Chrome suite on PRs, serialized with the existing
  fixed-account cloud job. The SDK and app README explain asset packaging and
  the Rust launch command. Physical Appwrite push remains its separate gate.

## Integration risk and proof

The Flutter Worker protocol is shared by server and Appwrite modes. The broker
must preserve watch, status, write, query, token refresh, disconnect, resume,
and sign-out message shapes while routing replies only to the authenticated
port. The browser test checks both the real OPFS path and a raw
BroadcastChannel listener; a build-only pass cannot validate either.
