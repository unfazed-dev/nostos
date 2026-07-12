# @nostos-sync/capacitor

A **web-only** [Capacitor v8](https://capacitorjs.com/) plugin that re-exports
[`@nostos-sync/web`][web]'s live browser sync path into the Capacitor plugin shape.
There is **no native `android/` or `ios/` source** in this package — the iOS
WKWebView and the Android WebView are full browser engines, so the WASM build
of `nostos-ffi-wasm` and the `WebSocket` API both run unmodified inside the
webview. The plugin's single `web` implementation is the only implementation.

[web]: ../nostos_web

## What it does

- Loads the wasm-pack `--target web` build of `nostos-ffi-wasm` (`@nostos-sync/web`'s
  `pkg-web/nostos_ffi_wasm.js`) inside the webview.
- Drives the wasm `NostosSocket` — a live WebSocket sync session built on
  `web_sys::WebSocket` + `Window::localStorage`. The socket subscribes to a
  server-side table, applies inbound frames through the pure apply-engine,
  ACKs per committed batch, and persists the resume LSN to localStorage so a
  reload resumes from where it left off.
- Exposes the PowerSync-shaped API in the Capacitor plugin convention:
  `connect`, `subscribe`, `write`, `query`, `checkpoint`, `rowCount`,
  `close`, `configure`.

This mirrors the wiring proven by
[`sdk/nostos_web/e2e/browser_live.spec.cjs`](../nostos_web/e2e/browser_live.spec.cjs),
which drives the same two-direction PUSH + ECHO round-trip against the shared
SDK E2E spine (`crates/nostos-infra/examples/e2e_server.rs`).

## Install

```bash
npm install @nostos-sync/capacitor
```

`@capacitor/core` v8 is a peer dependency. `@nostos-sync/web` is a regular
dependency (linked via `file:../nostos_web` inside this monorepo; on publish it
becomes a normal version constraint).

## Usage

```ts
import { Nostos } from "@nostos-sync/capacitor";

// Point at the wasm glue asset. In a bundled Capacitor app, this is the
// bundled asset URL (e.g. "assets/nostos_ffi_wasm.js"). The default
// "/pkg-web/nostos_ffi_wasm.js" suits dev servers that serve the asset at
// that path.
await Nostos.configure({ wasmUrl: "assets/nostos_ffi_wasm.js" });

// Open the live WS sync session and subscribe to "tasks".
const { rowCount, checkpoint } = await Nostos.connect({
  url: "wss://sync.example.com/sync",
  token: "<auth-token>",
  table: "tasks",
  whereSql: "priority > 5",
});

// Server pushes flow into the wasm engine. Read them back:
const { rows } = await Nostos.query({ table: "tasks" });

// Client writes are echoed back through the server's fan-out so the writer
// sees its own write on the same socket.
await Nostos.write({
  table: "tasks",
  op: "upsert",
  pk: "t1",
  payload: { title: "hello", status: "open", priority: 5 },
  clientWriteId: "w1",
});

await Nostos.close();
```

## Storage (ceiling + upgrade path)

Today the wasm engine holds applied rows in an in-memory KV — the same bar
`@nostos-sync/web` sets. Only the checkpoint (LSN) survives a reload; a reconnect
replays from that LSN. **Production storage arrives with
`@capacitor-community/sqlite`** (the upgrade path tracked in ADR-0017): the
wasm `Storage` trait will be implemented against the SQLite plugin so rows
persist across launches. Until then, treat this plugin as a live-replication
proof, not a durable store.

## Why no native source?

Capacitor's iOS WKWebView origin is `capacitor://localhost` and Android's is
`http://localhost`; both serve a full browser engine. WASM instantiation and
`WebSocket` work identically to a desktop browser, so reusing the existing
`@nostos-sync/web` browser path is strictly simpler than a native `echo`/`toast`
plugin that just bounces calls over the bridge. The bridge exists for things
the browser engine cannot do (keychain, biometrics, file system, sqlite) —
none of which the v0.1 sync plugin needs.

## Verify

The example app under `example-app/` has a Playwright E2E
(`example-app/e2e/push-echo.spec.cjs`) that spawns the SDK E2E spine, opens
the example page in a headless browser, and proves PUSH + ECHO. Run:

```bash
cd sdk/nostos_capacitor
npm install
npm run build         # tsc → dist/
cd example-app
npm install           # playwright + capacitor-core already hoisted by parent
npx playwright test --config=playwright.config.cjs
```

Success = the run prints `[cap-e2e] PUSH_OK` and `[cap-e2e] ECHO_OK` and
exits 0. The spine binary must exist at
`target/debug/examples/e2e_server` (build it with
`cargo build -p nostos-infra --examples`).

## License

Apache-2.0, end to end.
