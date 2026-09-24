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
//     {id, cmd:"orSetAdd", table, pk, element}            -> {id, ok, writeId}
//     {id, cmd:"orSetRemove", table, pk, element}         -> {id, ok, writeId}
//     {id, cmd:"counterIncrement", table, pk, delta}      -> {id, ok, writeId}
//     {id, cmd:"counterDecrement", table, pk, delta}      -> {id, ok, writeId}
//     {id, cmd:"writeBatch", ops:[{table,op,pk,payloadJson?}]} -> {id, ok, writeIds}
//     {id, cmd:"query", sql}                              -> {id, ok, json}
//     {id, cmd:"applySchema", tables:[{name, columns}]}   -> {id, ok}
//     {id, cmd:"setCrdtTables", orSet:[], counter:[]}     -> {id, ok}
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
// Why storageMode is "memory": "secondary-tab" | "opfs-unavailable" | null.
let storageReason = null;

// Multi-tab leadership (sqlite.org/wasm persistence.md, "OPFS SyncAccessHandle
// Pool VFS"): opfs-sahpool is ONE instance per origin — a second tab's
// installOpfsSAHPoolVfs() throws, and before this guard that tab silently
// degraded to memory with its OWN live socket: two diverging local states and
// non-durable writes in tab 2. Web Locks decides leadership first.
// Mirrors sdk/nostos_web/worker/nostos.worker.js — keep the two in step.
// The loser then proxies to the leader over a BroadcastChannel (follower
// proxy, end of file) instead of running its own memory engine.
const LEADER_LOCK = "nostos:opfs-sahpool";
let leaderLockHeld = false; // also set by the promotion path (follower proxy)
async function acquireLeaderLock() {
  if (typeof navigator === "undefined" || !navigator.locks) {
    leaderLockHeld = true;
    return true; // no Web Locks API → too old for OPFS anyway; let init decide
  }
  return new Promise((resolve) => {
    navigator.locks.request(LEADER_LOCK, { ifAvailable: true }, (lock) => {
      leaderLockHeld = lock !== null;
      resolve(leaderLockHeld);
      // Hold the lock for the Worker's lifetime: never settle this promise.
      return lock ? new Promise(() => {}) : undefined;
    });
  });
}

// Cached applySchema payload ([{name, primary_key, columns}]). The Dart side
// sends applySchema BEFORE connect (call-order-free contract), when sock is
// still null — stash it here and apply on every openSocket so the WS2
// read-views exist before the first query (the fix engine_web's ponytail
// named: "a pre-open schema cache applied inside the Worker on connect").
let schemaTables = null;

// Build the JSON-array-of-objects string for one table's current rows and push
// it to Dart. Uses rowsFor (reads nostos_data directly — no view dependency, so
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
  if (!(leaderLockHeld || (await acquireLeaderLock()))) {
    dbHandle = null;
    storageMode = "memory";
    storageReason = "secondary-tab";
    console.warn("[nostos_worker] another tab owns the durable store; memory mode");
  } else {
    try {
      const { openNostosDb } = await import("./sqlite_wasm_glue.js");
      dbHandle = await openNostosDb();
      storageMode = "durable";
    } catch (e) {
      console.error("[nostos_worker] storage init failed:", (e && e.message) || e);
      dbHandle = null;
      storageMode = "memory";
      storageReason = "opfs-unavailable";
    }
  }
  // MDN Storage API: origin storage is best-effort (evictable) unless the page
  // was granted persistence. `persist()` is Window-only — web_worker_port.dart
  // calls it on spawn; the Worker can only report the outcome.
  let persisted = null;
  try {
    persisted = await navigator.storage.persisted();
  } catch (_) {
    /* no StorageManager → leave null */
  }
  self.postMessage({ type: "storage", mode: storageMode, reason: storageReason, persisted });
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
  // Re-tag CRDT tables on every (re)connect so apply_local MERGES instead of
  // clobbering (ADR-0030 / ADR-0032 T4). setCrdtTables is a no-op when both
  // lists are empty. Stashed in connParams by the connect handler so reconnects
  // (resume/setToken → openSocket) re-apply without Dart re-sending.
  sock.setCrdtTables(connParams.orSetTables ?? [], connParams.counterTables ?? []);
  // Apply the (possibly pre-open) schema cache: DROP+CREATE VIEW IF EXISTS,
  // so re-applying on every (re)connect is idempotent.
  if (schemaTables !== null) {
    try {
      sock.applySchema(schemaTables);
    } catch (e) {
      console.warn("[nostos_worker] applySchema on connect failed:", e);
    }
  }
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
// init failure is a graceful degrade (memory mode). openSocket callers AWAIT
// bootP: connect must not race initStorage, or it builds a MEMORY engine
// while the durable handle lands a beat later (the storage push then lies —
// "durable" — and every query silently returns "[]", SqliteWasm-only by
// contract).
const bootP = ensureWasm()
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
        await bootP;
        // A follower tab never reaches here (its connect is proxied to the
        // leader — see the follower proxy at the end of this file).
        if (sock) {
          // A later tab's proxied connect JOINS the live session as-is (the
          // first tab's url/tables/CRDT tags win) instead of opening a second
          // socket over the same store.
          self.postMessage({ id, ok: true, checkpoint: sock.checkpoint });
          self.postMessage({ type: "status", connected: true });
          break;
        }
        token = m.token ?? null;
        connParams = {
          url: m.url,
          tables: m.tables ?? [],
          orSetTables: m.orSetTables ?? [],
          counterTables: m.counterTables ?? [],
        };
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
      case "orSetAdd":
      case "orSetRemove":
      case "counterIncrement":
      case "counterDecrement": {
        // Wave 4c (ADR-0036): CRDT delegates on NostosSocket. Each mints a
        // client HLC + enqueues + apply_locals in the engine (reusing
        // nostos-domain), then ships if OPEN. The wasm method name matches the
        // cmd (camelCase); dispatch by the cmd string.
        if (!sock) {
          self.postMessage({ id, error: "not connected" });
          break;
        }
        const fn = sock[m.cmd]; // orSetAdd | orSetRemove | counterIncrement | counterDecrement
        const writeId = fn.call(sock, m.table, m.pk, m.element ?? m.delta);
        self.postMessage({ id, ok: true, writeId });
        break;
      }
      case "writeBatch": {
        // Wave 4c: atomic enqueue (one storage txn) + per-op ship if OPEN.
        // ops is [{table, op, pk, payloadJson?}, ...] → matches NostosEngine's
        // writeBatch Vec<JsValue> shape. Returns the outbox ids in order.
        // wasm-bindgen returns Vec<f64> as a Float64Array; normalize to a plain
        // Array so the postMessage boundary carries JSON-friendly values (the
        // Dart + JS consumers both expect a regular array).
        if (!sock) {
          self.postMessage({ id, error: "not connected" });
          break;
        }
        const ops = (m.ops ?? []).map((o) => ({
          table: o.table,
          op: o.op,
          pk: o.pk,
          payloadJson: o.payloadJson ?? null,
        }));
        const writeIds = Array.from(sock.writeBatch(ops));
        self.postMessage({ id, ok: true, writeIds });
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
        // nostos_data); default to [] when Dart omits it.
        schemaTables = (m.tables ?? []).map((t) => ({
          name: t.name,
          primary_key: t.primaryKey ?? [],
          columns: t.columns ?? [],
        }));
        // Pre-open (sock null): stashed above, applied on the next connect.
        if (sock) {
          sock.applySchema(schemaTables);
        }
        self.postMessage({ id, ok: true });
        break;
      }
      case "setCrdtTables": {
        // Wave 4c: tag which tables are OR-set / counter CRDTs so apply_local
        // merges instead of clobbering. Delegates to NostosSocket.setCrdtTables
        // (→ the engine's set_crdt_tables). Call BEFORE any CRDT verb.
        if (sock) {
          sock.setCrdtTables(m.orSet ?? [], m.counter ?? []);
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
          await bootP;
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
          await bootP;
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

// ---- Multi-tab follower proxy (2026-09-21) --------------------------------
// A tab that lost the leader lock no longer runs a private memory engine with
// its own socket: it forwards every command over a BroadcastChannel to the
// leader tab's Worker (one OPFS handle, one socket — a single shared-engine
// shape, without a SharedWorker: SharedWorkerGlobalScope exposes no `Worker`
// and opfs-sahpool needs a dedicated worker's FileSystemSyncAccessHandle).
// Responses route back by request id; pushes (snapshot / status / writeResult
// / storage) mirror to every follower, so a follower's page sees the leader's
// storage mode with reason "follower". When the leader tab closes, the Web
// Lock queue promotes a follower: it opens OPFS and replays its own tab's
// last `connect`. `allowSecondaryTab: true` on connect opts a tab OUT into
// the old standalone memory engine.
// ponytail: requests in flight at a leader change are lost (no retry); a
// promoted follower whose tab never called connect waits for one to.
const BUS = new BroadcastChannel("nostos:multitab");
const MY_ID = Math.random().toString(36).slice(2);
let standalone = false; // allowSecondaryTab: own memory engine, no proxying
let lastConnect = null; // this tab's last connect request, replayed on promotion
const busInflight = new Map(); // leader: negative gid → { from, id }
let busNextId = -1;
const sticky = new Map(); // leader: last storage / status push, replayed on hello
const localHandler = self.onmessage;
const pagePost = self.postMessage.bind(self);
const isFollower = () => storageReason === "secondary-tab" && !standalone;

self.postMessage = (msg) => {
  if (msg.id != null && busInflight.has(msg.id)) {
    // Leader answering a follower's request: back over the bus, original id.
    const { from, id } = busInflight.get(msg.id);
    busInflight.delete(msg.id);
    BUS.postMessage({ bus: "res", to: from, msg: { ...msg, id } });
    return;
  }
  if (isFollower() && msg.type === "storage") {
    // Our own "memory / secondary-tab" boot push: ask the leader instead.
    BUS.postMessage({ bus: "hello", from: MY_ID });
    return;
  }
  pagePost(msg);
  if (msg.type && !isFollower() && !standalone) {
    if (msg.type === "storage" || msg.type === "status") sticky.set(msg.type, msg);
    BUS.postMessage({ bus: "push", msg });
  }
};

BUS.onmessage = (ev) => {
  const b = ev.data || {};
  if (isFollower()) {
    if (b.bus === "res" && b.to === MY_ID) pagePost(b.msg);
    else if (b.bus === "push")
      pagePost(b.msg.type === "storage" ? { ...b.msg, reason: "follower" } : b.msg);
    return;
  }
  if (storageReason === "secondary-tab") return; // standalone: not on the bus
  if (b.bus === "hello") {
    for (const s of sticky.values()) BUS.postMessage({ bus: "push", msg: s });
  } else if (b.bus === "req") {
    let m = b.msg;
    if (m.id != null) {
      const gid = busNextId--;
      busInflight.set(gid, { from: b.from, id: m.id });
      m = { ...m, id: gid };
    }
    localHandler({ data: m });
  }
};

self.onmessage = async (ev) => {
  const m = ev.data || {};
  await bootP; // leadership is known only after boot
  if (m.cmd === "connect") {
    lastConnect = m;
    if (m.allowSecondaryTab) standalone = true;
  }
  if (!isFollower()) return localHandler(ev);
  BUS.postMessage({ bus: "req", from: MY_ID, msg: m });
};

// Queue for promotion: granted only once the leader's Worker (and with it the
// lock) is gone; then held for this Worker's lifetime.
bootP.then(() => {
  if (storageReason !== "secondary-tab" || !navigator.locks) return;
  navigator.locks.request(LEADER_LOCK, async () => {
    if (standalone) return; // release straight through to the next in line
    leaderLockHeld = true;
    storageReason = null;
    await initStorage(); // opens OPFS now the old leader released it; pushes storage
    if (lastConnect) localHandler({ data: { ...lastConnect, id: undefined } });
    await new Promise(() => {});
  });
});
