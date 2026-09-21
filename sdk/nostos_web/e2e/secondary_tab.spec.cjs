// Playwright browser E2E for the multi-tab leader guard (2026-09-21).
//
// opfs-sahpool is one instance per origin (sqlite.org/wasm persistence.md), so
// a second tab used to degrade to memory SILENTLY with its own live socket —
// two diverging local states. The Worker now takes a Web Lock before opening
// OPFS. This spec pins the contract:
//   1. Tab 1 boots "durable".
//   2. Tab 2 (same origin, same browser context) boots "memory" with
//      reason "secondary-tab" and `connect` REJECTS.
//   3. `connect` with allowSecondaryTab:true is accepted (the opt-in).
//   4. Closing tab 1 releases the lock: a fresh tab boots "durable" again.
//
// No spine needed: the refusal happens before any socket is opened, and the
// opt-in connect targets a closed port (the write path never runs here).

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
};

function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
      const filePath = urlPath.startsWith("/pkg-web/")
        ? path.join(PKG_WEB, urlPath.slice("/pkg-web/".length))
        : path.join(WEB_SDK, urlPath);
      if (!filePath.startsWith(REPO_ROOT)) {
        res.writeHead(403);
        return res.end();
      }
      fs.readFile(filePath, (err, data) => {
        if (err) {
          res.writeHead(404);
          return res.end("not found: " + urlPath);
        }
        res.writeHead(200, {
          "Content-Type": MIME[path.extname(filePath)] || "application/octet-stream",
        });
        res.end(data);
      });
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port }),
    );
  });
}

// Open app.html in a new page of `context`, wait for the Worker's storage
// push, and return {page, mode, reason}.
async function bootTab(context, origin) {
  const page = await context.newPage();
  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  await page.goto(origin + "/e2e/app.html", { waitUntil: "domcontentloaded" });
  await expect
    .poll(() => logs.some((l) => l.startsWith("[web-e2e] STORAGE_MODE=")), {
      timeout: 20000,
      message: "STORAGE_MODE reported by Worker",
    })
    .toBe(true);
  const pick = (k) => {
    const line = logs.find((l) => l.startsWith("[web-e2e] " + k + "="));
    return line ? line.split("=")[1] : null;
  };
  return { page, mode: pick("STORAGE_MODE"), reason: pick("STORAGE_REASON") };
}

test("second tab loses the OPFS leader lock, refuses connect, opt-in allowed, lock released on close", async ({
  context,
}) => {
  test.setTimeout(60000);
  const { server, port } = await startStaticServer();
  const origin = `http://127.0.0.1:${port}`;
  try {
    const tab1 = await bootTab(context, origin);
    console.log("[secondary-tab-e2e] tab1:", tab1.mode, tab1.reason);
    expect(tab1.mode).toBe("durable");

    const tab2 = await bootTab(context, origin);
    console.log("[secondary-tab-e2e] tab2:", tab2.mode, tab2.reason);
    expect(tab2.mode).toBe("memory");
    expect(tab2.reason).toBe("secondary-tab");

    // Default: refused, loudly.
    await expect(
      tab2.page.evaluate(() =>
        window.nostos.connect("ws://127.0.0.1:1/sync", null, "tasks", null),
      ),
    ).rejects.toThrow(/secondary-tab/);

    // Opt-in: accepted (memory engine; the socket to :1 fails, which is fine —
    // the guard is what's under test, not the transport).
    const optIn = await tab2.page.evaluate(() =>
      window.nostos
        .connect("ws://127.0.0.1:1/sync", null, "tasks", null, true)
        .then(() => "ok", (e) => String(e)),
    );
    expect(optIn).not.toMatch(/secondary-tab/);

    // Close the leader; the lock is released with its Worker, so a new tab is
    // durable again.
    await tab1.page.close();
    await tab2.page.close();
    const tab3 = await bootTab(context, origin);
    console.log("[secondary-tab-e2e] tab3:", tab3.mode, tab3.reason);
    expect(tab3.mode).toBe("durable");
    await tab3.page.close();
  } finally {
    server.close();
  }
});
