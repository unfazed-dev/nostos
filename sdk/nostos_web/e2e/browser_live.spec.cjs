// Playwright headless-browser E2E for @nostos-sync/web against the SDK live-E2E spine.
//
// WS1: the live path (NostosSocket + apply engine + InMemoryStorage) runs INSIDE
// a Web Worker; the page (app.html) is a pure postMessage proxy that imports NO
// wasm. This spec drives the proxy and proves the two-direction round-trip plus
// the new ASYNC write contract:
//
//   1. PUSH: `connect` over postMessage opens the WS (in-Worker) + subscribes;
//      Node-side `POST /push` injects a `tasks` row server-side; the spine fans
//      it out → the in-Worker onmessage pump applies it → `rowsFor("tasks")`
//      (round-tripped through the Worker) returns the row → `[web-e2e] PUSH_OK`.
//   2. WRITE (async): `write` posts to the Worker; NostosSocket.write captures it
//      via the Outbox (enqueue + apply_local) and ships it when OPEN — it does
//      NOT throw when closed. The Worker emits a `writeResult{client_write_id,
//      ok:true}` push → `[web-e2e] WRITE_OK`. (Previously the spec asserted the
//      synchronous throw; that contract is gone — reviewer note #1.)
//   3. ECHO: the spine's WriteBack re-emits the write through the fan-out → the
//      in-Worker pump applies it → `rowsFor` returns it → `[web-e2e] ECHO_OK`.
//
// Setup: spawn the spine binary (nostos-infra `e2e_server` example, which prints
// `NOSTOS_E2E_PORT=` + `NOSTOS_E2E_READY`), spawn a static HTTP server that serves
// `sdk/nostos_web/*` with `/pkg-web/*` mapped to the wasm artifact, then launch
// headless chromium on `app.html`. Both are torn down in `finally`.

"use strict";

const { test, expect } = require("@playwright/test");
const { spawn } = require("node:child_process");
const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const SPINE_EXE = path.join(
  REPO_ROOT,
  "target",
  "debug",
  "examples",
  "e2e_server",
);
const PKG_WEB = path.join(REPO_ROOT, "crates", "nostos-ffi-wasm", "pkg-web");
const WEB_SDK = path.join(REPO_ROOT, "sdk", "nostos_web");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
};

// --- static HTTP server: serves sdk/nostos_web/* + /pkg-web/* on 127.0.0.1:0 ---

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
        // Defense-in-depth against path traversal.
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
          const ext = path.extname(filePath);
          res.writeHead(200, {
            "Content-Type": MIME[ext] || "application/octet-stream",
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
      const port = server.address().port;
      resolve({ server, port });
    });
  });
}

// --- spine binary: read NOSTOS_E2E_PORT + NOSTOS_E2E_READY from stdout ---

function startSpine() {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(SPINE_EXE)) {
      reject(
        new Error(
          "spine binary not found at " +
            SPINE_EXE +
            " — run `cargo build -p nostos-infra --example e2e_server` first.",
        ),
      );
      return;
    }
    const child = spawn(SPINE_EXE, [], {
      stdio: ["ignore", "pipe", "inherit"],
    });
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
    child.on("exit", (code, signal) => {
      reject(
        new Error(
          "spine exited before READY (code=" +
            code +
            " signal=" +
            signal +
            ")",
        ),
      );
    });
    setTimeout(
      () => reject(new Error("spine never signaled READY (30s)")),
      30000,
    ).unref();
  });
}

// --- POST /push from Node-side (the spine accepts /push from any client) ---

async function httpPush(port, bodyJson) {
  const resp = await fetch(`http://127.0.0.1:${port}/push`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: bodyJson,
  });
  if (!resp.ok) {
    throw new Error(`POST /push failed: HTTP ${resp.status}`);
  }
  return resp.text();
}

test("web live-replication round-trip via Worker (PUSH + async write + ECHO)", async ({
  page,
}) => {
  test.setTimeout(60000);

  // Capture browser console — the source of the [web-e2e] markers.
  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  page.on("pageerror", (err) =>
    logs.push("[pageerror] " + (err && err.message ? err.message : String(err))),
  );

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log("[web-e2e] spine on port", spine.port, "; static on", staticServer.port);

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/app.html`, {
      waitUntil: "domcontentloaded",
    });

    // The proxy is up, then the Worker boots the wasm.
    await expect
      .poll(() => logs.some((l) => l === "[web-e2e] PROXY_READY"), {
        timeout: 10000,
        message: "PROXY_READY from app.html",
      })
      .toBe(true);
    await expect
      .poll(() => logs.some((l) => l === "[web-e2e] WASM_READY"), {
        timeout: 20000,
        message: "WASM_READY from the Worker",
      })
      .toBe(true);

    // Connect + subscribe through the Worker (the WS + engine live there now).
    await page.evaluate(async (url) => {
      await window.nostos.connect(url, null, "tasks", null);
    }, wsUrl);

    // Connect resolves on ready_state==OPEN; the subscribe frame is sent in the
    // Worker's onopen, which fires shortly after. The spine ignores /push for
    // tables with no live subscriber, so drain before pushing.
    await page.waitForTimeout(500);

    // ---------------- PUSH direction ----------------
    await httpPush(
      spine.port,
      JSON.stringify({
        pk: "web-push",
        payload: { title: "from-server", status: "open", priority: "5" },
      }),
    );

    await expect
      .poll(
        async () => {
          const r = await page.evaluate(() => window.nostos.rowsFor("tasks"));
          return (r && r.rows ? r.rows : []).map((x) => x.pk);
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "pushed row web-push appears via Worker rowsFor('tasks')",
        },
      )
      .toContain("web-push");

    await page.evaluate(() => console.log("[web-e2e] PUSH_OK"));

    // ---------------- WRITE direction (async contract) ----------------
    // write() is fire-and-forget over postMessage; the outcome is the
    // writeResult push. It does NOT throw when closed (the old assertion).
    await page.evaluate(() =>
      window.nostos.write(
        "tasks",
        "upsert",
        "web-echo",
        JSON.stringify({
          title: "from-client",
          status: "open",
          priority: "5",
        }),
        "w1",
      ),
    );

    await expect
      .poll(
        async () => {
          return await page.evaluate(() =>
            (window.nostos.events || []).some(
              (e) =>
                e.type === "writeResult" &&
                e.client_write_id === "w1" &&
                e.ok === true,
            ),
          );
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "writeResult{client_write_id:'w1', ok:true} push arrived",
        },
      )
      .toBe(true);

    await page.evaluate(() => console.log("[web-e2e] WRITE_OK"));

    // ---------------- ECHO direction ----------------
    // The spine's WriteBack re-emits the write; the in-Worker pump applies it.
    // (apply_local also rendered it instantly, so this may already be present.)
    await expect
      .poll(
        async () => {
          const r = await page.evaluate(() => window.nostos.rowsFor("tasks"));
          return (r && r.rows ? r.rows : []).map((x) => x.pk);
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "echo row web-echo appears via Worker rowsFor('tasks')",
        },
      )
      .toContain("web-echo");

    await page.evaluate(() => console.log("[web-e2e] ECHO_OK"));

    expect(
      logs.some((l) => l === "[web-e2e] PUSH_OK"),
      "PUSH_OK in page console",
    ).toBe(true);
    expect(
      logs.some((l) => l === "[web-e2e] WRITE_OK"),
      "WRITE_OK in page console",
    ).toBe(true);
    expect(
      logs.some((l) => l === "[web-e2e] ECHO_OK"),
      "ECHO_OK in page console",
    ).toBe(true);

    // Cleanly close so the spine session ends gracefully.
    await page.evaluate(() => window.nostos.close());
  } finally {
    if (test.info().status !== "passed") {
      console.log("[web-e2e] captured page console:");
      for (const line of logs) {
        console.log("    | " + line);
      }
    }
    staticServer.server.close();
    try {
      spine.child.kill("SIGTERM");
    } catch (_) {
      /* already dead */
    }
  }
});
