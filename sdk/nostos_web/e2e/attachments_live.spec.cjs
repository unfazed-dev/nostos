// Playwright browser E2E: Attachments driver through the LIVE Worker gateway.
//
// Proves the one integration the node suite (e2e/attachments.spec.cjs) cannot:
// the LIVE WorkerAttachmentGateway reading/writing the synced `attachments`
// metadata table through the in-Worker wasm store (the node FakeGateway "stands
// in for" this). The blob plane (OpfsBlobStore + a fake adapter) stays on the
// main thread; only metadata crosses to the Worker. Reuses the browser_live
// spine + static-server harness; connects to `tasks` (known to the spine) and
// writes/reads `attachments` locally — the local-apply path (apply_local) is
// what's exercised, independent of server write-back for the blob plane.
//
// Run: npx playwright test --config=playwright.config.cjs e2e/attachments_live.spec.cjs

"use strict";

const { test, expect } = require("@playwright/test");
const { spawn } = require("node:child_process");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const SPINE_EXE = path.join(REPO_ROOT, "target", "debug", "examples", "e2e_server");
const PKG_WEB = path.join(REPO_ROOT, "crates", "nostos-ffi-wasm", "pkg-web");
const WEB_SDK = path.join(REPO_ROOT, "sdk", "nostos_web");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
};

function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
        let filePath;
        if (urlPath.startsWith("/pkg-web/")) {
          filePath = path.join(PKG_WEB, urlPath.slice("/pkg-web/".length));
        } else if (urlPath === "/") {
          filePath = path.join(WEB_SDK, "e2e", "app.html");
        } else {
          filePath = path.join(WEB_SDK, urlPath);
        }
        if (!filePath.startsWith(REPO_ROOT)) {
          res.writeHead(403);
          res.end("forbidden");
          return;
        }
        fs.readFile(filePath, (err, data) => {
          if (err) {
            res.writeHead(404);
            res.end("not found");
            return;
          }
          res.writeHead(200, { "Content-Type": MIME[path.extname(filePath)] || "application/octet-stream" });
          res.end(data);
        });
      } catch (e) {
        res.writeHead(500);
        res.end(String(e));
      }
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

function startSpine() {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(SPINE_EXE)) {
      reject(new Error("spine binary not found at " + SPINE_EXE));
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
        if (line.startsWith("NOSTOS_E2E_PORT=")) port = parseInt(line.slice("NOSTOS_E2E_PORT=".length), 10);
        if (line === "NOSTOS_E2E_READY" && port !== null) {
          resolve({ child, port });
          return;
        }
      }
    });
    child.on("error", reject);
    child.on("exit", (code, signal) => reject(new Error("spine exited before READY (code=" + code + " signal=" + signal + ")")));
    setTimeout(() => reject(new Error("spine never signaled READY (30s)")), 30000).unref();
  });
}

test("Attachments driver: queueUpload → pump → synced through the live Worker gateway", async ({ page }) => {
  test.setTimeout(60000);
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push("[pageerror] " + ((e && e.message) || e)));

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log("[att-live] spine on port", spine.port, "; static on", staticServer.port);

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/app.html`, { waitUntil: "domcontentloaded" });
    await expect.poll(() => logs.some((l) => l === "[web-e2e] PROXY_READY"), { timeout: 10000 }).toBe(true);
    await expect.poll(() => logs.some((l) => l === "[web-e2e] WASM_READY"), { timeout: 20000 }).toBe(true);

    // Connect (creates the in-Worker sock; subscribe to `tasks` — the attachment
    // metadata is written/read locally to `attachments`, independent of subscribe).
    await page.evaluate(async (url) => {
      await window.nostos.connect(url, null, "tasks", null);
    }, wsUrl);
    await page.waitForTimeout(400);

    const result = await page.evaluate(async () => {
      // CJS-shim load the real attachments.js + worker_attachment_gateway.js.
      const load = async (p) => {
        const src = await fetch(p).then((r) => r.text());
        const module = { exports: {} };
        // eslint-disable-next-line no-new-func
        new Function("module", "exports", "require", src)(module, module.exports, () => { throw new Error("unexpected require"); });
        return module.exports;
      };
      const A = await load("/attachments.js");
      const G = await load("/worker_attachment_gateway.js");

      // Fake adapter (blob plane) — captures uploads; no real Supabase here.
      const uploaded = new Map();
      const adapter = {
        async upload(path, bytes) { uploaded.set(path, Uint8Array.from(bytes)); },
        async download(path) { return uploaded.get(path); },
        async delete(path) { uploaded.delete(path); },
      };

      const gateway = new G.WorkerAttachmentGateway(window.nostos, { table: "attachments" });
      const a = new A.Attachments({
        gateway,
        adapter,
        blobStore: new A.OpfsBlobStore("nostos-att-live"),
        isOnline: async () => true,
      });

      const bytes = new Uint8Array([11, 22, 33, 44, 55]);
      const id = "att-live-1";
      await a.queueUpload({ filename: "live.txt", bytes, mediaType: "text/plain", id });
      await a.pump();

      const state = await gateway.currentState(id);
      const got = uploaded.get(id);
      const stillQueued = (await gateway.queuedRows()).length;
      return {
        state,
        uploaded: got ? Array.from(got) : null,
        stillQueued,
      };
    });

    expect(result.state, `state should be synced; logs: ${logs.join(" | ")}`).toBe("synced");
    expect(result.uploaded, "fake adapter should have received the blob bytes").toEqual([11, 22, 33, 44, 55]);
    expect(result.stillQueued, "no rows should remain queued after pump").toBe(0);

    await page.evaluate(() => window.nostos.close());
  } finally {
    if (test.info().status !== "passed") {
      console.log("[att-live] captured page console:");
      for (const line of logs) console.log("    | " + line);
    }
    staticServer.server.close();
    try { spine.child.kill("SIGTERM"); } catch (_) { /* dead */ }
  }
});
