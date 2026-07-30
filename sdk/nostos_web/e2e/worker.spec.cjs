// Playwright E2E for the WS1 dual-entry Worker probe (ADR-0017).
//
// Validates the load-bearing WS1 toolchain unknown WITHOUT the spine binary:
// the existing `wasm-pack --target web` artifact boots inside a module Worker
// (where `window` is absent) and round-trips one postMessage. If this passes,
// option (c) — one shared artifact, JS-layer dual entry — is proven and full
// WS1 can proceed on this foundation (no separate crate, no feature flag).
//
// Run only this spec (it needs no spine):
//   npx playwright test --config=playwright.config.cjs e2e/worker.spec.cjs
"use strict";

const { test, expect } = require("@playwright/test");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
// Authoritative artifact location: `wasm-pack ... --out-dir pkg-web` writes
// into the crate dir (see package.json build:web). The static server below
// exposes it under /pkg-web/*.
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
      try {
        const urlPath = decodeURIComponent(
          new URL(req.url, "http://x").pathname,
        );
        let filePath;
        if (urlPath.startsWith("/pkg-web/")) {
          filePath = path.join(PKG_WEB, urlPath.slice("/pkg-web/".length));
        } else {
          // /e2e/worker.html, /worker/nostos.worker.js, etc.
          filePath = path.join(WEB_SDK, urlPath);
        }
        fs.readFile(filePath, (err, data) => {
          if (err) {
            res.writeHead(404);
            res.end("not found: " + urlPath);
            return;
          }
          res.writeHead(200, {
            "Content-Type": MIME[path.extname(filePath)] ||
              "application/octet-stream",
          });
          res.end(data);
        });
      } catch (e) {
        res.writeHead(500);
        res.end(String(e));
      }
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port }),
    );
  });
}

test("WS1 dual-entry: pkg-web wasm boots in a Worker and round-trips ping", async ({
  page,
}) => {
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push("PAGEERROR " + String(e)));

  const srv = await startStaticServer();
  try {
    await page.goto(`http://127.0.0.1:${srv.port}/e2e/worker.html`, {
      waitUntil: "load",
    });

    // 1. The wasm-bindgen artifact initialised inside the Worker.
    await expect
      .poll(
        () => logs.some((l) => l === "[worker-e2e] WASM_READY_IN_WORKER"),
        { timeout: 15000, message: "WASM_READY_IN_WORKER marker" },
      )
      .toBe(true);

    // 2. The postMessage round-trip completed AND the wasm executed (checkpoint
    //    came back as a number — a value only a live NostosEngine can produce).
    await expect
      .poll(
        () => logs.some((l) => l.startsWith("[worker-e2e] PONG checkpoint=")),
        { timeout: 15000, message: "PONG round-trip marker" },
      )
      .toBe(true);

    // 3. No worker-level error fired.
    expect(
      logs.some((l) => l.startsWith("[worker-e2e] WORKER_ERROR")),
      "no WORKER_ERROR in console",
    ).toBe(false);

    // 4. Re-read the struct from the page to assert the wasm really ran: the
    //    checkpoint must be a number (0 until the first committed batch).
    const seen = await page.evaluate(() => window.__seen);
    expect(seen, "window.__seen exposed").toBeTruthy();
    expect(seen.pong, "pong received").toBeTruthy();
    expect(typeof seen.pong.checkpoint, "checkpoint is a number").toBe(
      "number",
    );
  } finally {
    const ok =
      logs.some((l) => l === "[worker-e2e] WASM_READY_IN_WORKER") &&
      logs.some((l) => l.startsWith("[worker-e2e] PONG checkpoint="));
    if (!ok) {
      console.log("[worker-e2e] captured console:");
      logs.forEach((l) => console.log("    | " + l));
    }
    srv.server.close();
  }
});
