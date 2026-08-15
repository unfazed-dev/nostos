// @nostos-sync/react-native — TS facade over the NativeNostos TurboModule.
//
// Mirrors `@nostos-sync/web`'s PowerSync-shaped API (connect / subscribe / write /
// query / checkpoint), Promise-returning. TWO row-access paths:
//   • POLL — `subscribe(table)` starts the live replication loop on the native
//     side (the UniFFI `run_with_reconnect` loop inside nostos-swift/kotlin);
//     the app polls `pollRows(table)` / `query(sql)` to drain applied rows.
//   • REACTIVE — `watch(table, onSnapshot)` PUSHES a fresh FULL snapshot
//     whenever the underlying rows change (initial snapshot + every delta),
//     built on nostos-client's hot-replay change stream (`subscribe_changes()`)
//     — NOT a poll. This is the RN port of node's `watch()` (napi
//     `ThreadsafeFunction`) and kotlin's `watch()` (UniFFI `SnapshotSink`);
//     the push crosses JSI as a retained TurboModule callback
//     (`NativeNostos.watchChanges`).
//
// Push/wake surface (ADR-0037, plan task 5.3): `disconnect()`/`resume()` bridge
// the UniFFI non-destructive teardown pair (plan task 5.1) through the
// TurboModule; `registerPushToken`/`deregisterPushToken` speak the pinned REST
// contract (POST/DELETE /push-tokens, the sync JWT as Bearer, strict 204)
// directly from the facade via RN's global fetch — the UniFFI layer has no
// registration method yet (plan task 5.2), and the REST contract is identical,
// so the seam lives where the credential already does.

import NativeNostos from "./NativeNostos";

/**
 * Client-side write intent. Matches `WriteOp::as_wire_str` in
 * `crates/nostos-core/src/outbox.rs` — the SAME wire strings the server's
 * `dispatch_write` accepts.
 */
export type WriteOp = "upsert" | "delete" | "patch";

/**
 * Push rail a token belongs to (ADR-0037 §3 — the server's pinned set).
 * RN apps register `"fcm"` (Android) or `"apns"` (iOS); `"webpush"` exists for
 * parity with the other SDKs.
 */
export type PushPlatform = "fcm" | "apns" | "webpush";

/**
 * Typed failure from `registerPushToken` / `deregisterPushToken` — the JS-side
 * shape of node's string-reason errors: the failing half of the pair, the HTTP
 * status + body when the server answered (anything but the pinned `204`,
 * INCLUDING other 2xx variants — contract drift must fail loudly), and
 * `undefined` status on a transport failure (offline, DNS, …).
 */
export class NostosPushError extends Error {
  /** Which REST half failed — `"register"` (POST) or `"deregister"` (DELETE). */
  readonly operation: "register" | "deregister";
  /** HTTP status when the server answered; `undefined` on transport failure. */
  readonly status?: number;
  /** Response body when the server answered with a non-204. */
  readonly body?: string;

  constructor(
    operation: "register" | "deregister",
    status: number | undefined,
    body: string | undefined,
    message: string,
  ) {
    super(message);
    this.name = "NostosPushError";
    this.operation = operation;
    this.status = status;
    this.body = body;
  }
}

/**
 * RN ships a global `fetch` (whatwg-fetch over its network stack) but NOT its
 * type declarations, and this package's tsconfig has no DOM lib — declare the
 * minimal structural surface the push-token REST seam uses. Jest's node test
 * env satisfies the same shape with Node's built-in fetch.
 */
declare const fetch: (
  input: string,
  init?: {
    method?: string;
    headers?: Record<string, string>;
    body?: string;
  },
) => Promise<{ status: number; text(): Promise<string> }>;

/** The minimal response surface `expectPush204` reads (structural — see fetch). */
interface PushHttpResponse {
  readonly status: number;
  text(): Promise<string>;
}

/** Constructor config — mirrors `@nostos-sync/web`'s `NostosClientConfig` shape. */
export interface NostosClientConfig {
  url?: string | null;
  token?: string | null;
  /** SQLite file path (`:memory:` for ephemeral). Default `:memory:`. */
  dbPath?: string;
}

/** A row is a column → value map (JSON-decoded from the native query() string). */
export type Row = Record<string, unknown>;

/** Handle returned by `subscribe(table)`. */
export interface Subscription {
  readonly table: string;
  /**
   * Drop the subscription handle from the facade's bookkeeping.
   *
   * ponytail: CEILING — Wave A has no native unsubscribe (nostos-swift/kotlin's
   * UniFFI surface has no `stop()`). This only drops the JS-side handle; the
   * native run-loop continues until the client is torn down. UPGRADE PATH:
   * Wave C adds `NativeNostos.unsubscribe(table)` when the UniFFI surface grows
   * a stop-session method.
   */
  unsubscribe(): void;
}

/**
 * Handle returned by `watch(table, onSnapshot)`. Call `unsubscribe()` to stop
 * receiving snapshots for this handle; when the last handle for a table goes
 * away, the facade tears down the native push pump for that table
 * (`NativeNostos.unwatchChanges`).
 */
export interface WatchSubscription {
  readonly table: string;
  unsubscribe(): void;
}

/**
 * One table's reactive-watch state: the bridge callback the native pump
 * invokes, plus the set of live JS handles to fan each snapshot out to.
 * Module-private — the public surface is `WatchSubscription`.
 */
interface WatchBundle {
  /** The retained callback passed to `NativeNostos.watchChanges`. */
  readonly bridge: (rowsJson: string) => void;
  /** Live JS handles for this table; fanned out to on every native tick. */
  readonly handles: Set<WatchHandle>;
}

/** A single registered `watch()` caller's handle. */
interface WatchHandle {
  readonly onSnapshot: (rows: Row[]) => void;
  /** Set to true on unsubscribe so a late native tick stops forwarding. */
  closed: boolean;
}

/**
 * PowerSync-style sync client for React Native. Wraps the NativeNostos
 * TurboModule (which, in Wave B, wraps nostos-swift / nostos-kotlin's UniFFI
 * `NostosClient`, which wraps `nostos_client::SyncClient<SqliteStorage>`).
 *
 * All methods are async — the native side blocks on its owned tokio runtime
 * and resolves the JS Promise when the Rust call returns.
 */
export class NostosClient {
  /**
   * The resolved config (defaults applied). Exposed readonly for app-level
   * introspection (`client.config.dbPath` etc.). The same three values are
   * plumbed through to the native TurboModule on `connect(...)` so the Kotlin
   * module can lazily construct the UniFFI `NostosClient` on first connect.
   */
  readonly config: Required<Pick<NostosClientConfig, "dbPath">> &
    Pick<NostosClientConfig, "url" | "token">;
  private readonly subscriptions: Map<string, Subscription> = new Map();
  /**
   * Active reactive watches, keyed by table. The native push pump is per-table
   * (one `NativeNostos.watchChanges(table, bridge)` at a time); the facade
   * multiplexes that single pump across every JS `watch()` caller for the table
   * and reference-counts teardown — `unwatchChanges(table)` fires only when the
   * table's last handle unsubscribes. This mirrors the per-table cancel seam
   * the kotlin/node ports tie to session lifecycle.
   */
  private readonly watches: Map<string, WatchBundle> = new Map();
  /**
   * Push tokens registered via `registerPushToken` this session (ADR-0037 §3),
   * best-effort deregistered by `signOut` — a leaked registration would push
   * the previous principal's data to the next user. The JS-side mirror of
   * nostos_node's `registered_push_tokens` cell.
   *
   * ponytail: in-memory only — tokens registered before a process restart are
   * not auto-deregistered. The stale case is covered server-side (the rails
   * prune on APNs 410 / FCM UNREGISTERED); persist the set locally if
   * rail-prune proves too slow.
   */
  private readonly registeredPushTokens: Set<string> = new Set();

  constructor(config: NostosClientConfig = {}) {
    this.config = {
      dbPath: config.dbPath ?? ":memory:",
      url: config.url ?? null,
      token: config.token ?? null,
    };
  }

  /**
   * Construct the backing UniFFI `NostosClient(url, token, dbPath)` + open the
   * local SQLite store + build the SyncClient. No network I/O until
   * `subscribe(table)`. The captured config is passed through to the native
   * TurboModule here — TurboModules are singletons with no JS-visible
   * constructor, so the facade threads the config through `connect(...)`.
   */
  async connect(): Promise<void> {
    // `url` is optional in NostosClientConfig (the constructor defaults it to
    // null), but the native Spec requires a string. Fail here with a message
    // naming the fix instead of handing null across the TurboModule boundary,
    // where it surfaces as an opaque native error.
    const { url } = this.config;
    if (url === null || url === undefined || url === "") {
      throw new Error(
        "nostos: no sync URL configured — pass { url: 'ws://host:port/sync' } to the NostosClient constructor",
      );
    }
    await NativeNostos.connect(url, this.config.token ?? null, this.config.dbPath);
  }

  /**
   * Start the live replication loop for `table` on the native side. Returns a
   * handle; the app polls `pollRows(table)` to drain applied rows. Idempotent
   * — re-subscribing the same table reuses the handle (the native side is
   * idempotent too: nostos-swift/kotlin guard on `session.is_some()`).
   */
  async subscribe(table: string): Promise<Subscription> {
    await NativeNostos.subscribe(table);
    let sub = this.subscriptions.get(table);
    if (sub === undefined) {
      sub = {
        table,
        unsubscribe: () => {
          this.subscriptions.delete(table);
        },
      };
      this.subscriptions.set(table, sub);
    }
    return sub;
  }

  /**
   * Run a SQL query against the on-device SQLite store. Returns decoded rows.
   * The native side returns a JSON-rows string (UniFFI returns `String`); the
   * facade decodes it. Author parameterized SQL here and bind values at the
   * SQLite layer (rusqlite on the native side handles binding).
   */
  async query(sql: string): Promise<Row[]> {
    const json = await NativeNostos.query(sql);
    return JSON.parse(json) as Row[];
  }

  /**
   * Convenience: `SELECT * FROM <table>`. The app polls this after subscribe
   * to drain rows the native apply loop has committed. Rejects unsafe
   * (non-identifier) table names — use `query()` with parameterized SQL for
   * anything dynamic.
   */
  async pollRows(table: string): Promise<Row[]> {
    return this.query(`SELECT * FROM ${quoteIdent(table)}`);
  }

  /**
   * Subscribe to a PUSH stream of full-table snapshots for `table`.
   * `onSnapshot` is invoked once with the INITIAL snapshot (the current row
   * set) and again after every applied change — a push, not a poll. The native
   * side drains `nostos_client::SyncClient::subscribe_changes()` (the hot-replay
   * change broadcast) and re-queries storage per tick, so each emission is a
   * FULL snapshot (self-healing on lag — the same shape kotlin's `SnapshotSink`
   * / node's `watch()` deliver).
   *
   * Multiple `watch()` calls on the same table share ONE native push pump; each
   * gets its own `WatchSubscription`. The first call starts the pump (and
   * receives the native-emitted initial snapshot); later calls get an initial
   * snapshot synthesized from a one-shot `query()` (the sanctioned re-query-
   * storage pattern — once per late watcher, NOT per-tick polling), then all
   * subsequent native ticks. `unsubscribe()` stops one handle; the native pump
   * is torn down (`NativeNostos.unwatchChanges`) when the table's last handle
   * goes away.
   */
  async watch(
    table: string,
    onSnapshot: (rows: Row[]) => void,
  ): Promise<WatchSubscription> {
    // `table` is a subscription key AND (for late-watcher replay) a SQL
    // identifier in `SELECT * FROM <table>` — fail fast on unsafe names, the
    // same defense `pollRows` applies. Returns the validated identifier.
    const safeTable = quoteIdent(table);

    const handle: WatchHandle = { onSnapshot, closed: false };
    const existing = this.watches.get(table);

    if (existing === undefined) {
      // First watcher for this table — start the native push pump. The bridge
      // decodes the native JSON-rows string (the same shape `query()` returns)
      // and fans it to every live handle. The native side invokes it on the JS
      // thread with the initial snapshot, then after each change.
      const handles = new Set<WatchHandle>();
      const bridge = (rowsJson: string): void => {
        const rows = JSON.parse(rowsJson) as Row[];
        for (const h of handles) {
          if (!h.closed) h.onSnapshot(rows);
        }
      };
      const bundle: WatchBundle = { bridge, handles };
      this.watches.set(table, bundle);
      bundle.handles.add(handle);
      await NativeNostos.watchChanges(table, bundle.bridge);
    } else {
      // Late watcher — the native pump is already running and only ticks on
      // change, so it will not re-emit an initial snapshot. Synthesize one via
      // a single `query()` so the "initial snapshot then each change" contract
      // holds for every watcher (nostos-client's re-query-storage pattern; NOT
      // per-tick polling — one fetch per late subscriber, then push takes
      // over). A replay-last-snapshot cache (the kotlin/node `last_snapshot`
      // seam) is the documented Wave-C follow-on.
      existing.handles.add(handle);
      const initial = await this.query(`SELECT * FROM ${safeTable}`);
      if (!handle.closed) handle.onSnapshot(initial);
    }

    return {
      table,
      unsubscribe: (): void => {
        // Idempotent — a second call is a no-op (guards double-unsubscribe and
        // a late native tick after teardown).
        if (handle.closed) return;
        handle.closed = true;
        const bundle = this.watches.get(table);
        if (bundle === undefined) return;
        bundle.handles.delete(handle);
        if (bundle.handles.size === 0) {
          // Last handle for this table — tear down the native pump and drop the
          // bundle so a future `watch(table)` restarts cleanly. Best-effort: we
          // do not await (unsubscribe is sync, matching `Subscription`).
          this.watches.delete(table);
          void NativeNostos.unwatchChanges(table);
        }
      },
    };
  }

  /**
   * Write a row. `payload` is JSON-serialized to match UniFFI's
   * `payload_json: Option<String>` (omitted/`undefined` → `null` =
   * "no row image", the delete shape). Returns the durable sequence number /
   * LSN the native `write()` yields.
   */
  async write(
    table: string,
    op: WriteOp,
    pk: string,
    payload?: unknown,
  ): Promise<number> {
    const payloadJson =
      payload === undefined ? null : JSON.stringify(payload);
    return NativeNostos.write(table, op, pk, payloadJson);
  }

  /** Current durable LSN (the resume_lsn on reconnect). */
  async checkpoint(): Promise<number> {
    return NativeNostos.checkpoint();
  }

  /**
   * NON-destructive pause of the live replication loop (ADR-0037 task 5.1 —
   * "this app is going to sleep"; `signOut()` is "this user is leaving").
   * Delegates to `NativeNostos.disconnect` → UniFFI `NostosClient::disconnect`:
   * the run loop gates closed at a safe point (final flush + ack) and
   * quiesces, but the session, durable store, and token all survive —
   * `query()`/`write()` keep answering and local watch pumps keep ticking.
   * Idempotent and a no-op with no active session.
   */
  async disconnect(): Promise<void> {
    await NativeNostos.disconnect();
  }

  /**
   * Re-open the live replication loop after `disconnect()` — the push wake
   * primitive (ADR-0037 task 5.1). Delegates to `NativeNostos.resume` → UniFFI
   * `NostosClient::resume`: the reconnect's Subscribe re-seeds `resume_lsn`
   * from the durable checkpoint, so the delta that accrued while disconnected
   * applies with no data loss. Does NOT re-run `connect()`. Idempotent with a
   * live loop; rejects before `connect()` (the native `resume` surfaces the
   * before-connect error as a promise rejection).
   */
  async resume(): Promise<void> {
    await NativeNostos.resume();
  }

  /**
   * Register a push token with the server (ADR-0037 §3): `POST /push-tokens`
   * with `{"platform": …, "token": …}`, authenticated by the SAME token the
   * sync connection uses (`Authorization: Bearer`, read from this handle's
   * cached credential — the one `connect()`/`setToken()` stage). The server
   * stamps tenant/account itself; the SDK never attests identity fields.
   *
   * FCM handler wiring (Android): call this from the `firebase/messaging`
   * `onNewToken` callback. APNs (iOS): from the `didRegisterForRemoteNotifications`
   * device-token callback (hex-encode the token). Resolves only on the pinned
   * `204`; anything else (including a 2xx variant) rejects with a
   * {@link NostosPushError} carrying the status + body. Registered tokens are
   * deregistered best-effort by `signOut()`.
   */
  async registerPushToken(
    platform: PushPlatform,
    token: string,
  ): Promise<void> {
    if (platform !== "fcm" && platform !== "apns" && platform !== "webpush") {
      // Fail before the wire — the same guard nostos_node applies (an unknown
      // platform is an SDK-side bug, not a server response).
      throw new Error(
        `unknown push platform ${JSON.stringify(platform)}: expected "fcm", "apns", or "webpush"`,
      );
    }
    const response = await this.pushFetch(this.pushEndpoint("/push-tokens"), {
      method: "POST",
      headers: this.bearerHeaders(this.config.token, {
        "Content-Type": "application/json",
      }),
      body: JSON.stringify({ platform, token }),
    }, "register");
    await expectPush204(response, "register");
    this.registeredPushTokens.add(token);
  }

  /**
   * Deregister a push token (ADR-0037 §3): `DELETE /push-tokens/{token}` with
   * the same auth as `registerPushToken`. Resolves only on the pinned `204`.
   * `signOut()` calls this for every session-registered token automatically;
   * call it directly when the app can no longer receive on the token.
   */
  async deregisterPushToken(token: string): Promise<void> {
    // The token rides the path percent-encoded as ONE segment: a webpush
    // token is the full pushSubscription JSON and contains `/`, which
    // un-encoded would split the path and 404 the DELETE (M1). The server's
    // Path extractor decodes it back verbatim.
    await this.deregisterPushTokenHttp(this.config.token, token);
    this.registeredPushTokens.delete(token);
  }

  /**
   * Shared DELETE core — `deregisterPushToken` (reads the live cached token)
   * and `signOut` (reads the token captured BEFORE it was cleared) both ride
   * this, so there is one wire shape (nostos_node's `deregister_push_token_http`).
   */
  private async deregisterPushTokenHttp(
    auth: string | null | undefined,
    token: string,
  ): Promise<void> {
    const response = await this.pushFetch(
      this.pushEndpoint(`/push-tokens/${encodeURIComponent(token)}`),
      { method: "DELETE", headers: this.bearerHeaders(auth, {}) },
    "deregister",
    );
    await expectPush204(response, "deregister");
  }

  /** The push-token REST base + path, derived from the configured sync URL. */
  private pushEndpoint(path: string): string {
    const { url } = this.config;
    if (url === null || url === undefined || url === "") {
      throw new Error(
        "@nostos-sync/react-native: no sync URL configured — pass { url: 'ws://host:port/sync' } to the NostosClient constructor",
      );
    }
    return httpBase(url) + path;
  }

  /** Bearer headers from `auth` — anonymous (null/undefined) sends NO Authorization. */
  private bearerHeaders(
    auth: string | null | undefined,
    extra: Record<string, string>,
  ): Record<string, string> {
    if (auth === null || auth === undefined) return extra;
    return { ...extra, Authorization: `Bearer ${auth}` };
  }

  /** fetch + transport-failure mapping into the typed error. */
  private async pushFetch(
    url: string,
    init: { method: string; headers: Record<string, string>; body?: string },
    operation: "register" | "deregister",
  ): Promise<PushHttpResponse> {
    try {
      return await fetch(url, init);
    } catch (e) {
      throw new NostosPushError(
        operation,
        undefined,
        undefined,
        `push-token ${operation} failed: ${e instanceof Error ? e.message : String(e)}`,
      );
    }
  }

  /**
   * Hot-swap the auth bearer WITHOUT tearing down the session (ADR-0029 #3).
   * Delegates to `NativeNostos.setToken`, which maps to UniFFI
   * `NostosClient::set_token`: the new token lands in the interior-mutable token
   * cell and is read on the reconnect loop's NEXT attempt — no forced disconnect,
   * no teardown. Pass `null` to clear (anonymous).
   *
   * `this.config.token` is kept in sync so app-level introspection reflects the
   * live token. Callable before `connect()` (stages the token for the first
   * connect) AND on a live session — the native `set_token` is callable in both
   * states (proven by the `set_token_swaps_before_and_after_connect` UniFFI test
   * in nostos-swift / nostos-kotlin).
   */
  async setToken(token: string | null): Promise<void> {
    await NativeNostos.setToken(token);
    this.config.token = token;
  }

  /**
   * Sign out (ADR-0029): abort the run loop, await quiescence, wipe local state
   * (rows + checkpoint + epoch + outbox + dead-letter), drop the session, and
   * clear the token. Delegates to `NativeNostos.signOut`, which maps to UniFFI
   * `NostosClient::sign_out` (abort → await → clear_local_state → drop session →
   * clear token). Idempotent.
   *
   * After this resolves the native client's session is GONE and its token
   * cleared, so the facade drops its JS-side bookkeeping (the `subscriptions`
   * and `watches` maps) and mirrors the token clear in `config`. A late native
   * push tick CANNOT arrive — the push pump died with the session — so clearing
   * the maps is safe WITHOUT per-handle `unwatchChanges` (which would call into
   * a dead client). The next `connect()` starts from a clean store + clean maps.
   */
  async signOut(): Promise<void> {
    // ADR-0037 §3: the sign-out deregistration needs the JWT from BEFORE the
    // native sign-out + the mirror below clear it — capture it now (node's
    // sign_out does the same before its step 4).
    const auth = this.config.token;
    await NativeNostos.signOut();
    this.config.token = null;
    this.subscriptions.clear();
    this.watches.clear();
    // ADR-0037 §3: deregister this session's push tokens — best-effort, AFTER
    // the local wipe (node's step-5 ordering; a failed DELETE is swallowed —
    // the server prunes stale rows on a rail 410/UNREGISTERED). Uses the token
    // captured above, so the DELETEs authorize as the signing-out principal.
    const registered = Array.from(this.registeredPushTokens);
    this.registeredPushTokens.clear();
    for (const token of registered) {
      await this.deregisterPushTokenHttp(auth, token).catch(() => undefined);
    }
  }
}

/**
 * Derive the HTTP base for the push-token REST endpoints from the WS `/sync`
 * URL: `wss`→`https`, `ws`→`http`, trailing path stripped — the same
 * derivation nostos_node's `http_base` and the Flutter SDK's
 * `_deriveHttpBase` use. One credential source, one URL source.
 */
function httpBase(wsUrl: string): string {
  const idx = wsUrl.indexOf("://");
  if (idx === -1) return wsUrl;
  const scheme = wsUrl.slice(0, idx);
  const mapped = scheme === "wss" ? "https" : scheme === "ws" ? "http" : scheme;
  // Authority runs to the first `/` (or end); the path is dropped.
  const rest = wsUrl.slice(idx + 3);
  const authority = rest.split("/")[0] || rest;
  return `${mapped}://${authority}`;
}

/**
 * Enforce the pinned push-token contract (ADR-0037 §3): success is exactly
 * `204 No Content`. Anything else — including a 2xx variant — throws a
 * {@link NostosPushError} carrying the status + body so contract drift fails
 * loudly on the SDK side.
 */
async function expectPush204(
  response: PushHttpResponse,
  operation: "register" | "deregister",
): Promise<void> {
  if (response.status === 204) return;
  const body = await response.text().catch(() => "");
  throw new NostosPushError(
    operation,
    response.status,
    body,
    `push-token ${operation} failed: HTTP ${response.status}: ${body}`,
  );
}

/**
 * Quote a SQL identifier (table/column name) for the `pollRows` convenience
 * path. Real query authoring should use parameterized SQL via `query()` —
 * this only defends the convenience path against obvious injection.
 */
function quoteIdent(name: string): string {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
    throw new Error(
      `@nostos-sync/react-native: unsafe table name ${JSON.stringify(name)} — ` +
        "use query() with parameterized SQL instead of pollRows()",
    );
  }
  return name;
}
