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

import NativeNostos from "./NativeNostos";

/**
 * Client-side write intent. Matches `WriteOp::as_wire_str` in
 * `crates/nostos-core/src/outbox.rs` — the SAME wire strings the server's
 * `dispatch_write` accepts.
 */
export type WriteOp = "upsert" | "delete" | "patch";

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
