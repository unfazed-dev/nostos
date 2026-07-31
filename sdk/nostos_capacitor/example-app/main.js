// Nostos Capacitor example app entry.
//
// Imports the compiled plugin from /dist/index.js (the static server maps
// /dist/* to nostos_capacitor/dist/*). The plugin's index.js in turn uses
// registerPlugin('Nostos', { web: () => import('./web') }) — the lazy web
// loader resolves to /dist/web.js, which dynamically imports the wasm glue.
//
// The Playwright spec loads this page with ?syncUrl=ws://127.0.0.1:<port>/sync
// so the spine's port can be injected per-run.

import { Nostos } from "/dist/index.js";

const params = new URLSearchParams(location.search);
const syncUrl = params.get("syncUrl");
const statusEl = document.getElementById("status");

function setStatus(text) {
  if (statusEl) statusEl.textContent = text;
}

if (!syncUrl) {
  setStatus("No ?syncUrl= provided.");
  console.log("[cap-e2e] NO_SYNC_URL");
} else {
  try {
    // Point the plugin at the wasm glue. The static server maps /pkg-web/* to
    // sdk/nostos_web/pkg-web/* (the --target web wasm-pack output).
    await Nostos.configure({ wasmUrl: "/pkg-web/nostos_ffi_wasm.js" });

    const { rowCount, checkpoint } = await Nostos.connect({
      url: syncUrl,
      token: null,
      table: "tasks",
      whereSql: null,
    });

    console.log("[cap-e2e] WASM_READY");
    console.log(
      "[cap-e2e] CONNECTED rowCount=" + rowCount + " checkpoint=" + checkpoint,
    );
    setStatus("Connected; rowCount=" + rowCount);

    // Expose the plugin to the Playwright evaluate() calls so the spec can
    // drive write/query without re-importing.
    window.Nostos = Nostos;

    // Reactive watch(): the listener fires immediately with the engine's
    // current rows (kind === "initial"). See README "Reactive watch()" for the
    // delta ceiling — the wasm change-callback seam is not yet wired.
    window.__nostosWatchSub = await Nostos.watch(
      { table: "tasks" },
      ({ kind, rows }) => {
        console.log(
          "[cap-e2e] WATCH_" +
            kind.toUpperCase() +
            " count=" +
            (rows ? rows.length : 0),
        );
      },
    );
  } catch (e) {
    setStatus("Init FAILED: " + ((e && e.message) || String(e)));
    console.log(
      "[cap-e2e] INIT_FAILED: " + ((e && e.message) || String(e)),
    );
  }
}
