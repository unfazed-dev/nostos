// Copyright (c) Nostos contributors
// SPDX-License-Identifier: Apache-2.0
//
// Public API surface of the @nostos-sync/capacitor plugin.
//
// This is a web-only Capacitor v8 plugin: the WKWebView (iOS) and Android
// WebView are full browser engines, so the existing @nostos-sync/web live browser
// path (the wasm-pack `--target web` build of nostos-ffi-wasm + the
// `NostosSocket` web-sys transport) runs unmodified. There is NO native
// `android/` or `ios/` source in this package — the `web` implementation
// registered in `src/index.ts` is the only implementation, and it does all
// the work in the browser engine the webview already is.
//
// The shapes below mirror @nostos-sync/web's public API (connect / subscribe /
// query / write / watch / checkpoint). Storage today is the wasm engine's
// in-memory KV (matches @nostos-sync/web's current bar — see ADR-0017 for the
// OPFS / @capacitor-community/sqlite durable upgrade path).

/**
 * Options passed to {@link NostosPlugin.connect}.
 *
 * `url` is the live WS sync endpoint (e.g. `wss://sync.example.com/sync`).
 * `token` is appended as `?token=` on the WS handshake (browsers cannot set
 * headers on a WS upgrade — same convention as the native SyncClient).
 * `table` is the table to subscribe; `whereSql` is the optional safe-SQL
 * predicate (cleared if empty/null).
 */
export interface ConnectOptions {
  url: string;
  token?: string | null;
  table?: string;
  whereSql?: string | null;
}

/**
 * Result of {@link NostosPlugin.connect}. `rowCount` is the number of rows
 * the engine currently holds for the subscribed table; `checkpoint` is the
 * durable LSN persisted to localStorage (the resume point on reconnect).
 */
export interface NostosConnectResult {
  rowCount: number;
  checkpoint: number;
}

/**
 * Options passed to {@link NostosPlugin.write}. The server's echo `WriteBack`
 * re-emits the row through the fan-out, so the writer receives its own write
 * back on the same socket — the round-trip every SDK E2E proves.
 *
 * `payload` (object) and `payloadJson` (pre-stringified) are alternatives;
 * if both are set, `payloadJson` wins. `clientWriteId` is the caller's
 * correlation id, echoed in the server's `write_result` ack.
 */
export interface WriteOptions {
  table: string;
  op: string;
  pk: string;
  payload?: unknown;
  payloadJson?: string;
  clientWriteId: string;
}

/** Options passed to {@link NostosPlugin.query}. */
export interface QueryOptions {
  table: string;
}

/**
 * A row in the engine's in-memory KV. `pk` is the primary key; `payload` is
 * the parsed JSON payload (decoded from the wire bytes by the wasm engine).
 */
export interface NostosRow {
  pk: string;
  payload: unknown;
}

/** Options passed to {@link NostosPlugin.watch}. */
export interface WatchOptions {
  table: string;
}

/**
 * A reactive snapshot delivered to a {@link NostosPlugin.watch} listener.
 *
 * `kind` is `"initial"` for the first emit — the engine's current rows for the
 * table, delivered as soon as the listener is registered — and `"delta"` for
 * every subsequent emit, after an inbound commit changes the table's rows.
 */
export interface NostosWatchSnapshot {
  /** `"initial"` = first emit with current state; `"delta"` = a post-commit emit. */
  kind: "initial" | "delta";
  /** The table these rows belong to. */
  table: string;
  /** The engine's current rows for `table` at emit time. */
  rows: NostosRow[];
}

/**
 * Handle returned by {@link NostosPlugin.watch}. Call `unsubscribe()` to stop
 * further emits for this listener; idempotent.
 */
export interface NostosWatchSubscription {
  unsubscribe(): void;
}

/** Options for {@link NostosPlugin.configure}. */
export interface ConfigureOptions {
  /**
   * Absolute or document-relative URL to the wasm-pack `--target web` glue
   * module (`nostos_ffi_wasm.js`), which in turn fetches its sibling
   * `nostos_ffi_wasm_bg.wasm`. Defaults to `/pkg-web/nostos_ffi_wasm.js` —
   * hosts that bundle the plugin should set this to the bundled asset URL.
   */
  wasmUrl: string;
}

/**
 * The Nostos Capacitor plugin.
 *
 * All methods are async because they cross the JS↔wasm↔WS boundary. Connect
 * must be called before any of write / query / checkpoint / subscribe.
 */
export interface NostosPlugin {
  /**
   * Load the wasm engine (if not already loaded) and open the live WS sync
   * session. Resolves once the browser's WebSocket `open` has fired and the
   * subscribe frame is queued.
   */
  connect(options: ConnectOptions): Promise<NostosConnectResult>;

  /**
   * No-op for the v0.1 web path: subscribe is folded into connect (the wasm
   * NostosSocket.subscribe is a single-session affordance). Real multi-table
   * subscribe arrives with ADR-0017 durable storage. Kept on the interface
   * for source-compat with the native plugin shape.
   */
  subscribe(options: {
    table: string;
    whereSql?: string | null;
  }): Promise<void>;

  /** Send a client write. See {@link WriteOptions}. */
  write(options: WriteOptions): Promise<void>;

  /** Read the rows the engine currently holds for `table`. */
  query(options: QueryOptions): Promise<{ rows: NostosRow[] }>;

  /**
   * Subscribe to a table's changing row set.
   *
   * The `listener` fires immediately with the engine's current rows
   * (`kind: "initial"`), then again with `kind: "delta"` after each inbound
   * commit that changes the table. Returns a subscription whose
   * `unsubscribe()` stops further emits (idempotent).
   *
   * ponytail: the `delta` path requires a JS-facing change-callback seam on the
   * wasm `NostosSocket` (Rust `on_change`, landing as JS `onChange` or similar)
   * that does not yet exist in the shipped `pkg-web` build — the socket's only
   * callbacks today are its internal WS handlers (`on_open`/`on_message`/...).
   * The web-SDK port has the seam in flight. Until it lands, `watch()` delivers
   * the initial snapshot only — the same bar `@nostos-sync/web`'s own `watch()` sets.
   * The delta hook probes the candidate seam names and lights up the moment the
   * seam is present, with no API change to callers. This is NOT polling — emits
   * are event-driven by the change callback, never a timer.
   */
  watch(
    options: WatchOptions,
    listener: (snapshot: NostosWatchSnapshot) => void,
  ): Promise<NostosWatchSubscription>;

  /** Read the durable checkpoint (the LSN to resume from on reconnect). */
  checkpoint(): Promise<{ checkpoint: number }>;

  /** Read the live row count the engine holds for the subscribed table. */
  rowCount(): Promise<{ rowCount: number }>;

  /** Close the socket. The server treats this as a session end. */
  close(): Promise<void>;

  /**
   * Override the wasm glue URL. Must be called before connect. Hosts that
   * bundle the plugin should point this at the bundled asset.
   */
  configure(options: ConfigureOptions): Promise<void>;
}
