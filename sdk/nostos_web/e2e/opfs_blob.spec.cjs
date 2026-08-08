// Playwright browser E2E for OpfsBlobStore (ADR-0034 / T6).
//
// OpfsBlobStore (attachments.js) is browser-only — its constructor throws when
// `navigator.storage` is absent (i.e. in Node) — so the node attachments suite
// (e2e/attachments.spec.cjs) substitutes an in-memory blob store and the REAL
// class is never exercised. This spec loads the REAL attachments.js source via a
// CJS shim in headless Chromium and drives put/get/remove/wipe against real OPFS.
//
// It uses the async FileSystemAccessHandle API (getDirectory + createWritable),
// which is available in a window context — no Worker required (unlike the
// opfs-sahpool sync handle the metadata store uses). No spine binary needed.
//
// Run: npx playwright test --config=playwright.config.cjs e2e/opfs_blob.spec.cjs

"use strict";

const { test, expect } = require("@playwright/test");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const WEB_SDK = path.join(REPO_ROOT, "sdk", "nostos_web");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
};

// Minimal static server over sdk/nostos_web so the harness can fetch
// /attachments.js and load /e2e/opfs_blob.html same-origin.
function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent(new URL(req.url, "http://x").pathname);
        const filePath = path.join(WEB_SDK, urlPath);
        if (!filePath.startsWith(WEB_SDK) || !fs.existsSync(filePath)) {
          res.writeHead(404);
          res.end("not found");
          return;
        }
        const data = fs.readFileSync(filePath);
        res.writeHead(200, {
          "Content-Type": MIME[path.extname(filePath)] || "application/octet-stream",
          // OPFS requires a secure context; headless Chromium treats 127.0.0.1
          // as secure, but cross-origin isolation is NOT needed for the async
          // FileSystemAccessHandle API (only opfs-sahpool needs COOP/COEP).
        });
        res.end(data);
      } catch (err) {
        res.writeHead(500);
        res.end(String(err));
      }
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
    server.on("error", reject);
  });
}

test("OpfsBlobStore put/get/remove/wipe works against real OPFS", async ({ browser }) => {
  const server = await startStaticServer();
  const port = server.address().port;
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    const errors = [];
    page.on("console", (m) => {
      if (m.type() === "error") errors.push(m.text());
    });
    page.on("pageerror", (e) => errors.push(String(e)));

    await page.goto(`http://127.0.0.1:${port}/e2e/opfs_blob.html`);
    // #out flips from "running…" to PASS/FAIL once the harness completes.
    await page.waitForFunction(
      () => {
        const t = document.getElementById("out");
        return t && /^(PASS|FAIL)/.test(t.textContent || "");
      },
      { timeout: 15000 },
    );
    const result = await page.textContent("#out");
    expect(result, `console errors: ${errors.join(" | ")}`).toMatch(/^PASS/);
  } finally {
    await context.close();
    server.close();
  }
});
