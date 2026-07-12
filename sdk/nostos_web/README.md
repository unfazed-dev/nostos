# @nostos-sync/web

PowerSync-style JS facade over the `nostos-ffi-wasm` apply engine.

**Status: reduced-scope feasibility proof.** Loads the wasm in Node 22+ and drives the in-memory apply engine (`NostosEngine`, `Frame`, `Outcome`). Does NOT yet open a live WebSocket — see "Ceiling" below.

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

## API (PowerSync-shaped)

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

`NostosSocket.connect()` — the live browser WS transport (E1) — is wired to `web-sys::WebSocket` + `Window::localStorage`, which node lacks. This package intentionally does not call it. Upgrade paths:

1. **Node WS adapter** — a thin Rust module that replaces the web-sys transport at the `NostosSocket` seam, gated behind `#[cfg(feature = "node-transport")]`.
2. **Browser build** — `wasm-pack build --target web` + a bundler (vite/webpack) where WebSocket + localStorage are native. A vitest browser-env test replaces this node smoke.

## What this proves

Nostos's wasm apply engine loads and runs in Node 22 via `require()`, with a PowerSync-shaped JS surface on top — moving Nostos from 3/10 to 5/10 platform coverage.
