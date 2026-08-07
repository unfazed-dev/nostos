// nostos.worker.js — WS1 Worker host (ADR-0017).
//
// This module Worker is the SOLE wasm host for the browser SDK. It owns the
// live connection (`NostosSocket` — web-sys WebSocket + the apply engine +
// `InMemoryStorage`, which impls `Storage` AND `Outbox`). The main thread is a
// pure `postMessage` proxy that imports NO wasm — it sends the boundary
// commands below and listens for the push events.
//
// Dual-entry foundation (slice 1, still proven by worker.spec.cjs): this is a
// *consumer* of the same `nostos_ffi_wasm.js` `--target web` artifact the main
// thread used to load directly. One shared artifact, JS-layer dual entry — no
// second crate, no feature flag.
//
// Boundary protocol (main <-> Worker):
//   main -> Worker (requests, each carries `id` except write):
//     {id, cmd:"connect", url, token?, table, where_sql?}
//     {      cmd:"write", table, op, pk, payload_json?, client_write_id}  (no id)
//     {id, cmd:"rowsFor", table}
//     {id, cmd:"checkpoint"}
//     {id, cmd:"close"}
//     {id, cmd:"watch", table}           reactive subscribe (ADR-0024)
//     {id, cmd:"unwatch"}                reactive unsubscribe
//     {id, cmd:"signOut"}                ADR-0029: clearLocalState + close + drop token
//     {id, cmd:"setToken", token}        ADR-0029: cache token; if live, reconnect with it
//   Worker -> main:
//     {id, ok:true, ...}                 response to a request
//     {id, error:"..."}                  response error
//     {type:"wasm-ready"}                (also {id:0,ok:"wasm-ready"} — slice-1 compat)
//     {type:"status", connected}
//     {type:"rowsChanged", count}        engine row count changed (legacy poll signal)
//     {type:"snapshot", table, rows}     reactive push: full-table snapshot on each
//                                        change tick (NostosSocket.onChange — ADR-0024)
//     {type:"writeResult", client_write_id, ok, error?}  async write outcome
//
// Reactive push (ADR-0024): `snapshot` is the TRUE Rust→JS push —
// `NostosSocket.onChange` is a wasm-bindgen Closure nostos fires synchronously
// from the onmessage frame-pump on every commit (initial snapshot + deltas).
// The legacy `rowsChanged` rowCount poll is kept as a compat signal; `snapshot`
// is the reactive primitive the main-thread `watch()` facade renders from.
//
// `unsafe`-free: pure JS. The Rust write path uses the real `Outbox` trait
// (enqueue + apply_local + flush loop), so a write never throws when the socket
// is closed — it is captured + rendered locally and ships on (re)connect.
//
// ADR-0033 (browser-durable storage): on boot, the Worker ALSO async-inits
// sqlite-wasm with opfs-sahpool. On success → durable mode (rows + outbox +
// checkpoint survive reload). On failure (Safari Private Browsing, old
// browsers, OPFS disallowed) → degrade to InMemoryStorage (today's behavior),
// surfaced on SyncStatus. The db_handle is passed to NostosSocket.connect as
// the 5th arg; the Rust side wraps it in WebStorage::SqliteWasm.
import init, { NostosSocket } from "../pkg-web/nostos_ffi_wasm.js";

let wasmReady = false;
let sock = null;
let lastRowCount = -1;
let pollTimer = null;
// The connected table (single-table v1) + whether the main thread is watching.
// onChange fires on every commit regardless; `watching` gates the forward so a
// connected-but-unwatched session doesn't spam the main thread with snapshots.
let table = null;
let watching = false;
// ADR-0029: cached connection params + token so `setToken` can reconnect the
// socket with a refreshed JWT and `signOut` can drop them. NostosSocket has no
// wasm-level token swap (the token is baked into the WS handshake at connect),
// so a token refresh IS a reconnect at this layer.
let connParams = null; // { url, table, where_sql } captured on connect
let token = null;      // opaque JWT; cleared on signOut

// ADR-0033: the durable SQLite-WASM db handle (or null in memory/degrade mode).
// Set by initStorage() on boot. Passed to NostosSocket.connect as the 5th arg.
let dbHandle = null;
// "durable" (OPFS-backed SQLite-WASM) or "memory" (InMemoryStorage fallback).
// Reported to the main thread via {type:"storage", mode} so SyncStatus can
// surface which backend is active.
let storageMode = "memory";

// Read the engine's current full-table snapshot and push it to the main thread.
// Used both for the initial snapshot (on `watch`) and for each change tick
// (onChange → rowsFor). Pure postMessage; safe when sock is null (empty push).
function postSnapshot() {
  const rows = sock
    ? sock.rowsFor(table).map((r) => ({ pk: r.pk, payload: r.payload }))
    : [];
  self.postMessage({ type: "snapshot", table, rows });
}

// Wire a freshly-opened socket's reactive push: seed the legacy row-count
// baseline, start the compat poll, and register the Rust→JS onChange callback
// (ADR-0024) that forwards a fresh snapshot on every change tick when a main-
// thread watcher is attached. Shared by `connect` and `setToken` reconnect so
// the onChange closure (with its `watching` gate) is defined once.
function attachChangePush(s) {
  lastRowCount = s.rowCount;
  startPolling();
  s.onChange(() => {
    if (watching) {
      try {
        postSnapshot();
      } catch (_) {
        /* socket torn down between tick + read — ignore */
      }
    }
  });
}

async function ensureWasm() {
  if (!wasmReady) {
    await init();
    wasmReady = true;
    // Slice-1-compat shape ({id:0, ok:"wasm-ready"}); app.html's proxy also
    // surfaces this so the spec can wait for WASM_READY before connecting.
    self.postMessage({ id: 0, ok: "wasm-ready" });
  }
}

// ADR-0033: async-init sqlite-wasm with opfs-sahpool on boot. On success →
// durable mode (dbHandle set, storageMode="durable"). On failure → degrade to
// memory mode (dbHandle stays null, InMemoryStorage is the backend). The mode
// is pushed to the main thread so SyncStatus can surface it. NOT a crash — the
// memory path is the explicit degrade fallback (ADR-0017 follow-up scope 5).
async function initStorage() {
  try {
    const { openNostosDb } = await import("./sqlite_wasm_glue.js");
    dbHandle = await openNostosDb();
    storageMode = "durable";
  } catch (e) {
    // OPFS unavailable (Safari Private Browsing, old browsers) or sqlite-wasm
    // package not installed (Node smoke — but the Worker never runs there).
    // Degrade to InMemoryStorage: dbHandle stays null, NostosSocket.connect
    // receives null as the 5th arg → NostosEngine::new() (memory path).
    console.error("[nostos.worker] storage init failed:", (e && e.message) || e);
    dbHandle = null;
    storageMode = "memory";
  }
  self.postMessage({ type: "storage", mode: storageMode });
}

// Eager-init on boot: wasm first (the engine host), then sqlite-wasm (the
// durable backend). The main thread sees WASM_READY + {type:"storage"} before
// any command. Wasm init failure is fatal (the spec's WASM_READY poll times
// out); storage init failure is a graceful degrade (memory mode).
void ensureWasm()
  .then(() => initStorage())
  .catch((e) =>
    self.postMessage({ id: 0, error: "wasm-init: " + String((e && e.message) || e) }),
  );

function startPolling() {
  if (pollTimer) {
    return;
  }
  // ponytail: rowsChanged is a LEGACY signal driven by polling rowCount here.
  // The reactive primitive is now NostosSocket.onChange (the `snapshot` push,
  // registered in `connect`) — that is the TRUE Rust→JS push fired on every
  // commit. This poll is retained only as a compat signal for the existing E2E
  // spec; a follow-on can delete it once the spec migrates to `snapshot`.
  pollTimer = setInterval(() => {
    if (!sock) {
      return;
    }
    try {
      const n = sock.rowCount;
      if (n !== lastRowCount) {
        lastRowCount = n;
        self.postMessage({ type: "rowsChanged", count: n });
      }
    } catch (_) {
      /* socket torn down between checks — ignore */
    }
  }, 150);
}

function stopPolling() {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

self.onmessage = async (ev) => {
  const m = ev.data || {};

  // ---- slice-1 ping probe (kept verbatim for worker.spec.cjs) ----
  if (m.cmd === "ping") {
    await ensureWasm();
    self.postMessage({ id: m.id, ok: "pong", checkpoint: 0 });
    return;
  }

  try {
    switch (m.cmd) {
      case "connect": {
        await ensureWasm();
        table = m.table;
        token = m.token ?? null;
        connParams = { url: m.url, table: m.table, where_sql: m.where_sql ?? null };
        // ADR-0033: pass dbHandle as the 5th arg. When non-null (durable mode),
        // the Rust side creates WebStorage::SqliteWasm; when null (memory mode),
        // it creates WebStorage::Memory (the degrade fallback).
        sock = await NostosSocket.connect(
          m.url,
          token,
          table,
          connParams.where_sql,
          dbHandle,
        );
        attachChangePush(sock);
        self.postMessage({ id: m.id, ok: true, checkpoint: sock.checkpoint });
        self.postMessage({ type: "status", connected: true });
        break;
      }
      case "write": {
        // No request id: write is fire-and-forget; the outcome is the
        // writeResult push (correlated by client_write_id). NostosSocket.write
        // returns Ok once the write is captured (enqueued + apply_local'd) —
        // it does NOT throw when the socket is closed (WS1 contract).
        if (!sock) {
          self.postMessage({
            type: "writeResult",
            client_write_id: m.client_write_id,
            ok: false,
            error: "not connected",
          });
          break;
        }
        sock.write(m.table, m.op, m.pk, m.payload_json ?? null, m.client_write_id);
        self.postMessage({
          type: "writeResult",
          client_write_id: m.client_write_id,
          ok: true,
        });
        break;
      }
      case "rowsFor": {
        const rows = sock
          ? sock.rowsFor(m.table).map((r) => ({ pk: r.pk, payload: r.payload }))
          : [];
        self.postMessage({ id: m.id, ok: true, rows });
        break;
      }
      case "checkpoint": {
        self.postMessage({ id: m.id, ok: true, checkpoint: sock ? sock.checkpoint : 0 });
        break;
      }
      case "watch": {
        // Reactive subscribe (ADR-0024). ACK first, then push the INITIAL
        // snapshot: NostosSocket.onChange already fired its initial tick at
        // connect (before any watcher existed), so synthesize one here so the
        // caller renders immediately, before the first delta. Subsequent ticks
        // (deltas) arrive via the onChange closure registered in `connect`.
        watching = true;
        self.postMessage({ id: m.id, ok: true });
        try {
          postSnapshot();
        } catch (_) {
          /* not yet connected — initial snapshot arrives on connect's tick */
        }
        break;
      }
      case "unwatch": {
        watching = false;
        self.postMessage({ id: m.id, ok: true });
        break;
      }
      case "close": {
        if (sock) {
          try {
            sock.offChange();
          } catch (_) {
            /* noop if never registered */
          }
          try {
            sock.close();
          } catch (_) {
            /* already closed */
          }
          sock = null;
          stopPolling();
        }
        watching = false;
        table = null;
        self.postMessage({ id: m.id, ok: true });
        self.postMessage({ type: "status", connected: false });
        break;
      }
      case "signOut": {
        // ADR-0029 D1 / WS4-D3: wipe the engine's rows + outbox
        // (`clearLocalState` clears BOTH under one borrow — half a clear is a
        // cross-user leak), close the socket, and drop the cached token +
        // subscription so the next user on this same Worker session sees none
        // of the previous user's rows or pending writes.
        //
        // ADR-0033: in durable mode, ALSO wipe the OPFS SQLite DB (clearAll
        // deletes rows + outbox + resets checkpoint to '0'). The main thread
        // is responsible for clearing localStorage checkpoint keys (Workers
        // cannot access localStorage). The token is dropped here + in the
        // main-thread proxy.
        if (sock) {
          try {
            sock.clearLocalState();
          } catch (_) {
            /* engine already torn down — nothing to wipe */
          }
          try {
            sock.offChange();
          } catch (_) {
            /* noop if never registered */
          }
          try {
            sock.close();
          } catch (_) {
            /* already closed */
          }
          sock = null;
          stopPolling();
        }
        // Wipe the durable OPFS store so the next principal starts fresh.
        if (dbHandle) {
          try {
            dbHandle.clearAll();
          } catch (_) {
            /* db already closed — nothing to wipe */
          }
        }
        watching = false;
        table = null;
        connParams = null;
        token = null;
        lastRowCount = -1;
        self.postMessage({ id: m.id, ok: true });
        self.postMessage({ type: "status", connected: false });
        break;
      }
      case "setToken": {
        // ADR-0029 §3: swap the auth token. NostosSocket has no wasm-level token
        // swap (the token is baked into the WS handshake at connect), so a
        // refresh = cache the new token and, if a session is live, reconnect
        // the socket with it (same url/table/where_sql). If not yet connected,
        // just cache it for the next `connect`.
        token = m.token ?? null;
        if (sock && connParams) {
          const rp = connParams;
          try {
            sock.offChange();
          } catch (_) {
            /* noop if never registered */
          }
          try {
            sock.close();
          } catch (_) {
            /* already closed */
          }
          sock = null;
          stopPolling();
          await ensureWasm();
          sock = await NostosSocket.connect(
            rp.url,
            token,
            rp.table,
            rp.where_sql,
            dbHandle,
          );
          attachChangePush(sock);
          self.postMessage({ id: m.id, ok: true, checkpoint: sock.checkpoint });
          self.postMessage({ type: "status", connected: true });
        } else {
          self.postMessage({ id: m.id, ok: true });
        }
        break;
      }
      default:
        self.postMessage({ id: m.id, error: "unknown cmd: " + String(m.cmd) });
    }
  } catch (e) {
    const msg = String((e && e.message) || e);
    if (m.cmd === "write") {
      self.postMessage({
        type: "writeResult",
        client_write_id: m.client_write_id,
        ok: false,
        error: msg,
      });
    } else {
      self.postMessage({ id: m.id, error: msg });
    }
  }
};
