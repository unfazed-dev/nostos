// @nostos-sync/web — PowerSync-style facade over the nostos-ffi-wasm apply engine.
//
// REDUCED-SCOPE PROOF (ponytail: ceiling + upgrade path below)
// ------------------------------------------------------------
// This package loads the wasm-pack `--target nodejs` build of
// nostos-ffi-wasm in Node 22+ and exposes a PowerSync-shaped API
// (connect / subscribe / watch / write / query) as a thin wrapper over
// the wasm apply-engine surface (`NostosEngine`, `Frame`, `Outcome`).
//
// CEILING: this is the apply engine only. `connect()` does NOT open a
// WebSocket. `NostosSocket.connect()` (the live browser WS transport,
// E1) is intentionally NOT invoked here because it is wired to
// `web-sys::WebSocket` + `Window::localStorage`, neither of which
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

// Resolve the wasm-pack output relative to this file so the package
// works regardless of the caller's cwd.
// ponytail: hardcoded relative path to a sibling crate's pkg-node — fine
// inside the monorepo, breaks if this package is `npm publish`'d without
// bundling the wasm. Upgrade: a `prepublishOnly` script that copies the
// wasm + JS glue into `sdk/nostos_web/dist/` and rewrites this path.
const PKG_NODE_DIR = path.resolve(
  __dirname,
  "..",
  "..",
  "crates",
  "nostos-ffi-wasm",
  "pkg-node"
);

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
          "(invokes `wasm-pack build ../../crates/nostos-ffi-wasm --target nodejs --out-dir pkg-node`)."
      );
    }
    throw err;
  }
  return _wasm;
}

/**
 * PowerSync-style sync client. Reduced-scope: see file header — no live
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
   * ponytail: real PowerSync writes go through the server mutation
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

  /** Current durable checkpoint (the LSN to resume from on reconnect). */
  get checkpoint() {
    return this._engine.checkpoint;
  }

  /** Rows currently held across all tables. */
  get rowCount() {
    return this._engine.rowCount;
  }
}

module.exports = { NostosClient, NostosEngine: () => wasm().NostosEngine, Frame: () => wasm().Frame };
