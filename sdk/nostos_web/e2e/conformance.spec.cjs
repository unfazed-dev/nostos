// The browser-Worker leg of `nostos_core::conformance` (direct-mode plan, step 8).
//
// The same cases run three times, once per Storage implementation:
//   - `InMemoryStorage`  — nostos-core's own unit tests
//   - `SqliteStorage`    — crates/nostos-client/tests/conformance_sqlite.rs
//   - `SqliteWasmStorage`— here, in a real Worker over OPFS
//
// The third one is not redundant. OPFS SQLite is a separate implementation with
// its own transaction boundaries, and the properties these cases hold are the
// ones whose failure is invisible: a transaction applied in pieces, an echoed
// page applied twice, a horizon that ran ahead of its rows. A device with any
// of those looks healthy and has wrong data.
//
// Requires a bundle built WITH the suite — it is behind an off-by-default cargo
// feature so the cases never ship in an app's `.wasm` (ADR-0015's size budget):
//
//   wasm-pack build crates/nostos-ffi-wasm --target web --features conformance
//
// Against a bundle built without it the Worker replies with an explanatory
// error and this spec skips rather than failing, so the default `make ci`
// bundle does not turn red for a missing test-only export.

"use strict";

const { test, expect } = require("@playwright/test");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const PKG_WEB = path.join(REPO_ROOT, "crates", "nostos-ffi-wasm", "pkg-web");
const WEB_SDK = path.join(REPO_ROOT, "sdk", "nostos_web");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      const urlPath = decodeURIComponent(new URL(req.url, "http://x").pathname);
      const filePath = urlPath.startsWith("/pkg-web/")
        ? path.join(PKG_WEB, urlPath.slice("/pkg-web/".length))
        : path.join(WEB_SDK, urlPath);
      fs.readFile(filePath, (err, data) => {
        if (err) {
          res.writeHead(404);
          res.end("not found: " + urlPath);
          return;
        }
        res.writeHead(200, {
          "Content-Type": MIME[path.extname(filePath)] || "application/octet-stream",
        });
        res.end(data);
      });
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

test("the conformance suite passes on the Worker's own storage backend", async ({ page }) => {
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push("PAGEERROR " + String(e)));

  const srv = await startStaticServer();
  try {
    await page.goto(`http://127.0.0.1:${srv.port}/e2e/conformance.html`, {
      waitUntil: "load",
    });

    await expect
      .poll(() => page.evaluate(() => window.__seen?.cases ?? window.__seen?.error ?? null), {
        timeout: 30000,
        message: "the Worker reported a conformance result",
      })
      .not.toBeNull();

    const seen = await page.evaluate(() => window.__seen);
    test.skip(
      typeof seen.error === "string" && seen.error.includes("no conformance suite"),
      "bundle built without --features conformance",
    );

    expect(seen.error, `conformance failed: ${seen.error}`).toBeFalsy();
    expect(seen.cases, "every case ran").toEqual([
      "a_multi_table_transaction_is_never_seen_in_pieces",
      "an_echoed_page_applies_twice_with_the_same_result",
      "the_horizon_never_runs_ahead_of_the_rows",
      "a_resnapshot_leaves_no_stale_horizon",
      "a_snapshot_reaps_rows_deleted_while_away",
    ]);
    // The whole point of the browser leg: it has to have run on OPFS, not on
    // the in-memory degrade path, or it proves nothing the Rust legs did not.
    expect(seen.mode, "cases ran on the durable OPFS backend").toBe("durable");
  } finally {
    if (!logs.some((l) => l.startsWith("[conformance] COVERED"))) {
      console.log("[conformance] captured console:");
      logs.forEach((l) => console.log("    | " + l));
    }
    srv.server.close();
  }
});
