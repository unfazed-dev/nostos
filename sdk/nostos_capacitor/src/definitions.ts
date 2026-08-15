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
//
// BETA (plan task 6.3 / ADR-0037 §2): the push surface is the one part with
// native source — a minimal bridge that obtains the OS push token and emits
// foreground-push events to JS. The REST half (registerPushToken /
// deregisterPushToken) stays in the webview implementation, riding the SAME
// base URL + Bearer the sync session uses (one credential source; the server
// stamps tenant/account itself — ADR-0018 discipline). Doorbell semantics:
// push is a hint, sync is the transport; row data never transits
// Apple/Google servers.

import type { Plugin, PluginListenerHandle } from "@capacitor/core";

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

/** Options for {@link NostosPlugin.setToken}. */
export interface SetTokenOptions {
  /**
   * The new auth token (JWT), or `null`/omitted to clear it. Swapped into the
   * token used by the next {@link NostosPlugin.connect} — an already-open socket
   * is NOT hot-swapped, because the wasm `NostosSocket` binds the token at
   * connect time (browsers cannot re-handshake an open WebSocket). This mirrors
   * the native `SyncClient::set_token`. Call {@link NostosPlugin.signOut} to
   * tear down a live session and clear the token together.
   */
  token?: string | null;
}

/**
 * The Nostos Capacitor plugin.
 *
 * All methods are async because they cross the JS↔wasm↔WS boundary. Connect
 * must be called before any of write / query / checkpoint / subscribe.
 *
 * The push methods are **beta** (plan task 6.3, ADR-0037): the REST pair is
 * implemented in the web implementation (the webview's `fetch`), while
 * `registerForPushNotifications` dispatches to the native sides (iOS only —
 * on Android FCM acquisition stays app-side; see the README push section).
 */
export interface NostosPlugin extends Plugin {
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
   * Swap the auth token used by the next {@link connect}. Does NOT hot-swap an
   * open socket — the wasm `NostosSocket` binds the token at connect (a browser
   * cannot re-handshake an open WebSocket), so a refreshed JWT takes effect on
   * the next {@link connect}. Mirrors the native `SyncClient::set_token`. Use
   * {@link signOut} to clear the token AND tear down the live session together.
   */
  setToken(options: SetTokenOptions): Promise<void>;

  /**
   * Sign out the current user (ADR-0029): deregister every session-registered
   * push token (ADR-0037 §3 — while the prior principal's JWT is still held),
   * wipe the engine's in-memory rows AND the pending outbox, close the live
   * socket, drop every reactive listener, and clear the stored token. After
   * this the plugin holds none of the prior user's state, so the next
   * {@link connect} (a new principal) cold-starts into an empty database
   * rather than the previous user's rows. Idempotent — safe to call when not
   * connected.
   */
  signOut(): Promise<void>;

  /**
   * Override the wasm glue URL. Must be called before connect. Hosts that
   * bundle the plugin should point this at the bundled asset.
   */
  configure(options: ConfigureOptions): Promise<void>;

  // ─────────────────── Push notifications (beta, ADR-0037) ───────────────────
  //
  // Doorbell semantics: push is a hint, sync is the transport. Registering a
  // token only tells the server WHERE to knock; data always arrives over the
  // sync connection, which resumes from the durable LSN checkpoint
  // (reconnect(), or a cold connect after a killed app). Row data never
  // transits Apple/Google servers.

  /**
   * Register this device's push token with the server (ADR-0037 §3):
   * `POST /push-tokens` with `{"platform": …, "token": …}`, authenticated by
   * the SAME token the sync session uses (`Authorization: Bearer` — the one
   * passed to {@link connect} or {@link setToken}; there is no second
   * credential source). The HTTP base is derived from the sync `url`
   * (`wss`→`https`, `ws`→`http`, path stripped) — the same derivation the
   * Flutter/Node SDKs use. The server stamps tenant/account itself.
   *
   * `platform` is `"apns"`, `"fcm"`, or `"webpush"`. Beta: implemented in the
   * web implementation (the webview's `fetch`) — on iOS/Android call it on
   * the {@link NostosWeb} instance you sync with, not through the bridge
   * proxy. Registered tokens are deregistered best-effort by {@link signOut}.
   */
  registerPushToken(platform: PushPlatform, token: string): Promise<void>;

  /**
   * Deregister `token` (ADR-0037 §3): `DELETE /push-tokens/{token}` (URL-
   * encoded) with the same auth as {@link registerPushToken}. Call when the
   * app can no longer receive on this token (e.g. notifications disabled);
   * {@link signOut} deregisters every session-registered token anyway.
   */
  deregisterPushToken(token: string): Promise<void>;

  /**
   * Ask the OS for this platform's push token (beta). iOS: registers with
   * APNs — no permission prompt (silent pushes need only registration; the
   * visible-notification authorization is app-side, see the README) — and the
   * hex device token arrives as a `pushToken` event; failure arrives as
   * `pushTokenError`. Android: `unimplemented` — FCM token acquisition stays
   * app-side (the plugin does not vendor Firebase); forward the token from
   * your `FirebaseMessagingService` and call `registerPushToken("fcm", …)`.
   * Web: rejects — web push is the Service Worker work (plan task 6.2).
   *
   * ponytail: skeleton bridge — no silent-background wake exists on any side.
   * A Capacitor app follows the native OS rules (iOS silent-push budgets,
   * force-quit discard, Doze); the honest wake path is: OS shows the
   * notification / budget-permits the silent push → app foregrounds or
   * reconnects → `reconnect()`/`connect()` resumes from the checkpoint.
   */
  registerForPushNotifications(): Promise<void>;

  /**
   * Close the live socket (if any) and re-open it with the last
   * {@link connect} options — the wake path a `foregroundPush` listener
   * triggers. The wasm socket resumes from the durable localStorage
   * checkpoint, so nothing is lost while it was down.
   *
   * ponytail: `close()` drops reactive listeners, so `watch()` subscriptions
   * must be re-registered after a reconnect (same as after any `close()`).
   * Re-arming them automatically arrives with the same wasm change-callback
   * seam the delta path needs (see {@link watch}).
   */
  reconnect(): Promise<NostosConnectResult>;

  /** `pushToken` event: the OS push token, as handed over by the native side. */
  addListener(
    eventName: "pushToken",
    listenerFunc: (event: NostosPushTokenEvent) => void,
  ): Promise<PluginListenerHandle>;

  /** `pushTokenError` event: APNs registration failed (iOS). */
  addListener(
    eventName: "pushTokenError",
    listenerFunc: (event: { message: string }) => void,
  ): Promise<PluginListenerHandle>;

  /**
   * `foregroundPush` event: a push arrived while the app was foregrounded.
   * iOS: emitted from `userNotificationCenter(_:willPresent:)` for
   * user-visible notifications (data-only foreground delivery is not
   * bridged — silent pushes are the OS's business). Android: emitted only if
   * the app's own `FirebaseMessagingService` calls
   * `NostosPlugin.emitForegroundPush(...)` (Firebase stays app-side).
   *
   * The typical handler triggers {@link reconnect} — or just re-reads state,
   * since a foregrounded live socket has already applied the data.
   */
  addListener(
    eventName: "foregroundPush",
    listenerFunc: (event: NostosForegroundPushEvent) => void,
  ): Promise<PluginListenerHandle>;

  /** Untyped fallback (from `Plugin`) for any other event name. */
  addListener(
    eventName: string,
    listenerFunc: (...args: any[]) => any,
  ): Promise<PluginListenerHandle>;
}

/** The platforms `POST /push-tokens` accepts (ADR-0037 §3). */
export type PushPlatform = "apns" | "fcm" | "webpush";

/** Payload of the `pushToken` event (iOS native side). */
export interface NostosPushTokenEvent {
  /** Always `"apns"` today — Android FCM tokens are app-side. */
  platform: PushPlatform;
  /** The hex APNs device token. */
  token: string;
}

/** Payload of the `foregroundPush` event. */
export interface NostosForegroundPushEvent {
  /**
   * The notification payload as the platform hands it over (iOS
   * `userInfo`-shaped object; Android the data map the app forwarded). No
   * schema is imposed — doorbell semantics mean the app syncs, not parses.
   */
  payload: unknown;
}
