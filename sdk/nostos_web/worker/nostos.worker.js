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

// Read the engine's current full-table snapshot and push it to the main thread.
// Used both for the initial snapshot (on `watch`) and for each change tick
// (onChange → rowsFor). Pure postMessage; safe when sock is null (empty push).
function postSnapshot() {
  const rows = sock
    ? sock.rowsFor(table).map((r) => ({ pk: r.pk, payload: r.payload }))
    : [];
  self.postMessage({ type: "snapshot", table, rows });
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

// Eager-init on boot so the main thread sees WASM_READY before any command.
// Failures surface as an unsolicited error (the spec's WASM_READY poll times out).
void ensureWasm().catch((e) =>
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
        sock = await NostosSocket.connect(
          m.url,
          m.token ?? null,
          m.table,
          m.where_sql ?? null,
        );
        lastRowCount = sock.rowCount;
        startPolling();
        // Reactive push (ADR-0024): register the Rust→JS callback. NostosSocket
        // fires it synchronously from the onmessage pump on every commit (a
        // change tick) + once now (initial). Gated by `watching` so a connected
        // session with no main-thread watcher stays quiet. The tick is the push;
        // rowsFor is the fresh-snapshot read.
        sock.onChange(() => {
          if (watching) {
            try {
              postSnapshot();
            } catch (_) {
              /* socket torn down between tick + read — ignore */
            }
          }
        });
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
