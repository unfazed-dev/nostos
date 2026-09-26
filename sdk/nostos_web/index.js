// @nostos-sync/web — JS facade over the nostos-ffi-wasm apply engine.
//
// REDUCED-SCOPE PROOF (ponytail: ceiling + upgrade path below)
// ------------------------------------------------------------
// This package loads the wasm-pack `--target nodejs` build of
// nostos-ffi-wasm in Node 22+ and exposes a connect/subscribe/write API
// (connect / subscribe / watch / write / query) PLUS the typed Tier-1 surface
// (writeBatch / OR-set / PN-counter / dead-letter visibility — ADR-0030/0032)
// as a thin wrapper over the wasm apply-engine surface (`NostosEngine`, `Frame`,
// `Outcome`).
//
// CEILING: this is the apply engine only. `connect()` does NOT open a
// WebSocket. `NostosSocket.connect()` (the live browser WS transport,
// E1) is intentionally NOT invoked here because it is wired to
// `web-sys::WebSocket` + `Window::localStorage` (the default of the
// injectable `setKvStore` seam — plan 6.1), neither of which
// exists in Node without a polyfill. Calling it from node would throw
// at the first `web_sys::WebSocket::new()` or `window()` access.
//
// UPGRADE PATH: to ship a live-sync @nostos-sync/web for node, either
//   (a) add a node WS transport adapter — a thin Rust module that
//       imports `ws` via `js_sys` / node's `require('ws')` and replaces
//       the web_sys transport at the `NostosSocket` seam — and gate it
//       behind a `#[cfg(feature = "node-transport")]`; OR
//   (b) publish a second build with `--target web` for browser bundlers
//       (vite/webpack) where the browser provides WebSocket +
//       localStorage natively. A vitest browser-env test then replaces
//       this node smoke.
//
// In the meantime: drive the apply engine from JS, feed frames, read
// rows, observe checkpoints — that is what this proof exercises.

"use strict";

const path = require("path");

// The build stages the generated glue beside this facade so the same path
// works in the checkout and in the published package.
const PKG_NODE_DIR = path.join(__dirname, "pkg-node");

// We require the generated CJS glue. The glue itself reads
// `nostos_ffi_wasm_bg.wasm` via `fs.readFileSync(__dirname + ...)`, so
// it finds the wasm bytes automatically.
let _wasm = null;
function wasm() {
  if (_wasm) return _wasm;
  try {
    _wasm = require(path.join(PKG_NODE_DIR, "nostos_ffi_wasm.js"));
  } catch (err) {
    const fs = require("fs");
    if (!fs.existsSync(path.join(PKG_NODE_DIR, "nostos_ffi_wasm.js"))) {
      throw new Error(
        "@nostos-sync/web: wasm pkg not built. Run `npm run build` in sdk/nostos_web " +
          "(builds and stages the Node wasm package)."
      );
    }
    throw err;
  }
  return _wasm;
}

/**
 * Sync client. Reduced-scope: see file header — no live
 * transport in node; this wraps the apply engine.
 */
class NostosClient {
  /**
   * @param {{url?: string, token?: string, table?: string}} config
   */
  constructor(config = {}) {
    this._config = { url: null, token: null, table: null, ...config };
    this._engine = new (wasm().NostosEngine)();
    this._connected = false;
  }

  /**
   * Connect to the sync server.
   * ponytail: reduced-scope — does NOT open a WS. Stores config and
   * marks the client ready to receive locally-fed frames. Live WS
   * arrives with the node-transport adapter (see file header).
   * @returns {Promise<NostosClient>}
   */
  async connect() {
    this._connected = true;
    return this;
  }

  /**
   * Subscribe to a table with an optional server-side predicate.
   * The predicate is stored on the engine for the (future) transport
   * to attach to the subscribe frame; it is NOT evaluated locally.
   * @param {string} table
   * @param {string|null} whereSql
   * @returns {NostosClient}
   */
  subscribe(table, whereSql = null) {
    this._config.table = table;
    this._engine.setWhereSql(whereSql);
    return this;
  }

  /**
   * Write a row into the apply engine (client-side insert).
   * ponytail: real writes go through the server mutation
   * pipeline; this proof feeds frames directly to demonstrate the
   * apply boundary. LSN is synthesized from Date.now() because there
   * is no server in the loop. Ceiling: replace with a server round-trip
   * once the transport lands.
   * @param {string} table
   * @param {string|number} pk
   * @param {Uint8Array|number[]} payload
   * @returns {{checkpoint: number, rowsApplied: number}}
   */
  write(table, pk, payload) {
    const lsn = Date.now();
    const buf = Buffer.from(payload);
    const frame = new (wasm().Frame)(
      lsn,
      "insert",
      table,
      String(pk),
      buf,
      null
    );
    const buffered = this._engine.feed(frame);
    // A standalone frame may buffer (no commit yet) — flush to durably
    // apply so the row is observable via query().
    const outcome = buffered || this._engine.flush();
    return {
      checkpoint: outcome.checkpoint,
      rowsApplied: outcome.rowsApplied,
    };
  }

  /**
   * Read the rows currently held for `table`.
   * ponytail: snapshot only — no reactive watch. Real `watch` would
   * fire the callback on each inbound commit; this proof calls it once
   * with the current state.
   * @param {string} table
   * @returns {Array<{pk: string, payload: Buffer}>}
   */
  query(table) {
    return this._engine.rowsFor(table).map((entry) => ({
      pk: entry.pk,
      payload: Buffer.from(entry.payload),
    }));
  }

  /**
   * Register a callback fired with a snapshot of the table.
   * Returns an unsubscribe stub.
   * ponytail: fires once immediately. Live watch needs E1 transport.
   * @param {string} table
   * @param {(rows: Array<{pk: string, payload: Buffer}>) => void} callback
   * @returns {() => void}
   */
  watch(table, callback) {
    callback(this.query(table));
    return () => {
      /* ponytail: no-op until live transport lands */
    };
  }

  // --- Typed Tier-1 surface (ADR-0030 / ADR-0032 T3+T4) ---------------------
  // Forwarded to the wasm `NostosEngine`, which (Wave 4a) already ports the full
  // typed surface from the native SyncClient onto the in-process apply engine —
  // reusing nostos-domain CRDT, NOT reinventing it. camelCase matches the Flutter
  // typed surface. CRDT verbs REQUIRE the table tagged via setCrdtTables FIRST,
  // else they throw CounterTableNotTagged / OrSetTableNotTagged — the
  // three-views-of-one-truth gate (must match the server's
  // NOSTOS_COUNTER_COLUMNS / NOSTOS_OR_SET_COLUMNS).

  /**
   * Tag tables as OR-set / counter so apply_local MERGES instead of clobbering.
   * Call BEFORE any orSet* / counter* verb. Mirrors native NOSTOS_*_COLUMNS.
   * @param {string[]} orSetTables
   * @param {string[]} counterTables
   */
  setCrdtTables(orSetTables, counterTables) {
    this._engine.setCrdtTables(orSetTables ?? [], counterTables ?? []);
  }

  /**
   * Enqueue a batch of writes atomically — one txn (mid-batch failure rolls back
   * the whole batch); each op is also apply_local'd for instant optimistic UI.
   * Each op is `{table, op:"upsert|delete|patch", pk, payloadJson?}` where
   * payloadJson is the column→value JSON object string (omit/empty for delete).
   * @param {Array<{table: string, op: string, pk: string, payloadJson?: string}>} ops
   * @returns {number[]} outbox write ids, in order
   */
  writeBatch(ops) {
    // wasm returns Float64Array (Vec<f64>); normalize to a plain Array — same
    // fix as nostos_worker.js (Array.isArray(Float64Array) is false).
    return Array.from(this._engine.writeBatch(ops));
  }

  /**
   * Add `element` to the add-wins OR-set in row `pk` of `table`. Mints a client
   * HLC, enqueues a merge-upsert; the element renders locally immediately.
   * @returns {number} outbox write id
   */
  orSetAdd(table, pk, element) {
    return this._engine.orSetAdd(table, pk, element);
  }

  /** Remove `element` from the OR-set (add-wins tombstone). @returns {number} */
  orSetRemove(table, pk, element) {
    return this._engine.orSetRemove(table, pk, element);
  }

  /**
   * Increment the PN-counter in row `pk` of `table` by `delta`.
   * @returns {number} outbox write id
   */
  counterIncrement(table, pk, delta) {
    return this._engine.counterIncrement(table, pk, delta);
  }

  /** Decrement the PN-counter by `delta`. @returns {number} */
  counterDecrement(table, pk, delta) {
    return this._engine.counterDecrement(table, pk, delta);
  }

  /**
   * Apply a client schema (ClientTable[]). Required for SQL query / CRDT apply
   * on the durable (sqlite-wasm) backend; the Memory node engine is schemaless.
   * @param {object[]} tables
   */
  applySchema(tables) {
    this._engine.applySchema(tables);
  }

  /**
   * Run arbitrary SQL against the store; returns rows as a JSON string. Durable
   * (sqlite-wasm) backed in the browser; the Memory node engine has a limited
   * query surface — prefer {@link query} (rowsFor) in node.
   * @param {string} sql
   * @returns {string}
   */
  querySql(sql) {
    return this._engine.query(sql);
  }

  /** Pending (server-un-acked) client writes in the outbox. */
  get pendingCount() {
    return this._engine.pendingCount;
  }

  /** Writes moved to the dead-letter queue (exhausted retries). */
  get deadLetteredCount() {
    return this._engine.deadLetteredCount;
  }

  /** Last outbox error — surfaces dead-letter cause. Falsy (undefined) when no
   * write has dead-lettered; a non-empty string otherwise (truthy-check it). */
  get lastError() {
    return this._engine.lastError;
  }

  /**
   * Sign out (ADR-0029): wipe the apply engine's in-memory rows + outbox and
   * drop the cached token, so a subsequent principal on the same process sees
   * none of the previous user's rows or pending writes. Maps to
   * `NostosEngine.clear()` (the wasm seam added for WS4-D3). The node proof has
   * no live socket to close; the browser Worker additionally closes the socket
   * (see worker/nostos.worker.js `signOut`).
   * @returns {void}
   */
  signOut() {
    this._engine.clear();
    this._config.token = null;
    this._connected = false;
  }

  /**
   * Swap the auth token (ADR-0029 §3). The node proof has no live transport, so
   * this just caches the new token for the next `connect`. The browser Worker
   * reconnects the socket on token swap (see worker/nostos.worker.js `setToken`).
   * @param {string|null} newToken
   */
  setToken(newToken) {
    this._config.token = newToken ?? null;
  }

  /** Current durable checkpoint (the LSN to resume from on reconnect). */
  get checkpoint() {
    return this._engine.checkpoint;
  }

  /** Rows currently held across all tables. */
  get rowCount() {
    return this._engine.rowCount;
  }

  /**
   * Storage backend mode (ADR-0033). Always "memory" in the node smoke — OPFS
   * is browser-only. The browser Worker surfaces "durable" when sqlite-wasm +
   * opfs-sahpool init succeeds, "memory" on the degrade path.
   * @returns {"memory"}
   */
  get storageMode() {
    return "memory";
  }
}

// T6 attachments (ADR-0034): two-plane blob sync. Re-exported so apps can
// `const { Attachments, SupabaseStorageAdapter } = require("@nostos-sync/web")`.
// The module is lazy-friendly: it loads without @supabase/supabase-js installed
// (SupabaseStorageAdapter pulls it in only at construction).
const attachments = require("./attachments.js");

module.exports = {
  NostosClient,
  NostosEngine: () => wasm().NostosEngine,
  Frame: () => wasm().Frame,
  // T6 attachments
  Attachments: attachments.Attachments,
  SupabaseStorageAdapter: attachments.SupabaseStorageAdapter,
  OpfsBlobStore: attachments.OpfsBlobStore,
  AttachmentConstants: {
    TABLE: attachments.ATTACHMENTS_TABLE,
    COL: attachments.COL,
    STATE: attachments.STATE,
  },
};
