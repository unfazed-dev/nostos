// nostos_worker.js — Flutter-web Worker host for WebNostosEngine (ADR-0036).
//
// This module Worker is the SOLE wasm host for Flutter-web. It owns the live
// connection (`NostosSocket` — web-sys WebSocket + the apply engine + the
// durable sqlite-wasm / memory backend) and speaks WebNostosEngine's boundary
// protocol over postMessage. The Dart side (`engine_web.dart` +
// `web_worker_port.dart`) is a pure-Dart protocol layer; this file is the JS
// half that actually loads wasm + sqlite-wasm and drives `NostosSocket`.
//
// It is a consumer of the SAME `nostos_ffi_wasm.js` `--target web` artifact the
// `@nostos-sync/web` SDK's worker (`sdk/nostos_web/worker/nostos.worker.js`) uses —
// one shared Rust backend, two JS-layer hosts (the JS SDK's single-table
// protocol and this Flutter-web multi-table protocol). No second crate, no
// feature flag.
//
// Why a separate worker rather than reusing nostos.worker.js verbatim: the Dart
// NostosEngine seam (subscribe/watch/write/query/applySchema/disconnect/resume,
// multi-table, json snapshots, writeStatus pushes) is a richer protocol than
// the JS SDK's single-table rowsFor/writeResult shape. Translating at the Dart
// layer would mean Dart reshaping every message; instead this worker speaks
// Dart's protocol natively and keeps `engine_web.dart` a thin pure-Dart fanout
// (VM-testable with a fake port — see test/engine_web_test.dart).
//
// Boundary protocol (Dart WebNostosEngine <-> this Worker):
//   Dart -> Worker (each request carries `id`):
//     {id, cmd:"connect", url, token?, tables:[{name, whereSql?}, ...]}
//     {id, cmd:"write", table, op, pk, payloadJson?}      -> {id, ok, writeId}
//     {id, cmd:"query", sql}                              -> {id, ok, json}
//     {id, cmd:"applySchema", tables:[{name, columns}]}   -> {id, ok}
//     {    cmd:"watch", table}        (no id — fire-and-forget)
//     {    cmd:"unwatch"}             (no id — clears all watches; see ponytail)
//     {id, cmd:"setToken", token?}                        -> {id, ok}
//     {id, cmd:"disconnect"}                               -> {id, ok}
//     {id, cmd:"resume"}                                   -> {id, ok}
//     {id, cmd:"close"}                                    -> {id, ok}
//     {id, cmd:"signOut"}                                  -> {id, ok}
//   Worker -> Dart:
//     {id, ok:true, ...} | {id, error:"..."}     response to a request
//     {type:"status", connected}                 connection-state transition
//     {type:"snapshot", table, json}             reactive push: per-table JSON
//                                               array-of-objects string, fired
//                                               on every change tick (onChange)
//     {type:"writeStatus", pending, deadLettered, lastError}   outbox status
//     {type:"storage", mode:"durable"|"memory"}  OPFS or degrade (ADR-0033)
//
// `unsafe`-free: pure JS. The Rust write path uses the real Outbox trait
// (enqueue + apply_local + flush), so a write never throws when the socket is
// closed — it is captured locally and ships on (re)connect.
import init, { NostosSocket } from "./nostos_ffi_wasm.js";

let wasmReady = false;
let sock = null;
// The set of tables Dart is watching (multi-table fanout — ADR-0022). onChange
// fires on every commit regardless; `watchedTables` gates the forward so a
// connected-but-unwatched table doesn't spam Dart with snapshots.
const watchedTables = new Set();
// Cached connection params + token so setToken/resume can reconnect.
let connParams = null; // { url, tables:[{name, whereSql?}] }
let token = null;

// ADR-0033: the durable SQLite-WASM db handle (or null in memory/degrade mode).
// Set by initStorage() on boot. Passed to NostosSocket.connect as the 5th arg.
let dbHandle = null;
// "durable" (OPFS-backed SQLite-WASM) or "memory" (InMemoryStorage fallback).
let storageMode = "memory";

// Build the JSON-array-of-objects string for one table's current rows and push
// it to Dart. Uses rowsFor (reads cairn_data directly — no view dependency, so
// this works before applySchema runs). Each row's payload is a JSON object; we
// parse + re-emit so Dart's Collection<T>.fromRow sees plain row objects (the
// same shape the native view query returns).
function postSnapshot(table) {
  let json = "[]";
  if (sock) {
    try {
      const rows = sock.rowsFor(table);
      json = JSON.stringify(
        rows.map((r) => {
          try {
            return JSON.parse(r.payload);
          } catch (_) {
            return { pk: r.pk };
          }
        }),
      );
    } catch (_) {
      /* socket torn down between tick + read — leave json "[]" */
    }
  }
  self.postMessage({ type: "snapshot", table, json });
}

// Push the durable-outbox status (pending / dead-lettered / last error). Called
// on every change tick so Dart's watchWriteStatus stays current as writes ship
// or dead-letter.
function postWriteStatus() {
  if (!sock) return;
  try {
    self.postMessage({
      type: "writeStatus",
      pending: sock.pendingCount,
      deadLettered: sock.deadLetteredCount,
      lastError: sock.lastError,
    });
  } catch (_) {
    /* socket torn down — ignore */
  }
}

// Wire a freshly-opened socket's reactive push: register the Rust→JS onChange
// callback that forwards a fresh snapshot for EVERY watched table on each
// change tick, plus the writeStatus. Shared by connect / setToken / resume.
function attachChangePush(s) {
  s.onChange(() => {
    for (const t of watchedTables) {
      postSnapshot(t);
    }
    postWriteStatus();
  });
}

async function ensureWasm() {
  if (!wasmReady) {
    await init();
    wasmReady = true;
  }
}

// ADR-0033: async-init sqlite-wasm with opfs-sahpool on boot. On success →
// durable mode (dbHandle set). On failure (Safari Private Browsing, old
// browsers, OPFS disallowed) → degrade to memory (dbHandle null). The mode is
// pushed to Dart so SyncStatus can surface it. NOT a crash — the memory path
// is the explicit degrade fallback.
async function initStorage() {
  try {
    const { openNostosDb } = await import("./sqlite_wasm_glue.js");
    dbHandle = await openNostosDb();
    storageMode = "durable";
  } catch (e) {
    console.error("[nostos_worker] storage init failed:", (e && e.message) || e);
    dbHandle = null;
    storageMode = "memory";
  }
  self.postMessage({ type: "storage", mode: storageMode });
}

// Open (or reopen) the socket for connParams: connect with the first table,
// then subscribe the rest (NostosSocket.connect is single-table; subscribe adds
// more over the open socket — Wave 4a multi-table).
async function openSocket() {
  const { url, tables } = connParams;
  const first = tables[0] ?? { name: "__placeholder__", whereSql: null };
  sock = await NostosSocket.connect(
    url,
    token,
    first.name,
    first.whereSql ?? null,
    dbHandle,
  );
  // Subscribe the remaining tables over the open socket.
  for (let i = 1; i < tables.length; i++) {
    try {
      sock.subscribe(tables[i].name, tables[i].whereSql ?? null);
    } catch (e) {
      // ponytail: subscribe fails if the socket isn't OPEN yet (the wasm
      // ready_state check). connect awaits OPEN, so this should not fire in
      // practice; if it does, the table is simply unwatched until a later
      // resume. Ceiling: engine-level per-table checkpoint tracking (see the
      // subscribe doc in lib.rs) would make multi-table first-class.
      console.warn("[nostos_worker] subscribe failed for", tables[i].name, e);
    }
  }
  attachChangePush(sock);
}

// Eager-init on boot: wasm first, then sqlite-wasm. Dart sees {type:storage}
// before any command. Wasm init failure is fatal (posted as an error); storage
// init failure is a graceful degrade (memory mode).
void ensureWasm()
  .then(() => initStorage())
  .catch((e) =>
    self.postMessage({
      type: "storage",
      mode: "memory",
      error: "wasm-init: " + String((e && e.message) || e),
    }),
  );

self.onmessage = async (ev) => {
  const m = ev.data || {};
  const id = m.id;

  try {
    switch (m.cmd) {
      case "connect": {
        await ensureWasm();
        token = m.token ?? null;
        connParams = { url: m.url, tables: m.tables ?? [] };
        await openSocket();
        self.postMessage({ id, ok: true, checkpoint: sock.checkpoint });
        self.postMessage({ type: "status", connected: true });
        break;
      }
      case "write": {
        if (!sock) {
          self.postMessage({ id, error: "not connected" });
          break;
        }
        // client_write_id is required by the wasm boundary (a string); use the
        // request id (Dart correlates the response by it anyway).
        const writeId = sock.write(
          m.table,
          m.op,
          m.pk,
          m.payloadJson ?? null,
          String(id),
        );
        self.postMessage({ id, ok: true, writeId });
        break;
      }
      case "query": {
        const json = sock ? sock.query(m.sql) : "[]";
        self.postMessage({ id, ok: true, json });
        break;
      }
      case "applySchema": {
        // Map Dart's {name, columns} into the ClientTableFfi shape the wasm
        // deserializer expects {name, primary_key, columns}. primary_key is
        // informational at this layer (the views key off table_name in
        // cairn_data); default to [] when Dart omits it.
        if (sock) {
          const tables = (m.tables ?? []).map((t) => ({
            name: t.name,
            primary_key: t.primaryKey ?? [],
            columns: t.columns ?? [],
          }));
          sock.applySchema(tables);
        }
        self.postMessage({ id, ok: true });
        break;
      }
      case "watch": {
        // No request id — fire-and-forget. Add to the watched set and push the
        // initial snapshot immediately (subsequent ticks arrive via onChange).
        watchedTables.add(m.table);
        postSnapshot(m.table);
        break;
      }
      case "unwatch": {
        // ponytail: Dart sends a bare unwatch (no table), so this clears ALL
        // watches. Per-table unwatch needs a {cmd:"unwatch", table} protocol
        // addition in engine_web.dart; acceptable today because Flutter watch
        // streams are long-lived (tabs persist for the session).
        watchedTables.clear();
        break;
      }
      case "setToken": {
        token = m.token ?? null;
        if (sock && connParams) {
          try { sock.offChange(); } catch (_) {}
          try { sock.close(); } catch (_) {}
          sock = null;
          await ensureWasm();
          await openSocket();
          self.postMessage({ id, ok: true, checkpoint: sock.checkpoint });
          self.postMessage({ type: "status", connected: true });
        } else {
          self.postMessage({ id, ok: true });
        }
        break;
      }
      case "disconnect": {
        if (sock) {
          try { sock.offChange(); } catch (_) {}
          try { sock.close(); } catch (_) {}
          sock = null;
        }
        self.postMessage({ id, ok: true });
        self.postMessage({ type: "status", connected: false });
        break;
      }
      case "resume": {
        if (sock && connParams) {
          // Already-open sockets re-send the subscribe frame as a heartbeat;
          // a closed socket reconnects. Either way, re-attach the push.
          try { await sock.resume(); } catch (_) {}
        } else if (connParams) {
          await ensureWasm();
          await openSocket();
        }
        self.postMessage({ id, ok: true });
        if (sock) self.postMessage({ type: "status", connected: true });
        break;
      }
      case "close": {
        if (sock) {
          try { sock.offChange(); } catch (_) {}
          try { sock.close(); } catch (_) {}
          sock = null;
        }
        watchedTables.clear();
        self.postMessage({ id, ok: true });
        self.postMessage({ type: "status", connected: false });
        break;
      }
      case "signOut": {
        // ADR-0029 D1: wipe rows + outbox, close, drop token + subscription.
        if (sock) {
          try { sock.clearLocalState(); } catch (_) {}
          try { sock.offChange(); } catch (_) {}
          try { sock.close(); } catch (_) {}
          sock = null;
        }
        if (dbHandle) {
          try { dbHandle.clearAll(); } catch (_) {}
        }
        watchedTables.clear();
        connParams = null;
        token = null;
        self.postMessage({ id, ok: true });
        self.postMessage({ type: "status", connected: false });
        break;
      }
      default:
        if (id !== undefined) {
          self.postMessage({ id, error: "unknown cmd: " + String(m.cmd) });
        }
    }
  } catch (e) {
    const msg = String((e && e.message) || e);
    if (id !== undefined) {
      self.postMessage({ id, error: msg });
    }
  }
};
