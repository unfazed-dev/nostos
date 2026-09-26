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
const ATLET_WEB = path.join(REPO_ROOT, "apps", "atlet", "flutter", "web", "nostos");
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
    const executionTokens = [];
    const state = { accountAvailable: true, revokedTokens: new Set(), accountDelayMs: {},
      failWasm: false, failSqlite: false };
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
        if (state.failWasm && urlPath === "/nostos/nostos_ffi_wasm_bg.wasm") {
          res.writeHead(503);
          res.end("wasm unavailable");
          return;
        }
        if (state.failSqlite && urlPath === "/nostos/sqlite_wasm_glue.js") {
          res.writeHead(503);
          res.end("sqlite unavailable");
          return;
        }
        let filePath;
        if (urlPath === "/account") {
          if (!state.accountAvailable) {
            res.writeHead(503);
            res.end("offline");
            return;
          }
          const jwt = String(req.headers["x-appwrite-jwt"] || "");
          const respond = () => {
            if (state.revokedTokens.has(jwt)) {
              res.writeHead(401);
              res.end("revoked");
              return;
            }
            const user = jwt.startsWith("token-a") ? "user-a" : "user-b";
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ $id: user, status: true }));
          };
          const delay = state.accountDelayMs[jwt] || 0;
          if (delay) setTimeout(respond, delay);
          else respond();
          return;
        } else if (urlPath === "/functions/test-function/executions") {
          executionTokens.push(String(req.headers["x-appwrite-jwt"] || ""));
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ responseStatusCode: 500,
            responseBody: JSON.stringify({ message: "mock transient failure" }) }));
          return;
        } else if (urlPath === "/") {
          filePath = path.join(E2E_DIR, "flutter_web_smoke.html");
        } else if (urlPath.startsWith("/nostos/")) {
          const rel = urlPath.slice("/nostos/".length);
          // worker + glue live in web/nostos/; the wasm .js/.wasm in pkg-web.
          const tryFlutter = path.join(FLUTTER_WEB, rel);
          const tryAtlet = path.join(ATLET_WEB, rel);
          filePath = fs.existsSync(tryFlutter)
            ? tryFlutter
            : fs.existsSync(tryAtlet) ? tryAtlet : path.join(PKG_WEB, rel);
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
      resolve({ server, port: server.address().port, executionTokens, state });
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

    // Connecting asks this page to host the dedicated OPFS worker. The broker
    // then reports its storage mode over this tab's private port.
    await page.evaluate((u) => window.nostosConnect(u, "tasks"), wsUrl);
    const mode = await page.waitForFunction(() => window.nostosStorage !== null, null, {
      timeout: 15000,
    }).then(() => page.evaluate(() => window.nostosStorage));
    console.log("[flutter-web-smoke] storage mode:", mode);
    expect(["durable", "memory"]).toContain(mode);

    // The live server connection and reactive status are now observable.
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
            (s) => s.table === "tasks" && s.json && s.json.includes("smoke-1") &&
              s.json.includes("smoke row"),
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

test("Flutter-web Worker rejects mismatched JWT before opening cached Appwrite rows", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`, { waitUntil: "load" });
    const result = await page.evaluate(async (url) => {
      function start() {
        const worker = new Worker("/nostos/nostos_worker.js", { type: "module" });
        const messages = [];
        const pending = new Map();
        let nextId = 1;
        worker.onmessage = ({ data }) => {
          messages.push(data);
          if (pending.has(data.id)) {
            pending.get(data.id)(data);
            pending.delete(data.id);
          }
        };
        const request = (message) => new Promise((resolve) => {
          const id = nextId++;
          pending.set(id, resolve);
          worker.postMessage({ ...message, id });
        });
        return { worker, messages, request };
      }
      const connect = (token) => ({ cmd: "connect", url, provider: "appwrite",
        projectId: "test-project", functionId: "test-function", userId: "user-a",
        token, tables: [{ name: "tasks" }] });
      const first = start();
      const firstReply = await first.request(connect("token-a"));
      first.worker.postMessage({ cmd: "watch", table: "tasks" });
      const write = await first.request({ cmd: "write", table: "tasks", op: "upsert",
        pk: "private-a", payloadJson: JSON.stringify({ title: "private-a-title" }) });
      await first.request({ cmd: "close" });
      first.worker.terminate();
      await new Promise((resolve) => setTimeout(resolve, 250));

      const wrong = start();
      const wrongReply = await wrong.request(connect("token-b"));
      wrong.worker.postMessage({ cmd: "watch", table: "tasks" });
      await new Promise((resolve) => setTimeout(resolve, 100));
      const wrongRows = wrong.messages.filter((m) => m.type === "snapshot");
      wrong.worker.terminate();
      await new Promise((resolve) => setTimeout(resolve, 250));

      const restored = start();
      const restoredReply = await restored.request(connect("token-a"));
      restored.worker.postMessage({ cmd: "watch", table: "tasks" });
      await new Promise((resolve) => setTimeout(resolve, 100));
      const restoredRows = restored.messages.filter((m) => m.type === "snapshot");
      await restored.request({ cmd: "close" });
      restored.worker.terminate();
      return { firstReply, write, wrongReply, wrongRows, restoredReply, restoredRows };
    }, endpoint);
    expect(result.firstReply.ok).toBe(true);
    expect(result.write.ok).toBe(true);
    expect(result.wrongReply.error).toMatch(/token does not match/i);
    expect(result.wrongRows.every((row) => !row.json.includes("private-a-title"))).toBe(true);
    expect(result.restoredReply.ok).toBe(true);
    expect(result.restoredRows.some((row) => row.json.includes("private-a-title"))).toBe(true);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web Worker refuses wipe ACK when WASM bootstrap fails", async ({ page }) => {
  test.setTimeout(30000);
  const staticServer = await startStaticServer();
  try {
    staticServer.state.failWasm = true;
    await page.goto(`http://127.0.0.1:${staticServer.port}/`);
    const result = await page.evaluate(() => new Promise((resolve, reject) => {
      const worker = new Worker("/nostos/nostos_worker.js", { type: "module" });
      const timer = setTimeout(() => {
        worker.terminate();
        reject(new Error("worker did not answer failed-bootstrap wipe"));
      }, 15000);
      worker.onmessage = ({ data }) => {
        if (data.type === "storage" && data.error) {
          worker.postMessage({ id: 1, cmd: "signOut" });
        }
        if (data.id === 1) {
          clearTimeout(timer);
          worker.terminate();
          resolve(data);
        }
      };
    }));
    expect(result.error).toMatch(/storage unavailable for local wipe/);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web Worker refuses durable wipe ACK during OPFS fallback", async ({ page }) => {
  test.setTimeout(30000);
  const staticServer = await startStaticServer();
  try {
    staticServer.state.failSqlite = true;
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    const result = await page.evaluate(async (url) => {
      const worker = new Worker("/nostos/nostos_worker.js", { type: "module" });
      const pending = new Map();
      let nextId = 1;
      let storage = null;
      worker.onmessage = ({ data }) => {
        if (data.type === "storage") storage = data;
        if (pending.has(data.id)) {
          pending.get(data.id)(data);
          pending.delete(data.id);
        }
      };
      const request = (message) => new Promise((resolve) => {
        const id = nextId++;
        pending.set(id, resolve);
        worker.postMessage({ ...message, id });
      });
      const connected = await request({ cmd: "connect", url, provider: "appwrite",
        projectId: "test-project", functionId: "test-function", userId: "user-a",
        token: "token-a", tables: [{ name: "tasks" }] });
      const wiped = await request({ cmd: "signOut" });
      worker.terminate();
      return { connected, wiped, storage };
    }, endpoint);
    expect(result.connected.ok).toBe(true);
    expect(result.storage.mode).toBe("memory");
    expect(result.storage.reason).toBe("opfs-unavailable");
    expect(result.wiped.error).toMatch(/storage unavailable for local wipe/);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web broker promotes a validated Appwrite tab after the OPFS host closes", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  const follower = await page.context().newPage();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    const firstWrite = await page.evaluate(() =>
      window.nostosWrite("tasks", "host-row", JSON.stringify({ title: "host-private-row" })));
    expect(firstWrite.ok).toBe(true);

    await follower.goto(`${endpoint}/`);
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await follower.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("host-private-row"))), { timeout: 10000 }).toBe(true);
    await follower.evaluate(() => { window.nostosSnapshots = []; });

    await page.evaluate(() => window.nostosClose());
    await page.close();
    await expect.poll(() => follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("host-private-row"))), { timeout: 15000 }).toBe(true);
    const secondWrite = await follower.evaluate(() =>
      window.nostosWrite("tasks", "promoted-row", JSON.stringify({ title: "promoted-write" })));
    expect(secondWrite.ok).toBe(true);
  } finally {
    await follower.close().catch(() => {});
    await staticServer.server.close();
  }
});

test("Flutter-web broker retires wiped engine before another account signs in", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  const follower = await page.context().newPage();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => window.nostosWrite("tasks", "a-row",
      JSON.stringify({ title: "only-user-a" })));
    await follower.goto(`${endpoint}/`);
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await follower.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("only-user-a"))), { timeout: 10000 }).toBe(true);

    await page.evaluate(() => window.nostosSignOut());
    await expect.poll(() => follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json === "[]")), { timeout: 10000 }).toBe(true);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-b", "user-b"), endpoint);
    const bWrite = await page.evaluate(() => window.nostosWrite("tasks", "b-row",
      JSON.stringify({ title: "only-user-b" })));
    expect(bWrite.ok).toBe(true);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("only-user-b"))), { timeout: 10000 }).toBe(true);
    expect(await follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("only-user-b")))).toBe(false);
  } finally {
    await follower.close().catch(() => {});
    await staticServer.server.close();
  }
});

test("Flutter-web broker lets a follower sign out and switch accounts", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  const follower = await page.context().newPage();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => { window.nostosRetireDelayMs = 600; });
    await follower.goto(`${endpoint}/`);
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await follower.evaluate(() => window.nostosSignOut());
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-b", "user-b"), endpoint);
    const bWrite = await follower.evaluate(() => window.nostosWrite("tasks", "follower-b-row",
      JSON.stringify({ title: "follower-user-b" })));
    expect(bWrite.ok).toBe(true);
    await follower.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => follower.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("follower-user-b"))), { timeout: 10000 }).toBe(true);
    expect(await page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("follower-user-b")))).toBe(false);
  } finally {
    await follower.close().catch(() => {});
    await staticServer.server.close();
  }
});

test("Flutter-web broker accepts a fresh JWT after a tab lease expires", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    const shortToken = await page.evaluate(() =>
      `token-a.${btoa(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + 2 }))}.sig`);
    await page.evaluate(({ url, token }) => window.nostosConnectAppwrite(url, token),
      { url: endpoint, token: shortToken });
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => window.nostosWrite("tasks", "lease-row",
      JSON.stringify({ title: "restored-after-lease" })));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("restored-after-lease"))), { timeout: 10000 }).toBe(true);
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots.at(-1)?.json === "[]"),
      { timeout: 10000 }).toBe(true);
    await page.evaluate(() => { window.nostosSnapshots = []; });
    await page.evaluate(() => window.nostosSetToken("token-a"));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("restored-after-lease"))), { timeout: 10000 }).toBe(true);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web broker wipes expired session offline before fresh sign-in", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    const shortToken = await page.evaluate(() =>
      `token-a.${btoa(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + 2 }))}.sig`);
    await page.evaluate(({ url, token }) => window.nostosConnectAppwrite(url, token),
      { url: endpoint, token: shortToken });
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => window.nostosWrite("tasks", "wipe-row",
      JSON.stringify({ title: "must-be-wiped" })));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("must-be-wiped"))), { timeout: 10000 }).toBe(true);
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots.at(-1)?.json === "[]"),
      { timeout: 10000 }).toBe(true);
    staticServer.state.accountAvailable = false;
    await page.evaluate(() => window.nostosSignOut());
    staticServer.state.accountAvailable = true;
    await page.evaluate(() => { window.nostosSnapshots = []; });
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json === "[]")), { timeout: 10000 }).toBe(true);
    expect(await page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("must-be-wiped")))).toBe(false);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web broker retries offline wipe after a silent Worker death", async ({ page }) => {
  test.setTimeout(65000);
  const staticServer = await startStaticServer();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => window.nostosWrite("tasks", "dead-worker-row",
      JSON.stringify({ title: "wipe-after-worker-death" })));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("wipe-after-worker-death"))), { timeout: 10000 }).toBe(true);
    staticServer.state.accountAvailable = false;
    await page.evaluate(() => window.nostosCrashEngine());
    const wiped = await page.evaluate(() => window.nostosSignOut());
    expect(wiped.ok).toBe(true);
    staticServer.state.accountAvailable = true;
    await page.evaluate(() => { window.nostosSnapshots = []; });
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json === "[]")), { timeout: 10000 }).toBe(true);
    expect(await page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("wipe-after-worker-death")))).toBe(false);
  } finally {
    await staticServer.server.close();
  }
});

test("Flutter-web broker restores host bearer when a follower closes", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  const follower = await page.context().newPage();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await expect.poll(() => staticServer.executionTokens.includes("token-a"),
      { timeout: 10000 }).toBe(true);
    await follower.goto(`${endpoint}/`);
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-a2"), endpoint);
    await follower.evaluate(() => window.nostosSetToken("token-a2"));
    await expect.poll(() => staticServer.executionTokens.includes("token-a2"),
      { timeout: 10000 }).toBe(true);
    const handoffStart = staticServer.executionTokens.length;
    await follower.evaluate(() => window.nostosClose());
    await follower.close();
    await page.evaluate(() => window.nostosWrite("tasks", "handoff-row",
      JSON.stringify({ title: "host-bearer" })));
    await expect.poll(() => staticServer.executionTokens.slice(handoffStart).includes("token-a"),
      { timeout: 10000 }).toBe(true);
  } finally {
    await follower.close().catch(() => {});
    await staticServer.server.close();
  }
});

test("Flutter-web broker resumes cloud sync when a new tab joins after pause", async ({ page }) => {
  test.setTimeout(90000);
  const staticServer = await startStaticServer();
  const follower = await page.context().newPage();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await expect.poll(() => staticServer.executionTokens.includes("token-a"),
      { timeout: 10000 }).toBe(true);
    await page.evaluate(() => window.nostosDisconnect());
    await page.evaluate(() => window.nostosSetToken("token-a2"));
    const pausedAt = staticServer.executionTokens.length;
    await follower.goto(`${endpoint}/`);
    await follower.evaluate((url) => window.nostosConnectAppwrite(url, "token-a3"), endpoint);
    await follower.evaluate(() => window.nostosWrite("tasks", "resumed-row",
      JSON.stringify({ title: "after-pause" })));
    await expect.poll(() => staticServer.executionTokens.slice(pausedAt).includes("token-a3"),
      { timeout: 10000 }).toBe(true);
  } finally {
    await follower.close().catch(() => {});
    await staticServer.server.close();
  }
});

test("Flutter-web broker restores an authenticated engine after Worker failure", async ({ page }) => {
  test.setTimeout(70000);
  const staticServer = await startStaticServer();
  try {
    const endpoint = `http://127.0.0.1:${staticServer.port}`;
    await page.goto(`${endpoint}/`);
    await page.evaluate((url) => window.nostosConnectAppwrite(url, "token-a"), endpoint);
    await page.evaluate(() => window.nostosWatch("tasks"));
    await page.evaluate(() => window.nostosWrite("tasks", "recovery-row",
      JSON.stringify({ title: "survives-worker-failure" })));
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("survives-worker-failure"))), { timeout: 10000 }).toBe(true);
    await page.evaluate(() => window.nostosCrashEngine());
    await expect(page.evaluate(() => window.nostosWrite("tasks", "timed-out-row",
      JSON.stringify({ title: "timed-out" })))).rejects.toThrow(/timed out/);
    await page.evaluate(() => { window.nostosSnapshots = []; });
    staticServer.state.revokedTokens.add("token-a");
    staticServer.state.accountDelayMs["token-a"] = 500;
    const refresh = await page.evaluate(async () => {
      const pendingWrite = window.nostosWrite("tasks", "failed-old-bearer",
        JSON.stringify({ title: "old-bearer" })).then(() => "ok", () => "rejected");
      await new Promise((resolve) => setTimeout(resolve, 30));
      const rotated = await window.nostosSetToken("token-a2");
      return { rotated, pendingWrite: await pendingWrite };
    });
    expect(refresh.rotated.ok).toBe(true);
    expect(refresh.pendingWrite).toBe("rejected");
    await expect.poll(() => page.evaluate(() => window.nostosSnapshots
      .some((s) => s.json.includes("survives-worker-failure"))), { timeout: 10000 }).toBe(true);
  } finally {
    await staticServer.server.close();
  }
});

// Wave 4c (ADR-0036): proves the CRDT + atomic-writeBatch delegates on
// NostosSocket ship over the live socket — counterIncrement, orSetAdd, and
// writeBatch each enqueue (HLC mint / nostos-domain) and return an outbox id,
// exercising the new Worker commands + the wasm delegates end-to-end in a real
// browser. Merge correctness is covered by the nostos-ffi-wasm host tests; this
// proves the verbs are reachable + ship from the connected Worker path.
test("Flutter-web Worker: CRDT + writeBatch delegates ship (Wave 4c)", async ({ page }) => {
  test.setTimeout(90000);

  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  page.on("pageerror", (err) =>
    logs.push("[pageerror] " + (err && err.message ? err.message : String(err))),
  );

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log("[flutter-web-smoke-4c] spine port", spine.port, "; static", staticServer.port);

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/`, { waitUntil: "load" });
    await page.waitForFunction(() => typeof window.nostosConnect === "function", null, {
      timeout: 10000,
    });
    // Connect + tag the CRDT tables before any CRDT verb (the loud-fail gate).
    await page.evaluate((u) => window.nostosConnect(u, "tasks"), wsUrl);
    await page.waitForFunction(() => window.nostosStorage !== null, { timeout: 15000 });
    await expect
      .poll(() => page.evaluate(() => window.nostosConnected), { timeout: 15000 })
      .toBe(true);
    await page.evaluate(() => window.nostosSetCrdtTables(["tags"], ["likes"]));

    // counterIncrement: enqueues a counter RMW (HLC + counter_apply_delta),
    // returns the outbox id, ships over the open socket.
    const counterRes = await page.evaluate(() =>
      window.nostosCounterIncrement("likes", "post1", 5),
    );
    expect(counterRes.ok).toBe(true);
    expect(typeof counterRes.writeId).toBe("number");

    // orSetAdd: mints an HLC, builds the OrSetPayload, enqueues + ships.
    const orSetRes = await page.evaluate(() =>
      window.nostosOrSetAdd("tags", "row1", "alice"),
    );
    expect(orSetRes.ok).toBe(true);
    expect(typeof orSetRes.writeId).toBe("number");

    // writeBatch: atomic enqueue (one storage txn) of two ops, ships each.
    const batchRes = await page.evaluate(() =>
      window.nostosWriteBatch([
        { table: "tasks", op: "upsert", pk: "batch-1", payloadJson: JSON.stringify({ n: 1 }) },
        { table: "tasks", op: "upsert", pk: "batch-2", payloadJson: JSON.stringify({ n: 2 }) },
      ]),
    );
    expect(batchRes.ok).toBe(true);
    expect(Array.isArray(batchRes.writeIds)).toBe(true);
    expect(batchRes.writeIds.length).toBe(2);

    const pageErrors = logs.filter((l) => l.startsWith("[pageerror]") || /worker.onerror/.test(l));
    expect(pageErrors, "page errors: " + pageErrors.join(" | ")).toEqual([]);
  } finally {
    await staticServer.server.close();
    spine.child.kill("SIGTERM");
  }
});

// Reload-persistence proof for the durable web backend. Wave 4c's test above
// proves counter/orSet/writeBatch SHIP on the live socket (happy path); this
// proves an atomic writeBatch's rows SURVIVE a full page reload in durable OPFS
// — i.e. SqliteWasmStorage.enqueue_batch's transactional commit lands in OPFS
// (nostos_data) and the re-spawned Worker resumes from it. (Plain-write reload
// persistence is already covered by sdk/nostos_web/e2e/durable.spec.cjs; this
// closes the writeBatch-specific gap.) Storage-internal not covered here —
// enqueue_batch rollback-on-failure and migrate_outbox_dlq mirror the native
// SqliteStorage and aren't reachable for failure-injection via the public
// surface; the happy path here exercises the same commit path.
test("Flutter-web Worker: writeBatch rows survive reload in durable OPFS", async ({ page }) => {
  test.setTimeout(120000);

  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  page.on("pageerror", (err) =>
    logs.push("[pageerror] " + (err && err.message ? err.message : String(err))),
  );

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log("[flutter-web-smoke-reload] spine port", spine.port, "; static", staticServer.port);

  const pks = ["persist-a", "persist-b", "persist-c"];
  const allPresent = async () => {
    const snaps = await page.evaluate(() => window.nostosSnapshots);
    const tasks = snaps
      .filter((s) => s.table === "tasks")
      .map((s) => s.json || "")
      .join("\n");
    return pks.every((pk) => tasks.includes(pk));
  };

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/`, { waitUntil: "load" });
    await page.waitForFunction(() => typeof window.nostosConnect === "function", null, {
      timeout: 10000,
    });
    await page.evaluate((u) => window.nostosConnect(u, "tasks"), wsUrl);
    const mode = await page
      .waitForFunction(() => window.nostosStorage !== null, null, { timeout: 15000 })
      .then(() => page.evaluate(() => window.nostosStorage));
    console.log("[flutter-web-smoke-reload] storage mode:", mode);

    await expect
      .poll(() => page.evaluate(() => window.nostosConnected), { timeout: 15000 })
      .toBe(true);
    await page.evaluate(() => window.nostosWatch("tasks"));

    // Atomic batch of 3 distinct rows → SqliteWasmStorage.enqueue_batch commits
    // them in one OPFS transaction.
    const batchRes = await page.evaluate((p) => {
      return window.nostosWriteBatch(
        p.map((pk, i) => ({
          table: "tasks",
          op: "upsert",
          pk,
          payloadJson: JSON.stringify({ n: i + 1 }),
        })),
      );
    }, pks);
    expect(batchRes.ok).toBe(true);
    expect(batchRes.writeIds.length).toBe(3);

    // Wait for all 3 to appear in a tasks snapshot (local apply + server echo),
    // then settle so the server has acked before we tear the socket down.
    await expect.poll(allPresent, { timeout: 20000 }).toBe(true);
    await page.waitForTimeout(800);

    // ===== RELOAD: Worker is destroyed + re-spawned; OPFS persists =====
    await page.reload({ waitUntil: "load" });
    await page.waitForFunction(() => typeof window.nostosConnect === "function", null, {
      timeout: 10000,
    });
    await page.evaluate((u) => window.nostosConnect(u, "tasks"), wsUrl);
    await page.waitForFunction(() => window.nostosStorage !== null, { timeout: 15000 });
    await expect
      .poll(() => page.evaluate(() => window.nostosConnected), { timeout: 15000 })
      .toBe(true);
    await page.evaluate(() => window.nostosWatch("tasks"));

    if (mode === "durable") {
      // DURABLE PROOF: the atomic-batch rows survived the reload in OPFS.
      await expect
        .poll(allPresent, { timeout: 20000, message: "writeBatch rows survive reload (durable)" })
        .toBe(true);
      console.log("[flutter-web-smoke-reload] DURABLE_OK");
    } else {
      // Memory mode: rows are lost on reload — the documented degrade ceiling,
      // not a failure. The test still proves the reload + reconnect path works.
      console.log("[flutter-web-smoke-reload] memory mode — persistence is the documented ceiling");
    }

    const pageErrors = logs.filter((l) => l.startsWith("[pageerror]") || /worker.onerror/.test(l));
    expect(pageErrors, "page errors: " + pageErrors.join(" | ")).toEqual([]);
  } finally {
    await staticServer.server.close();
    spine.child.kill("SIGTERM");
  }
});
