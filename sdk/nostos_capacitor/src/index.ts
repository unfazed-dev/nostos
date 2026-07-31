// Copyright (c) Nostos contributors
// SPDX-License-Identifier: Apache-2.0
//
// @nostos-sync/capacitor — a Capacitor v8 web-only plugin that re-exports
// @nostos-sync/web's live browser sync path. The mobile webview (iOS WKWebView,
// Android WebView) is a full browser engine, so the wasm NostosSocket
// (web-sys::WebSocket + Window::localStorage) runs unmodified — there is NO
// native android/ or ios/ source in this package. The `web` implementation
// registered below does all the work.
//
// Usage in a Capacitor app:
//
//   import { Nostos } from "@nostos-sync/capacitor";
//   await Nostos.configure({ wasmUrl: "assets/nostos_ffi_wasm.js" });
//   await Nostos.connect({ url: "wss://sync.example.com/sync", table: "tasks" });
//   await Nostos.write({ table: "tasks", op: "upsert", pk: "t1",
//                       payload: { title: "hi" }, clientWriteId: "w1" });
//   const { rows } = await Nostos.query({ table: "tasks" });

import { registerPlugin } from "@capacitor/core";

import type { NostosPlugin } from "./definitions";

export type {
  NostosConnectResult,
  NostosPlugin,
  NostosRow,
  NostosWatchSnapshot,
  NostosWatchSubscription,
  ConfigureOptions,
  ConnectOptions,
  QueryOptions,
  WatchOptions,
  WriteOptions,
} from "./definitions";
export { NostosWeb } from "./web";

/**
 * The Nostos plugin handle. Calling any method on this proxy dispatches to the
 * registered web implementation (the only implementation in this package).
 * The lazy `web` loader lets bundlers code-split the wasm glue.
 */
export const Nostos = registerPlugin<NostosPlugin>("Nostos", {
  web: () => import("./web").then((m) => new m.NostosWeb()),
});
