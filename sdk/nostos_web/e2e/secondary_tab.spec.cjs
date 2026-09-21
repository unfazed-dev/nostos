// Playwright browser E2E for multi-tab (2026-09-21): leader guard + follower proxy.
//
// opfs-sahpool is one instance per origin (sqlite.org/wasm persistence.md). The
// Worker takes a Web Lock before opening OPFS; the loser becomes a FOLLOWER that
// proxies every command to the leader's Worker over a BroadcastChannel. This
// spec pins the contract:
//   1. Tab 1 boots "durable"; tab 2 boots "durable" with reason "follower".
//   2. Both `connect` succeed (tab 2's joins the leader's live session).
//   3. A write from tab 2 lands in tab 1's `rowsFor` — one engine, one store.
//   4. Tab 2's own requests (checkpoint) resolve in tab 2 — per-tab routing.
//   5. `allowSecondaryTab:true` opts a tab OUT into a standalone memory engine.
//   6. Closing tab 1 promotes tab 2: it opens OPFS (the row is still there),
//      replays its own connect, and reports "durable" with no reason.
// Needs the spine (the writes go over a real socket).

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

// Same shape as browser_live.spec.cjs: the spine prints NOSTOS_E2E_PORT= then
// NOSTOS_E2E_READY on stdout.
function startSpine() {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(SPINE_EXE)) {
      reject(new Error("spine binary not found at " + SPINE_EXE + " — run `cargo build -p nostos-infra --example e2e_server` first."));
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

test("multi-tab: follower proxies to the leader, opt-out stays standalone, follower is promoted when the leader closes", async ({
  context,
}) => {
  test.setTimeout(90000);
  const { server, port } = await startStaticServer();
  const spine = await startSpine();
  const origin = `http://127.0.0.1:${port}`;
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  try {
    const tab1 = await bootTab(context, origin);
    const tab2 = await bootTab(context, origin);
    console.log("[multitab-e2e] tab1:", tab1.mode, tab1.reason, "| tab2:", tab2.mode, tab2.reason);
    expect(tab1.mode).toBe("durable");
    expect(tab1.reason).toBeNull();
    expect(tab2.mode).toBe("durable");
    expect(tab2.reason).toBe("follower");

    const c1 = await tab1.page.evaluate((u) => window.nostos.connect(u, null, "tasks", null), wsUrl);
    expect(c1.ok).toBe(true);
    const c2 = await tab2.page.evaluate((u) => window.nostos.connect(u, null, "tasks", null), wsUrl);
    expect(c2.ok).toBe(true);

    // Write in the follower → visible in the leader: one engine, one store.
    const pk = "multitab-" + Date.now();
    await tab2.page.evaluate(
      (pk) => window.nostos.write("tasks", "upsert", pk, JSON.stringify({ title: "from follower" }), "w-" + pk),
      pk,
    );
    const pksIn = (tab) => async () => {
      const r = await tab.page.evaluate(() => window.nostos.rowsFor("tasks"));
      return (r && r.rows ? r.rows : []).map((x) => x.pk);
    };
    await expect
      .poll(pksIn(tab1), { timeout: 15000, intervals: [100, 250, 500], message: "leader sees the follower's row" })
      .toContain(pk);
    // The follower's own request ids resolve in the follower (per-tab routing).
    const cp = await tab2.page.evaluate(() => window.nostos.checkpoint());
    expect(cp.ok).toBe(true);

    // Opt-out: a standalone memory engine with its own socket.
    const tab3 = await bootTab(context, origin);
    expect(tab3.reason).toBe("follower");
    const c3 = await tab3.page.evaluate((u) => window.nostos.connect(u, null, "tasks", null, true), wsUrl);
    expect(c3.ok).toBe(true);
    await tab3.page.evaluate(() => window.nostos.close());
    await tab3.page.close();

    // Leader gone → the queued follower is promoted: OPFS opens (the row
    // survived, it was durable), its connect is replayed, mode is reported
    // as a plain "durable".
    await tab1.page.close();
    await expect
      .poll(
        () =>
          tab2.page.evaluate(() =>
            window.nostos.events.some((e) => e.type === "storage" && e.mode === "durable" && !e.reason),
          ),
        { timeout: 20000, message: "tab 2 promoted to leader" },
      )
      .toBe(true);
    await expect
      .poll(pksIn(tab2), { timeout: 15000, intervals: [100, 250, 500], message: "promoted leader still has the row" })
      .toContain(pk);
    console.log("[multitab-e2e] tab2 promoted, row survived");
    await tab2.page.evaluate(() => window.nostos.close());
    await tab2.page.close();
  } finally {
    server.close();
    spine.child.kill("SIGTERM");
  }
});
