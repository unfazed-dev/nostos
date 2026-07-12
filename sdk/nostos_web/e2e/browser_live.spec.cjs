// Playwright headless-browser E2E for @nostos-sync/web against the SDK live-E2E spine.
//
// Proves the SAME two-direction round-trip the Rust reference template
// (`crates/nostos-client/tests/e2e_live_replication.rs`) proves, driven through
// the browser's real `WebSocket` via the wasm `NostosSocket` (the E1 transport
// in `nostos-ffi-wasm`):
//
//   1. PUSH: `NostosSocket.connect()` opens the WS + subscribes; Node-side
//      `POST /push` injects a `tasks` row server-side; the spine fans it out
//      over the real sync handler → the WASM onmessage pump decodes + applies
//      it → `sock.rowsFor("tasks")` returns the row → `[web-e2e] PUSH_OK`.
//   2. ECHO: `sock.write("tasks","upsert","web-echo",...)` sends a write frame;
//      the spine's echo `WriteBack` re-emits it through the fan-out → the
//      writer receives its own write back on the same socket → applies →
//      `rowsFor` returns it → `[web-e2e] ECHO_OK`.
//
// Setup: spawn the spine binary, discover its port via `NOSTOS_E2E_PORT`, spawn
// a tiny static HTTP server that serves `sdk/nostos_web/` with `/pkg-web/*`
// mapped to `crates/nostos-ffi-wasm/pkg-web/`, then launch headless chromium on
// `app.html`. The HTTP server + spine are torn down in `finally`.

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
  ".js": "application/javascript; charset=utf-8",
  ".mjs": "application/javascript; charset=utf-8",
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

test("web live-replication round-trip against spine (PUSH + ECHO)", async ({
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
    await page.goto(
      `http://127.0.0.1:${staticServer.port}/e2e/app.html`,
      { waitUntil: "domcontentloaded" },
    );

    // Wait for the WASM to init (the page logs WASM_READY).
    await expect
      .poll(() => logs.some((l) => l === "[web-e2e] WASM_READY"), {
        timeout: 20000,
        message: "WASM_READY from app.html",
      })
      .toBeTruthy();

    // Connect + subscribe. Store the socket on window so later evaluate calls
    // can reach it.
    await page.evaluate(async (url) => {
      window.__sock = await window.NostosSocket.connect(url, null, "tasks", null);
      console.log("[web-e2e] CONNECTED rowCount=" + window.__sock.rowCount);
    }, wsUrl);

    // Connect resolves on ready_state==OPEN; the subscribe frame is sent in
    // the `onopen` callback which fires shortly after on the browser's event
    // loop. The spine ignores /push for tables that have no live subscriber,
    // so wait briefly for the subscribe to land server-side before pushing.
    // (Mirrors the e2e_server_selftest's 200ms drain after subscribe.)
    await page.waitForTimeout(500);

    // ---------------- PUSH direction ----------------
    // Node-side POST /push → server fans out over WS → WASM applies it.
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
          return await page.evaluate(() => {
            const rows = window.__sock.rowsFor("tasks") || [];
            return rows.map((r) => r.pk);
          });
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "pushed row web-push appears in rowsFor('tasks')",
        },
      )
      .toContain("web-push");

    console.log("[web-e2e] PUSH_OK");
    // Mirror to page console for capture uniformity.
    await page.evaluate(() => console.log("[web-e2e] PUSH_OK"));

    // ---------------- ECHO direction ----------------
    // Browser-side write() → spine's echo WriteBack re-emits → applies.
    await page.evaluate(() => {
      window.__sock.write(
        "tasks",
        "upsert",
        "web-echo",
        JSON.stringify({
          title: "from-client",
          status: "open",
          priority: "5",
        }),
        "w1",
      );
    });

    await expect
      .poll(
        async () => {
          return await page.evaluate(() => {
            const rows = window.__sock.rowsFor("tasks") || [];
            return rows.map((r) => r.pk);
          });
        },
        {
          timeout: 15000,
          intervals: [100, 250, 500],
          message: "echo row web-echo appears in rowsFor('tasks')",
        },
      )
      .toContain("web-echo");

    console.log("[web-e2e] ECHO_OK");
    await page.evaluate(() => console.log("[web-e2e] ECHO_OK"));

    // Final assertion: both markers landed on the captured page console.
    expect(
      logs.some((l) => l === "[web-e2e] PUSH_OK"),
      "PUSH_OK in page console",
    ).toBe(true);
    expect(
      logs.some((l) => l === "[web-e2e] ECHO_OK"),
      "ECHO_OK in page console",
    ).toBe(true);

    // Cleanly close the socket so the spine session ends gracefully.
    await page.evaluate(() => {
      try {
        window.__sock.close();
      } catch (_) {
        /* already closed */
      }
    });
  } finally {
    // On failure, surface every captured console + pageerror line so the
    // runner isn't a black box (the spine's stderr is already inherited).
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
