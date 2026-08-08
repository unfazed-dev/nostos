// Playwright browser smoke for the Flutter-web Worker (ADR-0036).
//
// Proves — in a REAL browser (headless Chromium), not Node — that the
// Flutter-web Worker asset (`sdk/nostos_flutter/web/nostos/nostos_worker.js`) boots
// the shared `nostos-ffi-wasm` backend, connects to a live `nostos-server`
// (e2e_server), applies a write, and pushes the reactive snapshot back through
// WebNostosEngine's boundary protocol. The Dart WebNostosEngine protocol layer is
// covered by test/engine_web_test.dart (8 VM tests); this covers the JS Worker
// half + the wasm + the live socket round-trip.
//
// Reuses the e2e_server + static-HTTP pattern from sdk/nostos_web/e2e, but serves
// the FLUTTER-WEB worker + harness. Storage mode is whatever resolves: durable
// (OPFS) if @sqlite.org/sqlite-wasm is served, else memory (the Safari-Private-
// Browsing degrade — also a valid assertion target). The smoke does NOT depend
// on durable mode; it asserts connect + write + snapshot in whichever mode.
//
// Run (from repo root, after `cd sdk/nostos_web && npm install`):
//   NODE_PATH=sdk/nostos_web/node_modules \
//     npx playwright test --config=sdk/nostos_flutter/web/e2e/playwright.config.cjs

"use strict";

const { test, expect } = require("@playwright/test");
const { spawn } = require("node:child_process");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..", "..");
const SPINE_EXE = path.join(
  REPO_ROOT,
  "target",
  "debug",
  "examples",
  "e2e_server",
);
const PKG_WEB = path.join(REPO_ROOT, "crates", "nostos-ffi-wasm", "pkg-web");
const FLUTTER_WEB = path.join(REPO_ROOT, "sdk", "nostos_flutter", "web", "nostos");
const E2E_DIR = path.join(REPO_ROOT, "sdk", "nostos_flutter", "web", "e2e");
const SQLITE_WASM_NODE = path.join(
  REPO_ROOT,
  "sdk",
  "nostos_web",
  "node_modules",
  "@sqlite.org",
  "sqlite-wasm",
);

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
};

// Static server: harness at /, worker+glue+wasm under /nostos/, sqlite-wasm under
// /node_modules/@sqlite.org/sqlite-wasm/ (so the glue's ../node_modules import
// resolves relative to /nostos/).
function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
        let filePath;
        if (urlPath === "/") {
          filePath = path.join(E2E_DIR, "flutter_web_smoke.html");
        } else if (urlPath.startsWith("/nostos/")) {
          const rel = urlPath.slice("/nostos/".length);
          // worker + glue live in web/nostos/; the wasm .js/.wasm in pkg-web.
          const tryFlutter = path.join(FLUTTER_WEB, rel);
          filePath = fs.existsSync(tryFlutter)
            ? tryFlutter
            : path.join(PKG_WEB, rel);
        } else if (urlPath.startsWith("/node_modules/@sqlite.org/sqlite-wasm/")) {
          filePath = path.join(
            SQLITE_WASM_NODE,
            urlPath.slice("/node_modules/@sqlite.org/sqlite-wasm/".length),
          );
        } else {
          res.writeHead(404);
          res.end("not found: " + urlPath);
          return;
        }
        const rooted = filePath.startsWith(REPO_ROOT);
        if (!rooted) {
          res.writeHead(403);
          res.end("forbidden");
          return;
        }
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
      } catch (e) {
        res.writeHead(500);
        res.end(String(e));
      }
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      resolve({ server, port: server.address().port });
    });
  });
}

function startSpine() {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(SPINE_EXE)) {
      reject(new Error("e2e_server not found: cargo build -p nostos-infra --example e2e_server"));
      return;
    }
    const child = spawn(SPINE_EXE, [], { stdio: ["ignore", "pipe", "inherit"] });
    let port = null;
    let buffer = "";
    child.stdout.on("data", (chunk) => {
      buffer += chunk.toString();
      let idx;
      while ((idx = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, idx).trim();
        buffer = buffer.slice(idx + 1);
        if (line.startsWith("NOSTOS_E2E_PORT=")) {
          port = parseInt(line.slice("NOSTOS_E2E_PORT=".length), 10);
        }
        if (line === "NOSTOS_E2E_READY" && port !== null) {
          resolve({ child, port });
          return;
        }
      }
    });
    child.on("error", reject);
    child.on("exit", (code, signal) =>
      reject(new Error("spine exited before READY (code=" + code + " signal=" + signal + ")")));
    setTimeout(() => reject(new Error("spine never signaled READY (30s)")), 30000).unref();
  });
}

test("Flutter-web Worker: connect + write + reactive snapshot (ADR-0036)", async ({ page }) => {
  test.setTimeout(90000);

  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  page.on("pageerror", (err) =>
    logs.push("[pageerror] " + (err && err.message ? err.message : String(err))),
  );

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log("[flutter-web-smoke] spine port", spine.port, "; static", staticServer.port);

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/`, { waitUntil: "load" });

    // Wait for the harness to expose its driver.
    await page.waitForFunction(() => typeof window.nostosConnect === "function", null, {
      timeout: 10000,
    });

    // Worker reports a storage mode (durable OR memory) on boot — either is fine.
    const mode = await page.waitForFunction(() => window.nostosStorage !== null, null, {
      timeout: 15000,
    }).then(() => page.evaluate(() => window.nostosStorage));
    console.log("[flutter-web-smoke] storage mode:", mode);
    expect(["durable", "memory"]).toContain(mode);

    // Connect to the live server.
    await page.evaluate((u) => window.nostosConnect(u, "tasks"), wsUrl);
    await expect
      .poll(() => page.evaluate(() => window.nostosConnected), { timeout: 15000 })
      .toBe(true);

    // Watch the table (reactive snapshots), then write a row.
    await page.evaluate(() => window.nostosWatch("tasks"));
    const writeRes = await page.evaluate(() =>
      window.nostosWrite("tasks", "smoke-1", JSON.stringify({ title: "smoke row", done: false })),
    );
    expect(writeRes.ok).toBe(true);
    expect(typeof writeRes.writeId).toBe("number");

    // The server echoes the write back; the Worker pushes a snapshot containing
    // the row. Poll for the row appearing in any tasks snapshot.
    await expect
      .poll(
        async () => {
          const snaps = await page.evaluate(() => window.nostosSnapshots);
          return snaps.some(
            (s) => s.table === "tasks" && s.json && s.json.includes("smoke-1"),
          );
        },
        { timeout: 20000 },
      )
      .toBe(true);

    // No fatal worker/page errors.
    const pageErrors = logs.filter((l) => l.startsWith("[pageerror]") || /worker.onerror/.test(l));
    expect(pageErrors, "page errors: " + pageErrors.join(" | ")).toEqual([]);
  } finally {
    await staticServer.server.close();
    spine.child.kill("SIGTERM");
  }
});
