// Playwright headless-browser E2E for @nostos-sync/capacitor against the SDK
// live-E2E spine.
//
// Proves the SAME two-direction round-trip as @nostos-sync/web's browser_live.spec,
// driven through the Capacitor plugin shape (`registerPlugin('Nostos', { web })`
// → the NostosWeb class in src/web.ts → the wasm NostosSocket):
//
//   1. PUSH: `Nostos.connect()` opens the WS via the wasm NostosSocket;
//      Node-side `POST /push` injects a `tasks` row server-side; the spine
//      fans it out over the real sync handler → the WASM onmessage pump
//      decodes + applies it → `Nostos.query({ table: "tasks" })` returns the
//      row → `[cap-e2e] PUSH_OK`.
//   2. ECHO: `Nostos.write({ table: "tasks", op: "upsert", pk: "cap-echo",
//      payload: {...}, clientWriteId: "w1" })` sends a write frame via the
//      wasm NostosSocket; the spine's echo WriteBack re-emits it through the
//      fan-out → the writer receives its own write back → applies →
//      `Nostos.query()` returns it → `[cap-e2e] ECHO_OK`.
//
// Setup: spawn the spine binary, discover its port via `NOSTOS_E2E_PORT`,
// spawn a static HTTP server that serves the example app, the compiled
// plugin's dist/, the npm-installed @capacitor/core, and the pkg-web wasm.
// All torn down in `finally`.

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
const PLUGIN_DIR = path.resolve(__dirname, "..", ".."); // sdk/nostos_capacitor
const PKG_WEB = path.join(REPO_ROOT, "sdk", "nostos_web", "pkg-web");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".mjs": "application/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
};

// --- static HTTP server ----------------------------------------------
//
// URL → filesystem mapping (document root: nostos_capacitor/):
//   /                              → example-app/index.html
//   /main.js                       → example-app/main.js
//   /dist/*                        → dist/*           (compiled plugin)
//   /node_modules/*                → node_modules/*   (@capacitor/core ESM)
//   /pkg-web/*                     → nostos_web/pkg-web/* (wasm glue + bg.wasm)

function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
        let filePath;
        if (urlPath === "/") {
          filePath = path.join(PLUGIN_DIR, "example-app", "index.html");
        } else if (urlPath === "/main.js") {
          filePath = path.join(PLUGIN_DIR, "example-app", "main.js");
        } else if (urlPath.startsWith("/dist/")) {
          // Compiled TS emits extensionless specifiers (`import("./web")`);
          // the browser does NOT auto-append .js, so resolve it server-side.
          const candidate = path.join(PLUGIN_DIR, urlPath);
          filePath =
            fs.existsSync(candidate) || path.extname(candidate)
              ? candidate
              : candidate + ".js";
        } else if (urlPath.startsWith("/node_modules/")) {
          const candidate = path.join(PLUGIN_DIR, urlPath);
          filePath =
            fs.existsSync(candidate) || path.extname(candidate)
              ? candidate
              : candidate + ".js";
        } else if (urlPath.startsWith("/pkg-web/")) {
          filePath = path.join(PKG_WEB, urlPath.slice("/pkg-web/".length));
        } else {
          res.writeHead(404);
          res.end("not mapped: " + urlPath);
          return;
        }

        // Defense-in-depth against path traversal.
        const rooted =
          filePath.startsWith(PLUGIN_DIR) || filePath.startsWith(PKG_WEB);
        if (!rooted) {
          res.writeHead(403);
          res.end("forbidden");
          return;
        }
        if (!fs.existsSync(filePath)) {
          res.writeHead(404);
          res.end("not found: " + urlPath);
          return;
        }
        fs.readFile(filePath, (err, data) => {
          if (err) {
            res.writeHead(500);
            res.end("read error: " + err.message);
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
        res.end("server error: " + e.message);
      }
    });
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      resolve({ server, port });
    });
  });
}

// --- spine lifecycle -------------------------------------------------

function startSpine() {
  return new Promise((resolve, reject) => {
    if (!fs.existsSync(SPINE_EXE)) {
      reject(
        new Error(
          "spine binary missing at " +
            SPINE_EXE +
            " — run: cargo build -p nostos-infra --examples",
        ),
      );
      return;
    }
    const child = spawn(SPINE_EXE, [], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let port = null;
    let resolved = false;
    const stdoutBuf = [];
    const onLine = (line) => {
      stdoutBuf.push(line);
      const m = /^NOSTOS_E2E_PORT=(\d+)$/.exec(line);
      if (m) {
        port = parseInt(m[1], 10);
      }
      if (line.includes("NOSTOS_E2E_READY")) {
        if (port === null) {
          if (!resolved) {
            resolved = true;
            reject(new Error("spine signaled READY without NOSTOS_E2E_PORT"));
            child.kill("SIGTERM");
          }
          return;
        }
        if (!resolved) {
          resolved = true;
          resolve({ child, port });
        }
      }
    };
    let buf = "";
    child.stdout.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line) onLine(line);
      }
    });
    child.on("exit", (code, signal) => {
      if (!resolved) {
        resolved = true;
        reject(
          new Error(
            "spine exited before READY (code=" +
              code +
              " signal=" +
              signal +
              ")",
          ),
        );
      }
    });
    setTimeout(
      () => reject(new Error("spine never signaled READY (30s)")),
      30000,
    ).unref();
  });
}

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

test("capacitor web-only plugin live round-trip against spine (PUSH + ECHO)", async ({
  page,
}) => {
  test.setTimeout(60000);

  const logs = [];
  page.on("console", (msg) => logs.push(msg.text()));
  page.on("pageerror", (err) =>
    logs.push(
      "[pageerror] " + (err && err.message ? err.message : String(err)),
    ),
  );

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  console.log(
    "[cap-e2e] spine on port",
    spine.port,
    "; static on",
    staticServer.port,
  );

  try {
    await page.goto(
      `http://127.0.0.1:${staticServer.port}/?syncUrl=${encodeURIComponent(wsUrl)}`,
      { waitUntil: "domcontentloaded" },
    );

    // Wait for the plugin to load the wasm + connect + log WASM_READY.
    await expect
      .poll(() => logs.some((l) => l === "[cap-e2e] WASM_READY"), {
        timeout: 20000,
        message: "WASM_READY from main.js",
      })
      .toBeTruthy();

    await expect
      .poll(() => logs.some((l) => l.startsWith("[cap-e2e] CONNECTED")), {
        timeout: 15000,
        message: "CONNECTED from main.js",
      })
      .toBeTruthy();

    // Connect resolves on WS OPEN; the subscribe frame is sent in onopen
    // which fires shortly after on the browser's event loop. The spine
    // ignores /push for tables that have no live subscriber, so wait briefly
    // for the subscribe to land server-side. (Mirrors the
    // e2e_server_selftest's 200ms drain after subscribe.)
    await page.waitForTimeout(500);

    // ---------------- PUSH direction ----------------
    await httpPush(
      spine.port,
      JSON.stringify({
        pk: "cap-push",
        payload: { title: "from-server", status: "open", priority: "5" },
      }),
    );

    await expect
      .poll(
        async () => {
          const { rows } = await page.evaluate(() =>
            window.Nostos.query({ table: "tasks" }),
          );
          return (rows || []).map((r) => r.pk);
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "pushed row cap-push appears in query({table:'tasks'})",
        },
      )
      .toContain("cap-push");

    console.log("[cap-e2e] PUSH_OK");
    await page.evaluate(() => console.log("[cap-e2e] PUSH_OK"));

    // ---------------- ECHO direction ----------------
    await page.evaluate(() =>
      window.Nostos.write({
        table: "tasks",
        op: "upsert",
        pk: "cap-echo",
        payload: { title: "from-client", status: "open", priority: "5" },
        clientWriteId: "w1",
      }),
    );

    await expect
      .poll(
        async () => {
          const { rows } = await page.evaluate(() =>
            window.Nostos.query({ table: "tasks" }),
          );
          return (rows || []).map((r) => r.pk);
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "echo row cap-echo appears in query({table:'tasks'})",
        },
      )
      .toContain("cap-echo");

    console.log("[cap-e2e] ECHO_OK");
    await page.evaluate(() => console.log("[cap-e2e] ECHO_OK"));

    // Final assertion: both markers landed on the captured page console.
    expect(
      logs.some((l) => l === "[cap-e2e] PUSH_OK"),
      "PUSH_OK in page console",
    ).toBe(true);
    expect(
      logs.some((l) => l === "[cap-e2e] ECHO_OK"),
      "ECHO_OK in page console",
    ).toBe(true);

    // Cleanly close the socket so the spine session ends gracefully.
    await page.evaluate(() => {
      try {
        window.Nostos.close();
      } catch (_) {
        /* already closed */
      }
    });
  } finally {
    if (test.info().status !== "passed") {
      console.log("[cap-e2e] captured page console:");
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
