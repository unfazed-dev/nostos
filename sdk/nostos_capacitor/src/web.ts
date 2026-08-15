// Copyright (c) Nostos contributors
// SPDX-License-Identifier: Apache-2.0
//
// Web implementation of the @nostos-sync/capacitor plugin.
//
// This is the substantive file. It does NOT re-export @nostos-sync/web's `index.js`
// (which is the node apply-engine-only facade — see the file header there for
// why: it loads the `--target nodejs` wasm build and deliberately does NOT
// open a WebSocket, because web-sys::WebSocket + Window::localStorage do not
// exist in Node without a polyfill).
//
// Instead, this file mirrors the wiring in
// `sdk/nostos_web/e2e/browser_live.spec.cjs`: it dynamically loads the
// `--target web` wasm glue (`@nostos-sync/web`'s `pkg-web/nostos_ffi_wasm.js`), runs
// its default export to instantiate the wasm, then drives the `NostosSocket`
// class — a live WebSocket sync session built on web-sys::WebSocket +
// Window::localStorage. In a Capacitor webview (iOS WKWebView or Android
// WebView) both of those browser globals exist and behave exactly as in a
// desktop browser, so the live browser path runs unmodified.
//
// The wasm URL is configurable via {@link NostosWeb.configure}. The default
// `/pkg-web/nostos_ffi_wasm.js` is correct when the host serves the asset at
// that document-absolute path (the example app and Playwright E2E do this;
// bundled Capacitor apps override it to the bundled asset URL).

import { WebPlugin } from "@capacitor/core";
import type {
  NostosConnectResult,
  NostosPlugin,
  NostosRow,
  NostosWatchSnapshot,
  NostosWatchSubscription,
  ConfigureOptions,
  ConnectOptions,
  QueryOptions,
  SetTokenOptions,
  WatchOptions,
  WriteOptions,
} from "./definitions";

/** The platforms `POST /push-tokens` accepts (ADR-0037 §3). */
const PUSH_PLATFORMS = new Set(["apns", "fcm", "webpush"]);

/** Default URL of the wasm-pack `--target web` glue, served by the host. */
const DEFAULT_WASM_URL = "/pkg-web/nostos_ffi_wasm.js";

/**
 * Shape of the dynamically-imported wasm-pack `--target web` module. Kept as a
 * structural type (no runtime import — the URL is resolved at first use) so
 * the host can serve the asset from any path.
 */
interface WasmWebModule {
  /** wasm-pack's init; returns a Promise that resolves once wasm is ready. */
  default(): Promise<unknown>;
  /** The live WS sync session class. See pkg-web/nostos_ffi_wasm.d.ts. */
  NostosSocket: NostosSocketCtor;
}

/** Constructor shape for the wasm NostosSocket class. */
interface NostosSocketCtor {
  connect(
    url: string,
    token: string | null | undefined,
    table: string,
    whereSql?: string | null,
  ): Promise<NostosSocketInstance>;
}

/** Instance shape for the wasm NostosSocket. */
interface NostosSocketInstance {
  readonly rowCount: number;
  readonly checkpoint: number;
  rowsFor(table: string): Array<{ pk: string; payload: unknown }>;
  write(
    table: string,
    op: string,
    pk: string,
    payloadJson: string | null | undefined,
    clientWriteId: string,
  ): void;
  close(): void;
  /**
   * ADR-0029 D1: wipe the engine's in-memory rows + outbox. Call before
   * close() on sign-out so the next principal sees an empty database.
   */
  clearLocalState(): void;
  // The wasm NostosSocket is gaining a JS-facing change-callback seam — a
  // no-arg "tick" the host re-reads `rowsFor` inside. The web-SDK port is
  // landing it as Rust `NostosSocket::on_change` (likely JS `onChange`); the
  // final JS name is not yet finalized. `attachSnapshotBridge` probes an
  // ordered candidate set so the delta path activates with no code change
  // here once the seam ships. See the ponytail on `NostosPlugin.watch`.
}

/**
 * Nostos's web-only Capacitor implementation. Delegates to the live browser
 * path supplied by @nostos-sync/web's pkg-web wasm build. The class is exported for
 * direct construction by hosts that want to skip `registerPlugin`'s lazy
 * loader; the normal path is `registerPlugin('Nostos', { web: ... })` in
 * `src/index.ts`.
 *
 * Storage today is the wasm engine's in-memory KV (the same bar @nostos-sync/web
 * sets). Durable storage arrives with @capacitor-community/sqlite (ADR-0017
 * upgrade path — see README).
 */
export class NostosWeb extends WebPlugin implements NostosPlugin {
  /** The active socket, set by connect, cleared by close/signOut. */
  private sock: NostosSocketInstance | null = null;

  /**
   * The auth token for the next connect(). Set by connect()/setToken(),
   * cleared by signOut(). The wasm NostosSocket binds the token at connect (a
   * browser cannot re-handshake an open WebSocket), so a setToken() swap takes
   * effect on the NEXT connect() — same shape as SyncClient::set_token.
   */
  private token: string | null = null;

  /**
   * Per-table reactive listeners, fanned out from the socket's single change
   * callback (when present). Each watch() call adds a listener here.
   */
  private watchers = new Map<
    string,
    Set<(snapshot: NostosWatchSnapshot) => void>
  >();

  /** True once the socket-level onSnapshot bridge has been attached. */
  private snapshotBridgeAttached = false;

  /** Resolves to the wasm NostosSocket ctor once init completes. */
  private wasmPromise: Promise<NostosSocketCtor> | null = null;

  /** Configured wasm URL (configure() overrides the default). */
  private wasmUrl: string = DEFAULT_WASM_URL;

  /**
   * The last ConnectOptions (minus the token, which lives in this.token) —
   * kept so reconnect() can re-open the same session after a push wake.
   */
  private lastConnect: ConnectOptions | null = null;

  /**
   * HTTP base derived from the last connect() url (`wss`→`https`, `ws`→
   * `http`, path stripped) — the REST surface (`POST /push-tokens`,
   * `DELETE /push-tokens/{token}`) hangs off the SAME server the WS session
   * uses. Same derivation as the Flutter/Node SDKs.
   */
  private httpBase: string | null = null;

  /**
   * Push tokens registered via registerPushToken THIS session, deregistered
   * by signOut() (ADR-0037 §3 — a leaked registration would push the previous
   * principal's data to the next user).
   *
   * ponytail: in-memory only — tokens registered before a webview reload are
   * not auto-deregistered (the set dies with the page). The stale case is
   * covered server-side: the rails prune on APNs 410 / FCM `UNREGISTERED`.
   * Upgrade path: persist the set alongside the checkpoint if rail-prune
   * proves too slow for real tenants.
   */
  private registeredPushTokens = new Set<string>();

  /** @inheritDoc */
  async configure(options: ConfigureOptions): Promise<void> {
    if (!options || !options.wasmUrl) {
      throw new Error("Nostos.configure: wasmUrl is required");
    }
    // If init already ran with a different URL, drop the cached promise so
    // the next connect() re-imports at the new URL.
    if (this.wasmUrl !== options.wasmUrl) {
      this.wasmPromise = null;
    }
    this.wasmUrl = options.wasmUrl;
  }

  /** @inheritDoc */
  async connect(options: ConnectOptions): Promise<NostosConnectResult> {
    if (!options || typeof options.url !== "string" || !options.url) {
      throw new Error("Nostos.connect: url is required");
    }
    const NostosSocket = await this.loadWasm();
    // A setToken() before connect() supplies the token for this handshake; an
    // explicit options.token wins. The wasm NostosSocket binds the token at
    // connect (browsers can't set headers on a WS handshake and can't
    // re-handshake an open one), so a later setToken() applies on the NEXT
    // connect — same shape as SyncClient::set_token.
    const token = options.token ?? this.token;
    this.token = token ?? null;
    this.lastConnect = options;
    this.httpBase = deriveHttpBase(options.url);
    this.sock = await NostosSocket.connect(
      options.url,
      token ?? null,
      options.table ?? "tasks",
      options.whereSql ?? null,
    );
    // New socket: the change-callback bridge must (re)attach on next watch().
    this.snapshotBridgeAttached = false;
    return {
      rowCount: this.sock.rowCount,
      checkpoint: this.sock.checkpoint,
    };
  }

  /** @inheritDoc */
  async subscribe(_options: {
    table: string;
    whereSql?: string | null;
  }): Promise<void> {
    // ponytail: subscribe is folded into connect for the v0.1 web path. The
    // wasm NostosSocket is a single-session affordance that subscribes during
    // connect. Real multi-table subscribe arrives with ADR-0017 durable
    // storage + a session manager; this no-op keeps the interface
    // source-compatible with the native plugin shape.
    if (!this.sock) {
      throw new Error("Nostos.subscribe: connect() not called");
    }
  }

  /** @inheritDoc */
  async write(options: WriteOptions): Promise<void> {
    if (!this.sock) {
      throw new Error("Nostos.write: connect() not called");
    }
    if (!options || !options.table || !options.op || !options.pk) {
      throw new Error("Nostos.write: table, op, pk are required");
    }
    const payloadJson =
      options.payloadJson ?? JSON.stringify(options.payload ?? null);
    this.sock.write(
      options.table,
      options.op,
      options.pk,
      payloadJson,
      options.clientWriteId,
    );
  }

  /** @inheritDoc */
  async query(options: QueryOptions): Promise<{ rows: NostosRow[] }> {
    if (!this.sock) {
      throw new Error("Nostos.query: connect() not called");
    }
    if (!options || !options.table) {
      throw new Error("Nostos.query: table is required");
    }
    const rows = this.sock.rowsFor(options.table) ?? [];
    return {
      rows: rows.map((r) => ({
        pk: r.pk,
        payload: r.payload,
      })),
    };
  }

  /** @inheritDoc */
  async watch(
    options: WatchOptions,
    listener: (snapshot: NostosWatchSnapshot) => void,
  ): Promise<NostosWatchSubscription> {
    if (!options || !options.table) {
      throw new Error("Nostos.watch: table is required");
    }
    if (typeof listener !== "function") {
      throw new Error("Nostos.watch: listener is required");
    }
    if (!this.sock) {
      throw new Error("Nostos.watch: connect() not called");
    }
    const table = options.table;
    let listeners = this.watchers.get(table);
    if (!listeners) {
      listeners = new Set();
      this.watchers.set(table, listeners);
    }
    listeners.add(listener);

    // Attach the socket-level change bridge once, if the wasm seam is present.
    this.attachSnapshotBridge();

    // Initial snapshot: the engine's current rows for the table. Fired before
    // the subscription resolves so the caller's first paint is the live state.
    this.deliver(listener, "initial", table);

    let unsubscribed = false;
    return {
      unsubscribe: (): void => {
        if (unsubscribed) return;
        unsubscribed = true;
        const set = this.watchers.get(table);
        if (!set) return;
        set.delete(listener);
        if (set.size === 0) {
          this.watchers.delete(table);
        }
      },
    };
  }

  /**
   * Read the engine's current rows for `table` as the public {@link NostosRow}
   * shape. Returns `[]` if the socket is gone (e.g. after close()).
   */
  private snapshotRows(table: string): NostosRow[] {
    const rows = this.sock?.rowsFor(table) ?? [];
    return rows.map((r) => ({ pk: r.pk, payload: r.payload }));
  }

  /**
   * Deliver a snapshot to a single listener, isolating a thrown listener from
   * the subscription and sibling listeners.
   */
  private deliver(
    listener: (snapshot: NostosWatchSnapshot) => void,
    kind: "initial" | "delta",
    table: string,
  ): void {
    try {
      listener({ kind, table, rows: this.snapshotRows(table) });
    } catch {
      // A listener throw must not break the subscription or other listeners.
    }
  }

  /**
   * Candidate JS-facing names of the wasm NostosSocket change-callback seam.
   * The web-SDK port is landing this as Rust `NostosSocket::on_change` (a no-arg
   * `Closure<dyn FnMut()>` "tick" → likely JS `onChange`). The final exposed
   * name is not yet finalized, so we probe an ordered set and the delta path
   * lights up regardless of which name ships. Each candidate is expected to
   * register a no-arg callback (the host then re-reads `rowsFor`), matching
   * port-web's `FnMut()` contract.
   */
  private static readonly SNAPSHOT_SEAM_NAMES = [
    "onChange",
    "setOnChange",
    "setOnSnapshot",
    "on_change",
  ] as const;

  /**
   * Register a single change-callback on the wasm NostosSocket, if it exposes a
   * change-tick seam (probed across {@link SNAPSHOT_SEAM_NAMES}). On each
   * commit, fan out a per-table `delta` snapshot to every registered listener.
   * Idempotent and reconnect-safe.
   *
   * ponytail: the seam is absent on today's pkg-web build (see NostosPlugin.watch
   * — port-web has it in flight as `on_change`); until it lands AND pkg-web is
   * rebuilt, this is a no-op and watch() delivers the initial snapshot only.
   * NOT polling — the fan-out fires only when the socket invokes the callback,
   * never on a timer.
   */
  private attachSnapshotBridge(): void {
    if (this.snapshotBridgeAttached || !this.sock) return;
    const sock = this.sock as unknown as Record<string, unknown>;
    const seamName = NostosWeb.SNAPSHOT_SEAM_NAMES.find(
      (n) => typeof sock[n] === "function",
    );
    if (!seamName) {
      // Seam not present on this wasm build — delta path stays dormant.
      return;
    }
    this.snapshotBridgeAttached = true;
    const register = sock[seamName] as (cb: () => void) => void;
    try {
      register((): void => {
        for (const [table, listeners] of this.watchers) {
          for (const listener of listeners) {
            this.deliver(listener, "delta", table);
          }
        }
      });
    } catch {
      // If registering throws on this build, fall back to initial-snapshot-only.
      this.snapshotBridgeAttached = false;
    }
  }

  /** @inheritDoc */
  async checkpoint(): Promise<{ checkpoint: number }> {
    if (!this.sock) {
      throw new Error("Nostos.checkpoint: connect() not called");
    }
    return { checkpoint: this.sock.checkpoint };
  }

  /** @inheritDoc */
  async rowCount(): Promise<{ rowCount: number }> {
    if (!this.sock) {
      throw new Error("Nostos.rowCount: connect() not called");
    }
    return { rowCount: this.sock.rowCount };
  }

  /** @inheritDoc */
  async close(): Promise<void> {
    const s = this.sock;
    this.sock = null;
    // Drop all reactive listeners and require the bridge to reattach on a
    // future connect(); the socket's change callback dies with the socket.
    this.watchers.clear();
    this.snapshotBridgeAttached = false;
    if (s) {
      try {
        s.close();
      } catch {
        // Already closed — fine.
      }
    }
  }

  /** @inheritDoc */
  async setToken(options: SetTokenOptions): Promise<void> {
    this.token = options?.token ?? null;
  }

  /** @inheritDoc */
  async signOut(): Promise<void> {
    // Order matters (ADR-0037 §3 + ADR-0029): deregister push tokens while
    // this.token still holds the prior principal's JWT (the DELETE needs the
    // SAME auth the registration used), THEN wipe the engine's rows + outbox
    // WHILE the socket is still live, close the socket, drop reactive
    // listeners and clear the stored token. After this the plugin holds none
    // of the prior principal's state. Idempotent — the token set drains.
    for (const token of Array.from(this.registeredPushTokens)) {
      try {
        await this.deregisterPushToken(token);
      } catch {
        // Swallowed: one failed DELETE must not block the rest of sign-out —
        // the stale row is pruned server-side (rail 410/UNREGISTERED).
      }
    }
    const s = this.sock;
    this.sock = null;
    this.watchers.clear();
    this.snapshotBridgeAttached = false;
    if (s) {
      try {
        s.clearLocalState();
      } catch {
        // Engine already gone — close() below still ends the session.
      }
      try {
        s.close();
      } catch {
        // Already closed — fine.
      }
    }
    this.token = null;
    this.lastConnect = null;
    this.httpBase = null;
  }

  // ─────────────────── Push (beta, ADR-0037 §2/§3) ───────────────────

  /** @inheritDoc */
  async registerPushToken(platform: string, token: string): Promise<void> {
    if (!PUSH_PLATFORMS.has(platform)) {
      throw new Error(
        `Nostos.registerPushToken: platform must be one of ${Array.from(PUSH_PLATFORMS).join(", ")} (got ${JSON.stringify(platform)})`,
      );
    }
    if (!token) {
      throw new Error("Nostos.registerPushToken: token must be non-empty");
    }
    await this.pushTokensRest("POST", "/push-tokens", {
      platform: platform,
      token: token,
    });
    this.registeredPushTokens.add(token);
  }

  /** @inheritDoc */
  async deregisterPushToken(token: string): Promise<void> {
    await this.pushTokensRest(
      "DELETE",
      `/push-tokens/${encodeURIComponent(token)}`,
    );
    this.registeredPushTokens.delete(token);
  }

  /** @inheritDoc */
  async registerForPushNotifications(): Promise<void> {
    throw new Error(
      "Nostos.registerForPushNotifications: web has no OS push — web push is the Service Worker work (plan task 6.2); on iOS/Android the native side handles it",
    );
  }

  /** @inheritDoc */
  async reconnect(): Promise<NostosConnectResult> {
    if (!this.lastConnect) {
      throw new Error("Nostos.reconnect: connect() not called yet");
    }
    await this.close();
    return this.connect(this.lastConnect);
  }

  /**
   * One REST round-trip against the pinned push-token contract (ADR-0037 §3).
   * Sends the JSON body for POST (none for DELETE) and this.token as a Bearer
   * header when one exists (a NOSTOS_SYNC_AUTH=none server has no token to
   * send, same as the WS handshake). Resolves on 204; anything else rejects
   * with status + body in the message.
   */
  private async pushTokensRest(
    method: "POST" | "DELETE",
    path: string,
    body?: Record<string, string>,
  ): Promise<void> {
    if (!this.httpBase) {
      throw new Error("Nostos.pushTokensRest: connect() not called");
    }
    const headers: Record<string, string> = {};
    if (body !== undefined) {
      headers["content-type"] = "application/json";
    }
    if (this.token) {
      headers.authorization = `Bearer ${this.token}`;
    }
    const response = await fetch(`${this.httpBase}${path}`, {
      method: method,
      headers: headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (response.status !== 204) {
      const text = await response.text().catch(() => "");
      throw new Error(
        `Nostos.pushTokensRest: ${method} ${path} failed with ${response.status}${text ? `: ${text}` : ""}`,
      );
    }
  }


  /**
   * Dynamically import the wasm-pack `--target web` glue and instantiate the
   * wasm. Cached on this.wasmPromise so repeat connect() calls share the same
   * module. Throws on init failure; the cached promise is dropped so a later
   * retry can re-attempt.
   */
  private loadWasm(): Promise<NostosSocketCtor> {
    if (this.wasmPromise) {
      return this.wasmPromise;
    }
    const url = this.wasmUrl;
    // Dynamic import of a URL resolved at runtime — browsers and Capacitor
    // webviews handle this natively (the parent document's import map or the
    // bundler's config resolve any bare specifier in the glue; the glue
    // itself is served as a URL). The cast through unknown is necessary
    // because TypeScript cannot resolve a non-literal module specifier.
    const promise = (import(url) as Promise<unknown>)
      .then((mod: unknown) => {
        const m = mod as WasmWebModule;
        if (typeof m.default !== "function") {
          throw new Error(
            `Nostos wasm glue at ${url} has no default export (init)`,
          );
        }
        if (typeof m.NostosSocket !== "function") {
          throw new Error(
            `Nostos wasm glue at ${url} has no NostosSocket export`,
          );
        }
        // Run the wasm init; resolves once wasm bytes are instantiated.
        return Promise.resolve(m.default()).then(() => m.NostosSocket);
      })
      .catch((err: unknown) => {
        // Drop the cache so the next connect() can retry from scratch.
        if (this.wasmPromise === promise) {
          this.wasmPromise = null;
        }
        throw err;
      });
    this.wasmPromise = promise;
    return promise;
  }
}

/**
 * Derive the HTTP base for the REST surface (`POST /push-tokens`,
 * `DELETE /push-tokens/{token}`) from the WS /sync URL: `wss`→`https`,
 * `ws`→`http`, host+port preserved, path stripped. Same rules as the
 * Flutter/Node SDKs (`_deriveHttpBase` / `http_base`).
 */
function deriveHttpBase(wsUrl: string): string {
  const m = /^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/([^/?#]+)/.exec(wsUrl);
  if (!m) {
    return wsUrl;
  }
  const scheme = m[1] === "wss" ? "https" : m[1] === "ws" ? "http" : m[1];
  return `${scheme}://${m[2]}`;
}
