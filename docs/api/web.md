# Web — `@nostos-sync/web`

Extracted from `sdk/nostos_web/index.js`, `index.d.ts`, and `crates/nostos-ffi-wasm/src/lib.rs` on
2026-07-30. Index: [`README.md`](README.md).

**This package has two different APIs and only one of them syncs.** Getting them mixed up is the
single most likely mistake here.

| Path | Entry | Live WebSocket? | Use for |
|---|---|---|---|
| **Browser** | `pkg-web/nostos_ffi_wasm.js` → `NostosSocket` | **Yes** — the browser's real `WebSocket` | shipping a web app |
| **Node** | `index.js` → `NostosClient` | **No** — apply engine only | tests, replaying frames, server-side decode |

If you want a synced browser app, use `NostosSocket`. If you are in Node and want real sync, you
want [`@nostos-sync/node`](node.md) instead.

## Browser — `NostosSocket` (the live path)

A `wasm_bindgen` export from `crates/nostos-ffi-wasm/src/lib.rs:398`.

```js
import init, { NostosSocket } from "./pkg-web/nostos_ffi_wasm.js";
await init();

const sock = await NostosSocket.connect("ws://127.0.0.1:8080/sync", null, "tasks", null);
//                                     url,  token, table,   whereSql
sock.write("tasks", "1", new TextEncoder().encode(JSON.stringify({ title: "buy milk" })));
const rows = sock.rowsFor("tasks");          // [{ pk, payload }]
console.log(sock.checkpoint(), sock.rowCount());
```

| Member | Notes |
|---|---|
| `NostosSocket.connect(url, token, table, whereSql)` | **static**, returns a `Promise<NostosSocket>`. Opens the socket *and* subscribes |
| `write(table, pk, payload)` | payload is bytes |
| `rowsFor(table)` | `RowEntry[]` — each has `pk()` and `payload()` (bytes) |
| `checkpoint()` | durable LSN as a JS number |
| `rowCount()` | applied row count |

The token goes on the URL as `?token=` because **browsers cannot set headers on a WebSocket
handshake**. `resume_lsn` is read from `localStorage["cairn:checkpoint:<table>"]`, defaulting to 0 —
so a reload resumes rather than refetching.

## Node — `NostosClient` (apply engine only)

`index.js:184` exports `{ NostosClient, NostosEngine, Frame }`. Typed in `index.d.ts`:

| Member | Signature |
|---|---|
| constructor | `new NostosClient(config?: { url?, token?, table? })` |
| `connect` | `connect(): Promise<NostosClient>` — **does not open a socket** |
| `subscribe` | `subscribe(table, whereSql?): NostosClient` — sets local intent, chainable |
| `write` | `write(table, pk, payload: Uint8Array \| number[]): WriteResult` — **sync** |
| `query` | `query(table): Row[]` — **sync**, per-table, not SQL |
| `watch` | `watch(table, cb): () => void` — invokes `cb` immediately, returns an unsubscribe fn |
| `checkpoint` / `rowCount` | readonly getters |

`Row` is `{ pk: string, payload: Buffer }`; `WriteResult` is `{ checkpoint, rowsApplied }`.

Lower-level, also exported: `NostosEngine` (`newEngine`/`setWhereSql`/`flush`/`rowsFor`/`checkpoint`/
`rowCount`) and `Frame`, if you want to drive the apply path frame by frame.

## Reads are a KV store, not SQL

Unlike every SQLite-backed SDK here, the WASM apply engine keeps an **in-memory key-value store**.
There is no `cairn_data` table and no SQL: you call `rowsFor(table)` / `query(table)` and get
`{pk, payload}` pairs with the payload as **bytes you decode yourself**. Nothing persists across a
reload except the `localStorage` checkpoint.

## Ceilings

- One table per socket.
- No reactive stream in the browser path — poll `rowsFor` after writes, or wrap the pump yourself.
- Payloads are bytes both directions; you own encode/decode.

## Proven by

Two slices. `web` runs `e2e/browser_live.spec.cjs` under Playwright: a real browser, a real
`WebSocket`, full PUSH + ECHO against the Rust spine. `smoke.cjs` covers the Node facade and
**explicitly does not** exercise `NostosSocket.connect()`. A latent flush bug lived here once — the
WASM `onmessage` pump never flushed standalone frames — fixed with an unconditional
`engine.flush()` mirroring the native client's per-batch commit.
