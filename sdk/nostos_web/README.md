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
`web-sys::WebSocket` + `Window::localStorage` (now an injectable seam —
`setKvStore`, still window-backed by default), neither of which
Node provides.
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

## Experimental: Web Push (default OFF — plan 6.1/6.2, ADR-0037 §6 Wave 3)

**Flag: `enableWebPush`** (`sdk/nostos_web/push.js`). Nothing registers the
Service Worker, asks for notification permission, or touches `/push-tokens`
until the host app calls it with explicit config — existing embedders see zero
behavior change (ADR-0033 experimental-flag discipline). In the e2e boot path
(`e2e/app.html`) the config persists under the localStorage flag key
**`cairn:experimental:webpush`** so every load re-arms the wake listener
(remove that key + call `disable()` to turn it off; a listener does not
survive a reload).

```js
const push = await import("/push.js");
const r = await push.enableWebPush({
  vapidPublicKey: "<base64url P-256 public half of the server's VAPID keypair>",
  httpBase: "http://your-nostos-host:8080", // http(s) form of the sync ws:// url
  token: jwt,                             // same JWT as the sync connection
  swUrl: "/nostos.sw.js",                  // serve the SW at the scope root
  onWake: () => worker.postMessage({ cmd: "wake" }),
});
await r.disable(); // sign-out: unsubscribe + DELETE /push-tokens/{token}
```

**Server config** (`nostos-server`, ADR-0037 §1/§5): `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY`
(base64url P-256 scalar) + `NOSTOS_WEBPUSH_VAPID_SUBJECT` (mailto:). The server
does not expose the public half over REST — configure the matching
`vapidPublicKey` client-side. Registration rides the pinned REST contract:
`POST /push-tokens` `{"platform":"webpush","token":<subscription JSON>}` +
`Bearer` → `204`; the **whole subscription JSON is the token**.

**Architecture** — push is a doorbell, sync is the transport (ADR-0037 §2):
`push` event → `sw/nostos.sw.js` (the wake relay) → `client.postMessage
("nostos:wake")` → the page forwards `{cmd:"wake"}` to `worker/nostos.worker.js`,
which reconnects if the socket is down and resumes from the durable checkpoint
(ADR-0033). A missed push loses nothing — the LSN checkpoint is the
correctness mechanism.

**Also shipped (6.1):** the wasm transport's `Window::localStorage`
checkpoint dependency is now an injected `KvStore` seam — call
`setKvStore(anything-with-getItem/setItem)` (exported from the wasm package)
before `NostosSocket.connect` to swap in a Map-backed store (Service-Worker
compatible); unset = `window.localStorage`, unchanged.

**What works** (proven by `e2e/webpush.spec.cjs`, Playwright/headless Chromium):
the KV seam swap (injected store receives the checkpoint; localStorage
untouched; default unchanged), the SW wake chain on a *synthetic* push (the
spec drives the SW's real `push` handler via its `nostos:simulate-push` test
hook: closed session → wake → reconnect → server row applied), and the exact
REST wire shape (POST/DELETE, Bearer, percent-encoded token, non-204 rejects).

**What doesn't (assumed, not headless-provable):**
- **The live push-service leg** — a real encrypted push through a push
  service to the SW. Headless Chromium has no push service (`subscribe`
  fails, recorded as the degrade reason); verify on a real browser against a
  `NOSTOS_WEBPUSH_VAPID_*`-configured server.
- **Killed app (no open client):** the SW shows the visible notification but
  wakes nothing — the wasm engine does not run inside the SW yet
  (`ponytail:` ceiling in `sw/nostos.sw.js`; upgrade = host the engine in the
  SW over the 6.1 KV seam, or adopt declarative push if it ships). Data is
  never lost — the next open resumes from the checkpoint.
- **iOS**: the research doc (`docs/plans/nostos-push-notifications-research-2026-08-14.md`)
  rates web push "L" effort and does not cover Home-Screen-Web-App specifics;
  general platform reality (verify on device): iOS Web Push requires the app
  installed to the Home Screen (16.4+), and killed iOS apps receive *visible*
  payloads reliably but silent wakes only opportunistically.
- **`pushsubscriptionchange`** (key rotation) is not auto-resubscribed —
  re-run `enableWebPush` on boot/login (the e2e boot path already does).

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

### Multi-tab, eviction, bundlers (2026-09-21 — see `docs/research/web-sdk-best-practices-2026-09-21.md`)

- **One durable engine per origin, every tab uses it.** `opfs-sahpool` installs
  once per origin (sqlite.org persistence doc). The Worker takes a Web Lock
  (`cairn:opfs-sahpool`) before opening OPFS; a tab that loses the lock becomes
  a **follower**: it proxies every command over a `BroadcastChannel` to the
  leader tab's Worker (responses by id, pushes mirrored) and reports the
  leader's mode with `reason:"follower"`. A later `connect` joins the live
  session (first tab's url/table win); `close`/`signOut` from any tab end it
  for all. When the leader tab closes, the Web Lock queue promotes a follower,
  which opens OPFS and replays its own `connect` (in-flight requests at that
  moment are lost). Not a `SharedWorker`: Chromium exposes no `Worker` in
  `SharedWorkerGlobalScope` and opfs-sahpool needs a dedicated worker's sync
  access handle. Opt a tab OUT with `allowSecondaryTab: true` on connect →
  standalone memory engine, own socket, `reason:"secondary-tab"`. Pinned by
  `e2e/secondary_tab.spec.cjs`.
- **Ask for persistent storage.** Origin storage is best-effort and evictable
  (MDN). Call `await navigator.storage.persist()` on the main thread before
  spawning the Worker; the storage push carries `persisted` so the UI can warn.
- **Guard pending writes on close** (PowerSync recipe): when `deadLetters().pending > 0`
  add a `beforeunload` listener that calls `preventDefault()`. Do it always when
  `storageMode === "memory"` — those writes die with the tab.
- **Bundlers.** Construct the Worker so Vite/webpack can see it:
  `new Worker(new URL("@nostos-sync/web/worker/nostos.worker.js", import.meta.url), { type: "module" })`.
  `worker/`, `sw/` and `pkg-web/` ship in the npm `files` list; run
  `npm run build:web` before packing.

**A third gap is Node-only.** `NostosSocket.connect()` is wired to
`web-sys::WebSocket` + `Window::localStorage` (default of the injectable
`setKvStore` seam), which Node lacks, so the
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
