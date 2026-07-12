// @nostos-sync/react-native — TS facade over the NativeNostos TurboModule.
//
// Mirrors `@nostos-sync/web`'s PowerSync-shaped API (connect / subscribe / write /
// query / checkpoint) but Promise-returning and POLL-based (no event emitter):
// `subscribe(table)` starts the live replication loop on the native side (the
// UniFFI `run_with_reconnect` loop inside nostos-swift/kotlin), and the JS app
// polls `pollRows(table)` / `query(sql)` to drain applied rows. This matches
// nostos-swift/kotlin's poll floor (no row-tick callback yet — Phase-2 upgrade
// path is a UniFFI callback interface or a NativeNostos event subscription).

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
   * introspection (`client.config.dbPath` etc.) and so the field is not a
   * write-only dead store.
   *
   * ponytail: CEILING — config is captured here but NOT yet plumbed to the
   * native module. The NativeNostos TurboModule spec (Wave A) declares exactly
   * connect/subscribe/write/query/checkpoint (no constructor / configure),
   * matching the UniFFI surface. Wave B's native module reads url/token/dbPath
   * from native app config (Android gradle.properties / iOS Info.plist) OR the
   * spec grows a setConfig() method — undecided, flagged as a Wave-B unknown
   * in the README. The JS-side capture keeps the facade's shape aligned with
   * @nostos-sync/web so the public API is stable when the plumbing lands.
   */
  readonly config: Required<Pick<NostosClientConfig, "dbPath">> &
    Pick<NostosClientConfig, "url" | "token">;
  private readonly subscriptions: Map<string, Subscription> = new Map();

  constructor(config: NostosClientConfig = {}) {
    this.config = {
      dbPath: config.dbPath ?? ":memory:",
      url: config.url ?? null,
      token: config.token ?? null,
    };
  }

  /** Open the local SQLite store + build the SyncClient. No network I/O. */
  async connect(): Promise<void> {
    await NativeNostos.connect();
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
