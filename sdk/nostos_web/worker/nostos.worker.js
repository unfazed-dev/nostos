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
//   Worker -> main:
//     {id, ok:true, ...}                 response to a request
//     {id, error:"..."}                  response error
//     {type:"wasm-ready"}                (also {id:0,ok:"wasm-ready"} — slice-1 compat)
//     {type:"status", connected}
//     {type:"rowsChanged", count}        engine row count changed (server push / echo)
//     {type:"writeResult", client_write_id, ok, error?}  async write outcome
//
// `unsafe`-free: pure JS. The Rust write path uses the real `Outbox` trait
// (enqueue + apply_local + flush loop), so a write never throws when the socket
// is closed — it is captured + rendered locally and ships on (re)connect.
import init, { NostosSocket } from "../pkg-web/nostos_ffi_wasm.js";

let wasmReady = false;
let sock = null;
let lastRowCount = -1;
let pollTimer = null;

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
  // ponytail: rowsChanged is driven by polling rowCount here. The production
  // version adds a Rust callback fired from the onmessage pump (a later slice);
  // polling is sufficient + honest for the live E2E (latency ~150ms).
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
        sock = await NostosSocket.connect(
          m.url,
          m.token ?? null,
          m.table,
          m.where_sql ?? null,
        );
        lastRowCount = sock.rowCount;
        startPolling();
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
      case "close": {
        if (sock) {
          try {
            sock.close();
          } catch (_) {
            /* already closed */
          }
          sock = null;
          stopPolling();
        }
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
