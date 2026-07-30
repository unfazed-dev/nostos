// nostos.worker.js — WS1 dual-entry foundation (ADR-0017).
//
// This module Worker is a *consumer* of the SAME `nostos_ffi_wasm.js`
// `--target web` artifact the main-thread live demo loads today. It is NOT a
// second Rust build, NOT a feature-gated cdylib, and NOT a separate crate.
//
// Why that works (the load-bearing WS1 unknown, validated by worker.spec.cjs):
//   - wasm-bindgen `--target web` emits one ES module + one `.wasm`. Its
//     `init()` locates the wasm via `new URL('nostos_ffi_wasm_bg.wasm',
//     import.meta.url)` + `fetch()` — both available in a module Worker
//     exactly as on the main thread.
//   - The generated glue has ZERO references to `window`/`document` (verified
//     by grep over pkg-web/nostos_ffi_wasm.js), so module-eval is Worker-safe.
//   - `nostos-ffi-wasm` already produces two build targets from one crate
//     (`--target nodejs` -> pkg-node, `--target web` -> pkg-web); a Worker is
//     a third *consumer* of the web artifact, not a third build target.
//
// In full WS1 the main thread becomes a thin `postMessage` proxy that imports
// NO wasm; this Worker becomes the sole host of the engine + WS transport +
// SqliteWasmStorage. This file is the seed of that host.
//
// Thin-slice protocol (proves the Worker<->main boundary + the wasm boot):
//   main  ->  { id, cmd: "ping" }
//   worker -> { id:0, ok: "wasm-ready" }                  (unsolicited, once)
//   worker -> { id, ok: "pong", checkpoint: <number> }    (checkpoint proves
//                                                          the wasm executed)
//
// `unsafe`-free: this is pure JS; the Rust crate stays workspace-clean.
import init, { NostosEngine } from "../pkg-web/nostos_ffi_wasm.js";

let engine = null;
let wasmReady = false;

async function ensureWasm() {
  if (!wasmReady) {
    // Fetches nostos_ffi_wasm_bg.wasm via import.meta.url. In a module Worker
    // import.meta.url resolves to this script's URL, so the sibling ../pkg-web
    // wasm is found. Throws here if the artifact is NOT Worker-safe — the
    // exact failure this slice exists to catch.
    await init();
    wasmReady = true;
    self.postMessage({ id: 0, ok: "wasm-ready" });
  }
}

self.onmessage = async (ev) => {
  const { id, cmd } = ev.data || {};
  try {
    if (cmd === "ping") {
      await ensureWasm();
      if (!engine) {
        engine = new NostosEngine();
      }
      // checkpoint is 0 until the first committed batch. Returning it as a
      // number proves the wasm executed inside the Worker and returned a live
      // engine value — not merely that the script parsed.
      self.postMessage({ id, ok: "pong", checkpoint: engine.checkpoint });
      return;
    }
    self.postMessage({ id, error: "unknown cmd: " + String(cmd) });
  } catch (e) {
    self.postMessage({ id, error: String((e && e.message) || e) });
  }
};
