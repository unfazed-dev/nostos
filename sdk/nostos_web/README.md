# @nostos-sync/web

PowerSync-style JS facade over the `nostos-ffi-wasm` apply engine.

**Status: v0.1 alpha, not published to npm.** This package has **two distinct
paths**, and they differ in what they can do — read both before judging scope:

| path | entry point | live WebSocket? | proven by |
|---|---|---|---|
| **Browser** | `pkg-web/` (`wasm-pack --target web`) → `NostosSocket` | **Yes** — the browser's real `WebSocket` | `e2e/browser_live.spec.cjs` under Playwright: full PUSH + ECHO round-trip against the Rust spine |
| **Node** | `index.js` (`pkg-node/`, `--target nodejs`) | No — apply engine only | `smoke.cjs` |

`make sdk-e2e web` runs the **browser** path, and it passes a live
round-trip. So "does Nostos sync in a browser?" is demonstrated, not assumed.

The **Node facade in `index.js` is the reduced-scope half**: it drives the
in-memory apply engine (`NostosEngine`, `Frame`, `Outcome`) and its `connect()`
deliberately does **not** open a socket, because `NostosSocket` is wired to
`web-sys::WebSocket` + `Window::localStorage`, neither of which Node provides.
See "Ceiling" below — that limitation is Node-only.

> Until 2026-07-30 this README described only the Node path and called the whole
> package a "reduced-scope feasibility proof" that "does NOT yet open a live
> WebSocket." That understated a shipped, e2e-proven capability. The API table
> below likewise documents the Node facade, not `NostosSocket`.

## Build

```sh
# from sdk/nostos_web (or repo root with -p)
npm run build
# → invokes: wasm-pack build ../../crates/nostos-ffi-wasm --target nodejs --out-dir pkg-node
```

The build writes to `crates/nostos-ffi-wasm/pkg-node/` (gitignored). The facade resolves that path relative to itself, so any cwd works.

## Smoke

```sh
node smoke.cjs
```

Exercises `connect`, `subscribe`, `write`, `query`, `watch` against the in-memory engine.

## Browser API — `NostosSocket` (the live path)

Imported from the `--target web` build (`pkg-web/`), not from `index.js`.

| member | behavior |
|---|---|
| `NostosSocket.connect(url, token, table, whereSql)` | **static**, `Promise<NostosSocket>` — opens the real `WebSocket` and subscribes. `token` goes on the URL as `?token=` (browsers cannot set handshake headers). `resume_lsn` is read from `localStorage["cairn:checkpoint:<table>"]`, defaulting to 0 |
| `write(table, op, pk, payloadJson, clientWriteId)` | sends a write frame; rejects if the socket is not OPEN |
| `rowsFor(table)` | the rows currently applied |
| `checkpoint` | getter — the durable LSN persisted to `localStorage` |

```js
import init, { NostosSocket } from "./pkg-web/nostos_ffi_wasm.js";

await init();
const sock = await NostosSocket.connect("ws://127.0.0.1:8080/sync", null, "tasks", null);

sock.write("tasks", "upsert", "t1", JSON.stringify({ title: "Walk dog" }), "w1");
const rows = sock.rowsFor("tasks");
```

Requires a browser (or a Playwright/vitest browser env): `WebSocket` and
`localStorage` must exist. This is the path `e2e/browser_live.spec.cjs` drives.

## Node API — `NostosClient` (PowerSync-shaped, apply-engine only)

From `index.js`. `connect()` here does **not** open a socket — see *Ceiling*.

| method | behavior |
|---|---|
| `new NostosClient({ url, token, table })` | construct (no I/O) |
| `connect()` | `Promise<this>` — reduced-scope: marks ready, does not open WS |
| `subscribe(table, whereSql)` | stores the predicate on the engine |
| `write(table, pk, payload)` | feeds an insert Frame, flushes, returns `{ checkpoint, rowsApplied }` |
| `query(table)` | reads the rows currently held via `rowsFor` |
| `watch(table, cb)` | fires `cb` once with a snapshot, returns an unsubscribe stub |
| `checkpoint` / `rowCount` | getters on the engine |

**Typed Tier-1 surface** (ADR-0030/0032 — the same surface the Flutter SDK and the
wasm `NostosEngine` expose; forwarded since Wave 4a):

| method | behavior |
|---|---|
| `setCrdtTables(orSetTables, counterTables)` | tag tables so CRDT verbs **merge** instead of clobber — call before any `orSet*`/`counter*` (mirrors the server's `NOSTOS_*_COLUMNS`) |
| `writeBatch([{table, op, pk, payloadJson?}])` | atomic batch (`op`: `upsert`\|`delete`\|`patch`); returns the outbox write ids |
| `orSetAdd(table, pk, el)` / `orSetRemove(...)` | add-wins OR-set element |
| `counterIncrement(table, pk, delta)` / `counterDecrement(...)` | PN-counter delta |
| `pendingCount` / `deadLetteredCount` / `lastError` | outbox + dead-letter visibility (getters; `lastError` is falsy when no write has dead-lettered) |

## Attachments — two-plane blob sync (T6, ADR-0034)

Re-exported from `index.js`: `Attachments`, `SupabaseStorageAdapter`,
`OpfsBlobStore`, and `AttachmentConstants` (`TABLE`/`COL`/`STATE`). The metadata
plane is an ordinary synced `attachments` table; the **blob plane is a
developer-supplied adapter — blob bytes never transit the Nostos server** (moat
constraint). The driver is a pure state machine over three injectable seams:

| seam | role | browser | node |
|---|---|---|---|
| `AttachmentStorageAdapter` | remote bucket (upload/download/delete) | `SupabaseStorageAdapter` (`@supabase/supabase-js`, peer dep) | a fake, or `SupabaseStorageAdapter` |
| `BlobStore` | local cache | `OpfsBlobStore` (real OPFS, browser-only) | an in-memory fake |
| `AttachmentMetadataGateway` | read queued rows + patch state | *(live Worker gateway — see below)* | an in-memory fake |

```js
const { Attachments, SupabaseStorageAdapter, OpfsBlobStore } = require("@nostos-sync/web");
const a = new Attachments({
  gateway,                                   // your metadata-plane gateway
  adapter: new SupabaseStorageAdapter({ url, key, bucket: "files" }),
  blobStore: new OpfsBlobStore("nostos-blobs"), // browser only
  isOnline: async () => navigator.onLine,
});
const id = await a.queueUpload({ filename, bytes, mediaType });
await a.pump(); // → uploads, state flips queued_upload → synced
```

Lifecycle: `queueUpload`→`queued_upload`, `queueDownload`→`queued_download`,
`remove`→`queued_delete`; `pump()` dispatches each (when online) and flips
state — upload/download→`synced`, delete→`archived`. Adapter failures retry with
exponential backoff, then dead-letter to `archived` after `maxAttempts` (default
5). Declare the `attachments` table and add it to `NOSTOS_WRITE_TABLES` so the
client can patch state server-side (the standard write-back foot-gun).

**Tested:** the state machine is proven in node (`node --test e2e/attachments.spec.cjs`,
part of `npm run smoke`) with in-memory fakes — upload/download/delete/retry→
dead-letter/offline/wipe — guarding it against divergence from the Flutter driver.
**Remaining wiring:** the browser *live* metadata gateway (reading queued
`attachments` rows + patching state over the Worker's postMessage) and a real-OPFS
blob-plane browser test; the spec's fake gateway "stands in for" the live one.

## Ceiling (ponytail)

**In the browser this is an *in-session offline-capable* client** — writes do not
require a live socket. `NostosSocket.write` captures the frame into an in-memory
`Outbox` (`enqueue`) and renders the local row at once (`apply_local`), so a write
while disconnected **queues** instead of throwing and flushes on reconnect — the
synchronous "socket not OPEN" throw is gone (shipped `9004b3c`, "WS1 slice 2"; see
the [ADR-0017 addendum](../../docs/adr/0017-web-persistence.md)). One limit remains,
by design:

- **The browser Worker path IS reload-durable** (ADR-0033, shipped Wave 2): rows
  and pending writes persist to OPFS via `opfs-sahpool` — the *synchronous*
  `FileSystemSyncAccessHandle` primitive, Worker+browser-only — so a refresh
  resumes from the SQLite checkpoint with nothing lost. This resolved the earlier
  "`Storage`/`Outbox` are sync traits, IndexedDB is async" blocker (opfs-sahpool
  is sync). Proven by `e2e/durable.spec.cjs` (write survives a full reload;
  `signOut` wipes the store). The **Node `NostosClient` facade stays in-memory**
  (no OPFS in Node) — for Node-side durability use `@nostos-sync/node` (napi + real
  SQLite).

So: the browser client is offline-capable AND reload-durable; the Node facade is
offline-capable within a session only.

**A third gap is Node-only.** `NostosSocket.connect()` is wired to
`web-sys::WebSocket` + `Window::localStorage`, which Node lacks, so the
`index.js` facade intentionally does not call it — it would panic at the first
`web_sys::WebSocket::new()` or `window()` access. The browser build has no such
limit and is exercised live by `e2e/browser_live.spec.cjs`.

Upgrade path for the Node path:

1. **Node WS adapter** — a thin Rust module replacing the web-sys transport at
   the `NostosSocket` seam, gated behind `#[cfg(feature = "node-transport")]`.
   (`@nostos-sync/node` already covers server-side Node via napi + real SQLite, so
   this is only worth building if a wasm-in-Node story is specifically wanted.)

Also still open:

2. **`watch` is a stub** on the Node facade — it fires the callback once with a
   snapshot and returns a no-op unsubscribe. Real reactivity is the
   [reactive-facade ADR](../../docs/adr/0024-client-reactive-facade-and-query-primitive.md)
   shape.
3. **Not published to npm.** A11 — see the completion assessment.

## What this proves

Nostos's wasm apply engine loads and runs in Node 22 via `require()`, with a PowerSync-shaped JS surface on top — moving Nostos from 3/10 to 5/10 platform coverage.
