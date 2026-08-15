// Playwright E2E for the EXPERIMENTAL Web Push slice (plan tasks 6.1 + 6.2,
// ADR-0037 §6 Wave 3 — behind the enableWebPush flag, default OFF).
//
// Three proofs, honest about what a headless browser can exercise:
//
//  1. KV STORAGE SEAM (6.1) — main-thread NostosSocket (kv_seam.html):
//     default run persists the checkpoint to window.localStorage (existing
//     embedder behavior unchanged); a setKvStore-injected spy store receives
//     the SAME key while localStorage stays untouched (the swap works).
//  2. SW WAKE (6.2) — the real Service Worker (sw/nostos.sw.js) is registered
//     from the app.html boot path behind the flag; a SYNTHETIC push (the
//     SW's "nostos:simulate-push" message drives the same handler as a real
//     `push` event — headless Chromium cannot mint encrypted pushes through
//     a push service) wakes a CLOSED sync session: the Worker's `wake`
//     command reconnects, and a server row pushed afterwards is applied —
//     the full doorbell → reconnect → sync-caught-up chain.
//  3. REST CONTRACT (6.2) — registerSubscription / deregisterSubscription
//     pin the server's exact wire shape: POST /push-tokens with Bearer +
//     {"platform":"webpush","token":<subscription JSON>} → 204, and DELETE
//     /push-tokens/{percent-encoded JSON}. Non-204 rejects.
//
// The live push-service leg (real encrypted push → SW) is NOT exercisable
// headlessly and is marked assumed — see README "Experimental: Web Push".

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
  ".map": "application/json; charset=utf-8",
};

// --- static HTTP server (durable.spec.cjs pattern + /nostos.sw.js mapping) ---

function startStaticServer() {
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      try {
        const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
        let filePath;
        if (urlPath.startsWith("/pkg-web/")) {
          filePath = path.join(PKG_WEB, urlPath.slice("/pkg-web/".length));
        } else if (urlPath === "/nostos.sw.js") {
          // Root-path alias for the SW: default scope "/" without a
          // Service-Worker-Allowed header. Hosts either serve the SW at the
          // scope root (this) or send the header (README documents both).
          filePath = path.join(WEB_SDK, "sw", "nostos.sw.js");
        } else if (urlPath === "/") {
          filePath = path.join(WEB_SDK, "e2e", "app.html");
        } else {
          filePath = path.join(WEB_SDK, urlPath);
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

// --- spine binary (durable.spec.cjs pattern) ---

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
    child.on("exit", (code, signal) => {
      reject(new Error("spine exited before READY (code=" + code + " signal=" + signal + ")"));
    });
    setTimeout(() => reject(new Error("spine never signaled READY (30s)")), 30000).unref();
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

// A REAL P-256 public key (the shape VAPID applicationServerKey demands —
// Node crypto.subtle generates the keypair; only the public half crosses to
// the page). subscribe() may still fail headless (no push service), but the
// key itself is then provably valid.
async function realVapidPublicKey() {
  const { publicKey } = await crypto.subtle.generateKey(
    { name: "ECDH", namedCurve: "P-256" },
    true,
    ["deriveBits"],
  );
  const raw = new Uint8Array(await crypto.subtle.exportKey("raw", publicKey));
  return Buffer.from(raw)
    .toString("base64")
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

// ============================================================================
// 1. KV storage seam (plan 6.1)
// ============================================================================

test("kv seam: default persists to localStorage; injected store swaps in untouched", async ({
  page,
}) => {
  test.setTimeout(90000);

  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push("[pageerror] " + ((e && e.message) || String(e))));

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;

  try {
    // ---- phase A: DEFAULT (no setKvStore) — localStorage, unchanged ----
    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/kv_seam.html`, {
      waitUntil: "domcontentloaded",
    });
    await expect
      .poll(() => logs.some((l) => l === "[kv-e2e] MAIN_READY"), { timeout: 20000 })
      .toBe(true);

    await page.evaluate(async (url) => {
      window.__sock = await window.__nostos.NostosSocket.connect(url, null, "tasks", null, null);
    }, wsUrl);
    await page.waitForTimeout(500);
    await httpPush(
      spine.port,
      JSON.stringify({ pk: "kv-default", payload: { title: "a", status: "open", priority: "5" } }),
    );
    await expect
      .poll(
        () =>
          page.evaluate(
            () => Number(localStorage.getItem("cairn:checkpoint:tasks") || 0),
          ),
        { timeout: 15000, message: "default run persists checkpoint to localStorage" },
      )
      .toBeGreaterThan(0);

    await page.evaluate(() => window.__sock.close());

    // ---- phase B: INJECTED store — same key, localStorage untouched ----
    const before = await page.evaluate(() => localStorage.getItem("cairn:checkpoint:tasks"));
    expect(before, "phase A left a localStorage checkpoint").toBeTruthy();

    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/kv_seam.html`, {
      waitUntil: "domcontentloaded",
    });
    await expect
      .poll(() => logs.filter((l) => l === "[kv-e2e] MAIN_READY").length >= 2, {
        timeout: 20000,
      })
      .toBe(true);

    await page.evaluate(() => {
      const map = new Map();
      window.__kv = {
        calls: [],
        getItem: (k) => (map.has(k) ? map.get(k) : null),
        setItem: (k, v) => {
          map.set(k, v);
          window.__kv.calls.push([k, v]);
        },
      };
      window.__nostos.setKvStore(window.__kv);
    });

    await page.evaluate(async (url) => {
      window.__sock = await window.__nostos.NostosSocket.connect(url, null, "tasks", null, null);
    }, wsUrl);
    await page.waitForTimeout(500);
    await httpPush(
      spine.port,
      JSON.stringify({ pk: "kv-injected", payload: { title: "b", status: "open", priority: "5" } }),
    );
    await expect
      .poll(
        () =>
          page.evaluate(() => {
            const hit = (window.__kv.calls || []).find(([k]) => k === "cairn:checkpoint:tasks");
            return hit ? hit[1] : null;
          }),
        { timeout: 15000, message: "injected store received the checkpoint under the pinned key" },
      )
      .toBeTruthy();

    const after = await page.evaluate(() => localStorage.getItem("cairn:checkpoint:tasks"));
    expect(after, "injected run must NOT touch localStorage").toBe(before);

    await page.evaluate(() => window.__sock.close());
  } finally {
    if (test.info().status !== "passed") {
      for (const line of logs) console.log("    | " + line);
    }
    staticServer.server.close();
    try {
      spine.child.kill("SIGTERM");
    } catch (_) {
      /* already dead */
    }
  }
});

// ============================================================================
// 2. Service Worker wake (plan 6.2) — synthetic push through the REAL SW
// ============================================================================

test("service worker: synthetic push wakes a closed sync session (doorbell → reconnect → caught up)", async ({
  page,
}) => {
  test.setTimeout(120000);

  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  page.on("pageerror", (e) => logs.push("[pageerror] " + ((e && e.message) || String(e))));

  const spine = await startSpine();
  const staticServer = await startStaticServer();
  const wsUrl = `ws://127.0.0.1:${spine.port}/sync`;
  const httpBase = `http://127.0.0.1:${spine.port}`;

  // The spine has no /push-tokens route (that's nostos-server) — mock it so
  // the registration leg is deterministic whether or not headless Chromium
  // can actually reach a push service.
  await page.route("**/push-tokens", (route) => route.fulfill({ status: 204 }));

  try {
    await page.context().grantPermissions(["notifications"]);
    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/app.html`, {
      waitUntil: "domcontentloaded",
    });
    await expect
      .poll(() => logs.some((l) => l === "[web-e2e] WASM_READY"), { timeout: 20000 })
      .toBe(true);

    // Opt in behind the flag (default OFF): registers the SW + persists the
    // boot-path config under the flag key.
    const enableResult = await page.evaluate(
      async (cfg) => {
        const r = await window.nostos.enableWebPush(cfg);
        return { enabled: r.enabled, reason: r.reason || null };
      },
      {
        vapidPublicKey: await realVapidPublicKey(),
        httpBase,
        token: null,
        swUrl: "/nostos.sw.js",
      },
    );
    console.log("[webpush-e2e] enableWebPush:", JSON.stringify(enableResult));

    // Wait for the SW to activate, then reload so it CONTROLS the page (the
    // boot path re-arms the wake listener in the fresh document).
    await page.evaluate(async () => {
      await navigator.serviceWorker.ready;
    });
    await page.reload({ waitUntil: "domcontentloaded" });
    await expect
      .poll(() => logs.filter((l) => l === "[web-e2e] WASM_READY").length >= 2, {
        timeout: 20000,
      })
      .toBe(true);
    await expect
      .poll(() => logs.some((l) => l === "[web-e2e] WEBPUSH_READY"), { timeout: 20000 })
      .toBe(true);
    const controlled = await page.evaluate(() => navigator.serviceWorker.controller !== null);
    expect(controlled, "the SW controls the page after reload").toBe(true);

    // Live session, then close it (the "backgrounded" stand-in).
    await page.evaluate(async (url) => {
      await window.nostos.connect(url, null, "tasks", null);
    }, wsUrl);
    await page.waitForTimeout(500);
    const connectedBefore = await page.evaluate(() =>
      (window.nostos.events || []).filter((e) => e.type === "status" && e.connected).length,
    );
    await page.evaluate(() => window.nostos.close());
    await page.waitForTimeout(300);

    // Synthetic push → the SAME handler a real `push` event drives.
    await page.evaluate((payload) => {
      navigator.serviceWorker.controller.postMessage({
        type: "nostos:simulate-push",
        payload,
      });
    }, { table: "tasks", lsn: 999_999 });

    // The doorbell woke the Worker: a NEW connected:true status.
    await expect
      .poll(
        () =>
          page.evaluate(() =>
            (window.nostos.events || []).filter((e) => e.type === "status" && e.connected).length,
          ),
        { timeout: 20000, message: "wake reconnected the sync session" },
      )
      .toBeGreaterThan(connectedBefore);

    // And sync caught up: a row pushed after the wake is applied.
    await httpPush(
      spine.port,
      JSON.stringify({
        pk: "wake-probe",
        payload: { title: "after-wake", status: "open", priority: "5" },
      }),
    );
    await expect
      .poll(
        async () => {
          const r = await page.evaluate(() => window.nostos.rowsFor("tasks"));
          return (r && r.rows ? r.rows : []).map((x) => x.pk);
        },
        { timeout: 15000, message: "post-wake row applied (sync caught up)" },
      )
      .toContain("wake-probe");

    // Sign-out hygiene (experimental flag off again): the flag key is host
    // state — cleared by the host, not by disable() (documented in README).
    await page.evaluate(() => window.nostos.close());
  } finally {
    if (test.info().status !== "passed") {
      for (const line of logs) console.log("    | " + line);
    }
    staticServer.server.close();
    try {
      spine.child.kill("SIGTERM");
    } catch (_) {
      /* already dead */
    }
  }
});

// ============================================================================
// 3. REST contract (plan 6.2) — no spine, page-side fetch capture
// ============================================================================

test("push registration: pins POST/DELETE /push-tokens wire shape + rejects non-204", async ({
  page,
}) => {
  test.setTimeout(60000);

  const staticServer = await startStaticServer();
  const logs = [];
  page.on("pageerror", (e) => logs.push("[pageerror] " + ((e && e.message) || String(e))));

  try {
    await page.goto(`http://127.0.0.1:${staticServer.port}/e2e/kv_seam.html`, {
      waitUntil: "domcontentloaded",
    });
    const ready = [];
    page.on("console", (m) => ready.push(m.text()));
    await expect
      .poll(() => ready.some((l) => l === "[kv-e2e] MAIN_READY"), { timeout: 20000 })
      .toBe(true);

    const sub = {
      endpoint: "https://fcm.example.com/send/abc-123",
      keys: { p256dh: "B publicKey", auth: "authSecret" },
      expirationTime: null,
    };

    const captured = await page.evaluate(async (subJson) => {
      const sub = JSON.parse(subJson);
      const mod = await import("/push.js");
      const calls = [];
      const realFetch = window.fetch;
      window.fetch = async (input, init) => {
        calls.push({ url: String(input), method: init.method, headers: init.headers, body: init.body });
        return new Response(null, { status: 204 });
      };
      try {
        await mod.registerSubscription(sub, { httpBase: "http://rest.test", token: "jwt-1" });
        await mod.deregisterSubscription(sub, { httpBase: "http://rest.test", token: "jwt-1" });
        // key conversion: 65 bytes, 0x04 uncompressed-point marker first
        const bytes = Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : i));
        const b64 = btoa(String.fromCharCode(...bytes))
          .replace(/\+/g, "-")
          .replace(/\//g, "_")
          .replace(/=+$/, "");
        const key = mod.urlB64ToUint8Array(b64);
        // non-204 must reject
        let rejected = null;
        window.fetch = async () => new Response("nope", { status: 400 });
        try {
          await mod.registerSubscription(sub, { httpBase: "http://rest.test", token: "jwt-1" });
        } catch (e) {
          rejected = String(e);
        }
        return { calls, keyLen: key.length, keyFirst: key[0], rejected };
      } finally {
        window.fetch = realFetch;
      }
    }, JSON.stringify(sub));

    expect(captured.keyLen, "VAPID key converts to a 65-byte Uint8Array").toBe(65);
    expect(captured.keyFirst, "uncompressed-point marker 0x04").toBe(4);
    expect(captured.rejected, "non-204 rejects").toContain("400");

    expect(captured.calls.length).toBe(2);

    const post = captured.calls[0];
    expect(post.url).toBe("http://rest.test/push-tokens");
    expect(post.method).toBe("POST");
    expect(post.headers["Content-Type"]).toBe("application/json");
    expect(post.headers.Authorization).toBe("Bearer jwt-1");
    const body = JSON.parse(post.body);
    expect(body.platform).toBe("webpush");
    expect(body.token).toBe(JSON.stringify(sub), "the whole subscription JSON is the token");

    const del = captured.calls[1];
    expect(del.method).toBe("DELETE");
    expect(del.url.startsWith("http://rest.test/push-tokens/")).toBe(true);
    const encoded = del.url.slice("http://rest.test/push-tokens/".length);
    expect(decodeURIComponent(encoded)).toBe(
      JSON.stringify(sub),
      "the webpush token is percent-encoded (subscription JSON contains '/')",
    );
  } finally {
    staticServer.server.close();
  }
});
