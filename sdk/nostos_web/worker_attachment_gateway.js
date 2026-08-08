// WorkerAttachmentGateway — the LIVE browser AttachmentMetadataGateway (ADR-0034).
//
// The Attachments driver (attachments.js) talks to its metadata plane through a
// small gateway interface (queuedRows / patchState / upsertRow / currentState).
// In node, e2e/attachments.spec.cjs supplies an in-memory FakeGateway. In the
// browser, the synced `attachments` table lives in the Worker's wasm store, so
// this gateway proxies each op to the Worker over the EXISTING postMessage
// commands — no new Worker handlers, no Rust/Wasm change:
//
//   queuedRows()/currentState() → `rowsFor("attachments")` (the row's payload is
//                                the column→value JSON, stored as bytes; decoded
//                                here) + a client-side filter by state.
//   upsertRow()/patchState()    → `write("attachments", op, pk, payloadJson)` where
//                                op is "upsert" (full row, WriteOp::Upsert) or
//                                "patch" (column merge, WriteOp::Patch — so
//                                patchState flips only `state`, preserving the
//                                other columns). NostosSocket.write parses via
//                                WriteOp::from_wire_str (upsert|delete|patch).
//
// Ordering: `write` is fire-and-forget (postMessage), but postMessage is FIFO, so
// a subsequent `rowsFor` request is processed by the Worker AFTER the write — the
// row is already applied locally (apply_local) when it's read. No race.
//
// The blob plane (OpfsBlobStore + the developer's adapter) stays on the main
// thread; only metadata crosses to the Worker.

"use strict";

const QUEUED = new Set([
  "queued_upload",
  "queued_download",
  "queued_delete",
]);

class WorkerAttachmentGateway {
  /**
   * @param {{write: (table:string, op:string, pk:string, payload_json:string, client_write_id:string)=>void, rowsFor:(table:string)=>Promise<{rows:Array<{pk:string, payload:Uint8Array|number[]|string}>}>}} proxy — the Worker proxy (e.g. window.nostos from e2e/app.html)
   * @param {{table?:string, stateColumn?:string}} [opts]
   */
  constructor(proxy, opts = {}) {
    this._proxy = proxy;
    this._table = opts.table || "attachments";
    this._stateCol = opts.stateColumn || "state";
    this._dec = new TextDecoder();
    this._seq = 1;
  }

  /** Decode a row's byte payload into a column→value object. */
  _decode(payload) {
    let s = "";
    if (payload instanceof Uint8Array) {
      s = this._dec.decode(payload);
    } else if (typeof payload === "string") {
      s = payload;
    } else if (Array.isArray(payload)) {
      s = this._dec.decode(Uint8Array.from(payload));
    }
    if (!s) return {};
    try {
      return JSON.parse(s);
    } catch (_) {
      return {};
    }
  }

  /** All attachment rows as {id, ...columns}. */
  async _rows() {
    const r = await this._proxy.rowsFor(this._table);
    return (r.rows || []).map((row) => ({
      id: row.pk,
      ...this._decode(row.payload),
    }));
  }

  async queuedRows() {
    return (await this._rows()).filter((r) => QUEUED.has(r[this._stateCol]));
  }

  async patchState(id, state) {
    // op "patch" MERGES only the `state` column into the existing row
    // (WriteOp::Patch — preserves filename/mediaType/etc.).
    this._proxy.write(
      this._table,
      "patch",
      id,
      JSON.stringify({ [this._stateCol]: state }),
      "gw-patch-" + this._seq++,
    );
  }

  async upsertRow(row) {
    // op "upsert" sets the full row (WriteOp::Upsert).
    this._proxy.write(
      this._table,
      "upsert",
      row.id,
      JSON.stringify(row),
      "gw-upsert-" + this._seq++,
    );
  }

  async currentState(id) {
    const rows = await this._rows();
    const r = rows.find((x) => x.id === id);
    if (!r) throw new Error("WorkerAttachmentGateway: no row for " + id);
    return r[this._stateCol];
  }
}

module.exports = { WorkerAttachmentGateway, QUEUED };
