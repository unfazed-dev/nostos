# Web — `@nostos-sync/web`

Extracted from `sdk/nostos_web/` + `crates/nostos-ffi-wasm/src/lib.rs`. Index: [`README.md`](README.md).

**This package has two different APIs and only one of them syncs.** Getting them mixed up is the
single most likely mistake here.

| Path | Entry | Live WebSocket? | Durable? | Use for |
|---|---|---|---|---|
| **Browser (Worker)** | `worker/nostos.worker.js` → postMessage proxy → `NostosSocket` | **Yes** — the browser's real `WebSocket` | **Yes** — OPFS/SQLite-WASM (ADR-0033) | shipping a web app |
| **Node** | `index.js` → `NostosClient` | **No** — apply engine only | No | tests, replaying frames, server-side decode |

If you want a synced browser app, use the Worker architecture (below). If you are in Node and want
real sync, you want [`@nostos-sync/node`](node.md) instead.

## Browser — the Worker architecture (ADR-0017 / ADR-0024 / ADR-0033)

The browser SDK runs entirely inside a **Web Worker** (`sdk/nostos_web/worker/nostos.worker.js`).
The main thread is a pure `postMessage` proxy that imports NO wasm. The Worker owns:

1. **The wasm instance** (`NostosSocket` + the apply engine + storage backend).
2. **The storage backend** — either durable (SQLite-WASM / `opfs-sahpool`) or memory
   (`InMemoryStorage`), decided at boot.
3. **The WebSocket** — `web-sys::WebSocket`, opened inside the Worker.

### Storage modes (ADR-0033)

On boot, the Worker async-initializes `@sqlite.org/sqlite-wasm` with the `opfs-sahpool` VFS.
The mode is pushed to the main thread as `{type:"storage", mode:"durable"|"memory"}` and surfaced
on `SyncStatus.storageMode`:

| Mode | Backend | Survives reload? | When |
|---|---|---|---|
| `"durable"` | SQLite-WASM (`opfs-sahpool`) | **Yes** — rows + outbox + checkpoint persist in OPFS | Chrome/Edge/Firefox/Safari 17+ with OPFS enabled |
| `"memory"` | `InMemoryStorage` | No — everything is lost on reload | Safari Private Browsing, old browsers, OPFS disallowed |

The memory path is NOT a crash — it is the explicit degrade fallback (ADR-0017 follow-up scope 5).
The mode is checked at runtime; no user configuration is needed.

### Header-free deployment — a feature

`opfs-sahpool` uses synchronous `FileSystemSyncAccessHandle` writes, NOT `SharedArrayBuffer` /
`Atomics`. Cross-origin isolation (COOP/COEP headers) is **NOT required**. This is a deliberate
advantage over wa-sqlite's `OPFSCoopSyncVFS` (which forces COOP/COEP onto every deployment,
breaking OAuth popups, analytics iframes, and non-CORS-clean embeds).

Your `vite.config.ts` / static server ships **zero special headers**. This is a requirement of the
chosen backend (ADR-0017 Decision), not merely a current-state observation.

### The postMessage protocol (main thread → Worker)

```js
const worker = new Worker("/worker/nostos.worker.js", { type: "module" });

// Requests (each carries `id`; responses are {id, ok:true, ...} or {id, error:"..."}):
worker.postMessage({ id: 1, cmd: "connect", url, token, table, where_sql });
worker.postMessage({ id: 2, cmd: "write", table, op, pk, payload_json, client_write_id }); // no id (fire-and-forget)
worker.postMessage({ id: 3, cmd: "rowsFor", table });
worker.postMessage({ id: 4, cmd: "checkpoint" });
worker.postMessage({ id: 5, cmd: "watch", table });     // reactive subscribe (ADR-0024)
worker.postMessage({ id: 6, cmd: "unwatch" });
worker.postMessage({ id: 7, cmd: "signOut" });           // ADR-0029 + ADR-0033: wipe OPFS + localStorage + token
worker.postMessage({ id: 8, cmd: "setToken", token });   // ADR-0029: token refresh = reconnect
worker.postMessage({ id: 9, cmd: "close" });
```

### Push events (Worker → main thread)

```js
worker.onmessage = (ev) => {
  const d = ev.data;
  // Unsolicited pushes (no `id`):
  if (d.type === "wasm-ready") { ... }
  if (d.type === "storage") { /* d.mode = "durable"|"memory" */ }
  if (d.type === "status") { /* d.connected = bool */ }
  if (d.type === "snapshot") { /* d.table, d.rows = [{pk, payload}] — reactive push (ADR-0024) */ }
  if (d.type === "writeResult") { /* d.client_write_id, d.ok, d.error? — async write outcome */ }
  if (d.type === "rowsChanged") { /* d.count — legacy compat poll signal */ }
  // Response to a request (has `id`):
  if (d.id != null) { /* resolves the pending promise */ }
};
```

### The `write` contract (ADR-0017 WS1 / ADR-0013 outbox)

`write` is **fire-and-forget** over postMessage — it never throws because the socket is closed.
The Worker's `NostosSocket.write` captures the write via the Rust `Outbox` trait (`enqueue` +
`apply_local`) before any network round-trip, so:

- The row is visible **instantly** (optimistic local apply).
- The write ships on the next (re)connect via the onopen flush loop.
- The async outcome arrives as a `writeResult{client_write_id, ok, error?}` push.

This is the same contract as the native SDKs (durable intent before any network call).

### Sign-out (ADR-0029 + ADR-0033)

`signOut()` wipes:
1. **OPFS SQLite DB** — `dbHandle.clearAll()` (DELETE rows + outbox + checkpoint reset to '0').
2. **`localStorage["cairn:checkpoint:*"]`** — cleared from the main thread (Workers cannot access
   `localStorage`; belt-and-suspenders for any future main-thread checkpoint path).
3. **The cached token** — dropped so the next `connect` requires re-auth.
4. **The engine rows + outbox** — `sock.clearLocalState()` (the Rust `Storage::clear` +
   `Outbox::clear` under one borrow — half a clear is a cross-user leak).

### Durable checkpoint

In durable mode, the checkpoint is read from SQLite (`cairn_meta` key `'checkpoint'`), NOT from
`localStorage`. The `localStorage` key is retained ONLY as a sign-out wipe target (clearing it
prevents a stale-LSN resume after OPFS is wiped). In memory mode, the checkpoint lives only in the
engine's `InMemoryStorage` and is lost on reload.

### Reactive watch (ADR-0024)

The Worker fires a `snapshot` push on every change tick — the initial snapshot plus each delta —
synchronously from the `onmessage` frame-pump. This is a TRUE Rust→JS push (the `NostosSocket.onChange`
Closure), NOT a `setInterval` poll of `rowCount`. The main thread fans snapshots to per-table watchers.

## `NostosSocket` — the wasm-bindgen surface

A `wasm_bindgen` export from `crates/nostos-ffi-wasm/src/lib.rs`.

```js
import init, { NostosSocket } from "./pkg-web/nostos_ffi_wasm.js";
await init();

// In the Worker, dbHandle is the JS wrapper from openNostosDb() (or null in memory mode):
const sock = await NostosSocket.connect(url, token, table, whereSql, dbHandle);
//                                     url  token  table   predicate  SQLite handle|null

sock.write("tasks", "upsert", "1", JSON.stringify({ title: "buy milk" }), "cw1");
//      table     op       pk   payload_json (string|null)            client_write_id

sock.onChange(() => { /* change tick — re-read rowsFor */ });
const rows = sock.rowsFor("tasks");    // [{ pk, payload }] — payload is Uint8Array
console.log(sock.checkpoint(), sock.rowCount());
sock.clearLocalState();                 // ADR-0029 sign-out wipe
sock.close();
```

| Member | Notes |
|---|---|
| `NostosSocket.connect(url, token?, table, whereSql?, dbHandle?)` | **static**, returns `Promise<NostosSocket>`. `dbHandle` non-null = durable mode |
| `write(table, op, pk, payload_json?, client_write_id)` | Never throws on closed socket — captures via Outbox + apply_local. `op`: `"upsert"\|"delete"\|"patch"` |
| `rowsFor(table)` | `RowEntry[]` — each has `.pk` (string) and `.payload` (Uint8Array) |
| `onChange(callback)` / `offChange()` | Reactive push (ADR-0024) — fires on every commit |
| `checkpoint` | durable LSN as a JS number (getter) |
| `rowCount` | applied row count (getter) |
| `clearLocalState()` | ADR-0029: wipe rows + outbox (half a clear is a leak) |
| `close()` | Close the socket (code 1000) |

The token goes on the URL as `?token=` because **browsers cannot set headers on a WebSocket
handshake**.

## Typed verbs on `NostosEngine` (ADR-0035 / Wave 4a)

The wasm bridge now exposes the full Tier-1 typed surface from ADR-0032, ported
(not wired) from the native `SyncClient` — which is tokio-based and unreachable
from wasm. Each verb is thin orchestration on the in-process `ApplyEngine`, and
the CRDT invariants (`Hlc`, `OrSetElement`, counter merge) come from
`nostos-domain` directly — **not** re-implemented. See ADR-0035 for the
port-not-wire decision and the three `SqliteWasmStorage` overrides that mirror
native `SqliteStorage` (transactional `enqueue_batch`, dead-letter columns,
counter merge in `apply_local` + `read_payload`).

```js
import init, { NostosEngine } from "./pkg-web/nostos_ffi_wasm.js";
await init();
const eng = new NostosEngine();

// CRDT tables must be tagged BEFORE the first write so apply_local merges
// instead of clobbering (mirrors native SqliteStorage builder):
eng.setCrdtTables(["tags"], ["counters"]);

// Single write returns the outbox id (f64 at the JS boundary):
const id = eng.write("tasks", "upsert", "1", JSON.stringify({ title: "buy milk" }));

// Atomic batch — mid-batch failure rolls back the entire outbox insert:
const ids = eng.writeBatch([
  { table: "tasks", op: "upsert", pk: "1", payloadJson: '{"title":"a"}' },
  { table: "tasks", op: "upsert", pk: "2", payloadJson: '{"title":"b"}' },
]);

// CRDT verbs — mint HLC internally, merge element-wise (ADR-0030):
eng.orSetAdd("tags", "row1", "red");
eng.orSetRemove("tags", "row1", "red");
eng.counterIncrement("counters", "c1", 5);
eng.counterDecrement("counters", "c1", 2);

// Read engine primitives:
eng.applySchema([{ name: "tasks", columns: ["id", "title"] }]);  // SqliteWasm only; Memory no-op
const rows = JSON.parse(eng.query("SELECT * FROM tasks"));       // SqliteWasm only; Memory → []

// Outbox diagnostics for watchWriteStatus (ADR-0027):
eng.pendingCount();       // non-dead-lettered writes
eng.deadLetteredCount();  // writes marked dlq=1
eng.lastError();         // last dead-letter error string, or null
```

| Member | Notes |
|---|---|
| `setCrdtTables(orSetTables, counterTables)` | Tag tables so `apply_local` merges (ADR-0030). Call before the first write to a CRDT table |
| `write(table, op, pk, payloadJson?)` | Returns outbox id (`number`). `op`: `"upsert"\|"delete"\|"patch"\|"increment"` |
| `writeBatch(ops[])` | Atomic — mid-batch failure rolls back. Returns ids in order |
| `orSetAdd(table, pk, element)` / `orSetRemove(table, pk, element)` | Mints HLC, merges element-wise |
| `counterIncrement(table, pk, delta)` / `counterDecrement(table, pk, delta)` | Mints HLC, merges per-replica max (PN-counter) |
| `applySchema(tables[])` | Materialize WS2 read-views. SqliteWasm only; Memory is a no-op |
| `query(sql)` | Returns JSON string of rows. SqliteWasm only; Memory returns `"[]"` |
| `pendingCount()` / `deadLetteredCount()` / `lastError()` | Outbox diagnostics for `watchWriteStatus` |

**Multi-table `subscribe` + `resume`** (on `NostosSocket`):

| Member | Notes |
|---|---|
| `subscribe(tables[], whereSql?)` | Sends a subscribe frame carrying the full table list (Worker batched push model) |
| `resume()` | Re-sends the subscribe frame with the persisted checkpoint if the socket is open; otherwise signals the caller to reconnect via `connect()` (engine state preserved in the dbHandle) |

**Testing boundary:** the host `cargo test -p nostos-ffi-wasm` suite (56 tests,
run in `make ci`) covers the typed-verb orchestration on the `Memory` backend,
CRDT-merge correctness, HLC monotonicity, and the subscribe/resume frame shapes.
The three `SqliteWasmStorage` overrides are browser-only code paths — a
Playwright harness is the open follow-up (ADR-0034).

## Node — `NostosClient` (apply engine only)

`index.js` exports `{ NostosClient, NostosEngine, Frame }` plus the T6 attachment surface
(`Attachments`, `SupabaseStorageAdapter`, `OpfsBlobStore`, `AttachmentConstants` — ADR-0034, see
[Attachments](#attachments--two-plane-blob-sync-t6--adr-0034) below). Typed in `index.d.ts`.

| Member | Signature |
|---|---|
| constructor | `new NostosClient(config?: { url?, token?, table? })` |
| `connect` | `connect(): Promise<NostosClient>` — **does not open a socket** |
| `subscribe` | `subscribe(table, whereSql?): NostosClient` — sets local intent, chainable |
| `write` | `write(table, pk, payload): WriteResult` — **sync**, feeds apply engine |
| `query` | `query(table): Row[]` — **sync**, per-table |
| `watch` | `watch(table, cb): () => void` — invokes `cb` immediately, returns unsubscribe |
| `signOut` | `signOut(): void` — ADR-0029: wipe rows + outbox + token |
| `setToken` | `setToken(token): void` — ADR-0029: cache for next connect |
| `checkpoint` / `rowCount` | readonly getters |
| `storageMode` | readonly getter — always `"memory"` in Node (no OPFS) |

## Attachments — two-plane blob sync (T6 / ADR-0034)

Mirrors the Flutter driver (`sdk/nostos_flutter/lib/src/attachments.dart`) over the **same** pure
state machine in `nostos-core` (`crates/nostos-core/src/attachments.rs`). Two planes:

- a **metadata plane** — an ordinary synced `attachments` table (`id, filename, size, media_type,
  state, timestamp`), replicated + outbox-driven like any business table; and
- a **blob plane** — a developer-supplied `AttachmentStorageAdapter` + a local blob cache.

Blobs **never transit the Nostos server** (moat constraint — would pollute fan-out throughput and
make the server stateful). `@supabase/supabase-js` is a **peer dep**: the module + tests load
without it; only `SupabaseStorageAdapter` construction pulls it in.

### Setup — `NOSTOS_WRITE_TABLES` (the #1 foot-gun)

The metadata table is writable through the collapsed outbox, so the server's empty-default
allowlist **MUST include `attachments`** (ADR-0013). A forgotten entry surfaces loudly:

```
table not writable: 'attachments' — add it to NOSTOS_WRITE_TABLES
```

```bash
export NOSTOS_WRITE_TABLES=attachments,tasks,…   # comma-separated; empty by default
```

### API (`attachments.js`)

```js
const { NostosClient } = require("@nostos-sync/web");
// or for the driver alone: require("@nostos-sync/web/attachments.js")
const { Attachments, SupabaseStorageAdapter, OpfsBlobStore, AttachmentConstants } = require("@nostos-sync/web");

const driver = new Attachments({
  gateway,                                            // see below — metadata-plane access
  adapter: new SupabaseStorageAdapter({ url, key, bucket: "uploads" }),
  blobStore: new OpfsBlobStore("nostos-blobs"),        // browser OPFS dir; throws in node
  isOnline: async () => navigator.onLine,
  maxAttempts: 5,                                     // → archived after
});

const id = await driver.queueUpload({ filename: "photo.png", bytes, mediaType: "image/png" });
await driver.pump();                                  // one tick: when online, dispatch blob ops
await driver.queueDownload(id);
await driver.remove(id);                              // queued_delete → archived
```

| Member | Signature | Notes |
|---|---|---|
| `queueUpload` | `Promise<string> queueUpload({filename, bytes, mediaType, id?})` | caches bytes locally, upserts a `queued_upload` row |
| `queueDownload` | `Promise<void> queueDownload(id)` | flips an existing synced row to `queued_download` |
| `remove` | `Promise<void> remove(id)` | flips to `queued_delete` |
| `pump` | `Promise<void> pump()` | one driver tick (when online) |
| `lastErrorFor` | `string|null lastErrorFor(id)` | last adapter error (dead-letter reason); local-only |

`AttachmentStorageAdapter` (`upload(path, bytes, mediaType)` / `download(path)` / `delete(path)`)
MUST be idempotent under retry. `SupabaseStorageAdapter` ships first-class (`upsert: true`; a
`not found` on delete is swallowed). `OpfsBlobStore` is the browser OPFS cache (builds on Wave-2
durable storage, ADR-0033; throws in node — pass an in-memory `BlobStore` in tests). The host
calls `blobStore.wipe()` on `signOut` (ADR-0029 parity).

The driver depends on a small `AttachmentMetadataGateway` interface (`queuedRows`, `patchState`,
`upsertRow`, `currentState`) so it is testable in node **without** the browser Worker / live
transport. In the browser, a Worker-backed gateway will wrap Wave-2's postMessage protocol once
the live write path lands.

State machine + ordering + dead-letter are identical to the Flutter driver — see the Flutter
[attachments](flutter.md#attachments--two-plane-blob-sync-t6--adr-0034) section and ADR-0034 for
the shared contract (weaker cross-row ordering; attempt count is driver-local, not synced).

## Ceilings

- **One table per socket** (single-table v1).
- **Node `NostosClient` has no live transport** — `connect()` does NOT open a WS. It drives the
  apply engine only. For Node live sync, use [`@nostos-sync/node`](node.md).
- **Memory mode (degrade path) is not reload-survivable** — when OPFS is unavailable (Safari Private
  Browsing, old browsers), the store is InMemoryStorage and everything is lost on reload. This is
  the documented degrade ceiling, surfaced on `SyncStatus.storageMode`.
- **Payloads are bytes both directions** — you own encode/decode. The browser `write` takes a JSON
  string (`payload_json`); the apply engine stores opaque bytes.

## Proven by

| Spec | What it proves |
|---|---|
| `e2e/browser_live.spec.cjs` | PUSH + async write + ECHO round-trip via the Worker, under Playwright headless Chromium |
| `e2e/durable.spec.cjs` | ADR-0033: write survives page reload in durable mode; signOut wipes the OPFS store; degrade path reported correctly |
| `e2e/worker.spec.cjs` | The Worker boots, responds to ping, and surfaces the postMessage protocol |
| `smoke.cjs` | The Node `NostosClient` facade (apply engine, no live transport) |
| `e2e/attachments.spec.cjs` | ADR-0034: the T6 attachment driver — queue→reconnect→upload→second-client-download→dead-letter→wipe, against an in-memory fake adapter (no bucket). Same state machine as the Flutter suite. **Real Supabase-Storage round-trip is untested-environment** (no project configured). |

`FileSystemSyncAccessHandle` (the `opfs-sahpool` primitive) is browser+Worker-only — it does not
exist in Node. The durable-storage spec MUST run in a real browser (Playwright/headless Chromium).
