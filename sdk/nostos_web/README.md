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

## Ceiling (ponytail)

**The remaining gap is Node-only.** `NostosSocket.connect()` is wired to
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
