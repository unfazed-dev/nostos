"use strict";

// Browser assertions for the built Atlet Flutter UI. The persistent launcher
// is the Rust appwrite_flutter_web_smoke binary; it supplies credentials only
// through process environment and removes them when this process exits.
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const { chromium } = require("@playwright/test");

const root = path.resolve(process.env.ATLET_WEB_ROOT ?? "build/web");
const email = process.env.ATLET_WEB_EMAIL;
const password = process.env.ATLET_WEB_PASSWORD;
const role = process.env.ATLET_WEB_ROLE ?? "customer_a";
const evidencePath = process.env.ATLET_WEB_EVIDENCE;
if (!email || !password || !evidencePath) {
  throw new Error("Rust launcher must supply browser credentials and evidence path");
}

const mime = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ttf": "font/ttf",
  ".otf": "font/otf",
};

function serve() {
  const server = http.createServer((request, response) => {
    let name;
    try {
      name = decodeURIComponent(new URL(request.url, "http://127.0.0.1").pathname);
    } catch (_) {
      response.writeHead(400).end();
      return;
    }
    const file = path.resolve(root, `.${name === "/" ? "/index.html" : name}`);
    const relative = path.relative(root, file);
    if (relative.startsWith("..") || path.isAbsolute(relative)) {
      response.writeHead(403).end();
      return;
    }
    fs.readFile(file, (error, body) => {
      if (error) {
        response.writeHead(404).end();
        return;
      }
      response.writeHead(200, { "content-type": mime[path.extname(file)] ?? "application/octet-stream" });
      response.end(body);
    });
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

async function signIn(page, baseUrl, accountEmail, accountPassword) {
  await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
  await page.locator("flt-semantics-placeholder").waitFor({ timeout: 30000 });
  await page.evaluate(() => document.querySelector("flt-semantics-placeholder")?.click());
  await enterFlutterText(page.getByRole("textbox", { name: "Email" }), accountEmail);
  await enterFlutterText(page.getByRole("textbox", { name: "Password" }), accountPassword);
  await page.waitForFunction(() => {
    const button = [...document.querySelectorAll('[role="button"]')]
      .find((node) => node.textContent?.trim() === "Sign in");
    return button && button.getAttribute("aria-disabled") !== "true";
  }, undefined, { timeout: 10000 });
  await page.getByRole("button", { name: "Sign in" }).click({ timeout: 10000 });
  await page.getByRole("button", { name: "Add session" }).waitFor({ timeout: 30000 });
  // This label is driven by the Worker's reported OPFS mode, rather than a
  // successful WASM download that could still have fallen back to memory.
  await page.getByText(/^Offline storage ready/).waitFor({ timeout: 30000 });
}

async function enterFlutterText(field, value) {
  // Flutter owns the editing state behind its semantics input. `fill()` can
  // change that DOM node without committing the value to Flutter; real key
  // events are required. A slow CI bridge sometimes drops keys, so retry the
  // whole entry only when the observed input is incomplete.
  let actual = "";
  for (let attempt = 0; attempt < 5; attempt++) {
    await field.click();
    await field.press("ControlOrMeta+A");
    await field.press("Backspace");
    await field.pressSequentially(value, { delay: 60 });
    actual = await field.inputValue();
    if (actual === value) return;
  }
  throw new Error(`Flutter text input mismatch (expected ${value.length} chars, got ${actual.length})`);
}

async function main() {
  const started = Date.now();
  const server = await serve();
  const executablePath = process.env.ATLET_CHROME_EXECUTABLE ||
    (process.platform === "darwin"
      ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      : undefined);
  const browser = await chromium.launch({ headless: true, executablePath });
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const appwriteCalls = [];
  const pushTrace = [];
  const browserErrors = [];
  const workers = [];
  let sqliteWasmLoaded = false;
  page.on("worker", (worker) => workers.push(new URL(worker.url()).pathname));
  page.on("pageerror", (error) => browserErrors.push(String(error.message).slice(0, 250)));
  page.on("console", (message) => {
    if (message.type() === "error") browserErrors.push(message.text().slice(0, 250));
  });
  page.on("response", (response) => {
    if (response.url().includes("/functions/atlet_sync/executions")) {
      appwriteCalls.push(response.status());
      let body;
      try { body = JSON.parse(response.request().postDataJSON()?.body ?? "null"); }
      catch (_) { body = null; }
      if (body?.table) {
        pushTrace.push({
          table: body.table,
          op: body.op,
          pk: body.pk,
          mutation_id: body.mutation_id,
          title: body.payload?.title ?? null,
        });
      }
    }
    if (response.url().endsWith("/nostos/sqlite-wasm/sqlite3.wasm") && response.ok()) {
      sqliteWasmLoaded = true;
    }
  });
  const evidence = { role, provider: "appwrite", browser: "chrome", success: false };
  try {
    const baseUrl = `http://127.0.0.1:${server.address().port}/`;
    await signIn(page, baseUrl, email, password);
    const homeText = await page.locator("body").innerText();
    evidence.home_marker = homeText.includes("Welcome back.") ? "sign_in" : "other";
    evidence.storage = "durable";
    evidence.sqlite_wasm_asset_loaded = sqliteWasmLoaded;
    evidence.workers = workers;
    evidence.browser_errors = browserErrors.slice(0, 10);
    await page.screenshot({ path: path.join(path.dirname(evidencePath), "appwrite-web-home.png") });
    if (role === "admin") {
      await page.mouse.click(1120, 750);
      await page.getByText("Atlet admin").waitFor({ timeout: 15000 });
      evidence.admin_title_visible = true;
      await page.screenshot({ path: path.join(path.dirname(evidencePath), "appwrite-web-admin.png") });
      const productName = `Atlet web product ${Date.now()}`;
      await page.getByRole("button", { name: "Add product" }).click();
      const nameField = page.getByRole("textbox", { name: "Name" });
      await enterFlutterText(nameField, productName);
      const submittedProductName = await nameField.inputValue();
      const priceField = page.getByRole("textbox", { name: "Price in dollars" });
      await enterFlutterText(priceField, "12.50");
      const submittedPrice = await priceField.inputValue();
      if (!submittedProductName.includes("web product") ||
          !(Number(submittedPrice) > 0)) {
        throw new Error("Flutter admin product inputs did not receive keyboard text");
      }
      const productPush = page.waitForResponse((response) => {
        if (!response.url().includes("/functions/atlet_sync/executions")) return false;
        try { return response.request().postDataJSON()?.path === "/sync/push"; }
        catch (_) { return false; }
      }, { timeout: 45000 }).catch(() => null);
      await page.getByRole("button", { name: "Save" }).click();
      await page.getByText(submittedProductName).waitFor({ timeout: 15000 });
      const productResponse = await productPush;
      if (!productResponse) throw new Error("Admin product push did not reach Appwrite");
      const productExecution = await productResponse.json();
      if (productExecution.responseStatusCode < 200 || productExecution.responseStatusCode >= 300) {
        throw new Error("Admin product write was rejected by Appwrite");
      }
      evidence.product_name = submittedProductName;
      evidence.product_price_input = submittedPrice;
      evidence.admin_product_cloud_push = true;
      await page.evaluate(() => {
        const bus = new BroadcastChannel("nostos:multitab");
        const messages = [];
        bus.onmessage = (event) => messages.push(event.data);
        window.__atletRawBusProbe = { bus, messages };
      });
      // The successful cloud push proves the leader has an active session.
      // A second Worker on this origin claims to be the admin but carries a
      // real customer B JWT. The leader must verify the bearer with Appwrite
      // before forwarding any cached product snapshot or connected status.
      const wrongAccountJwt = process.env.ATLET_WEB_WRONG_ACCOUNT_JWT;
      if (!wrongAccountJwt) throw new Error("Rust launcher omitted customer B JWT");
      await page.evaluate(({ endpoint, projectId, token }) => {
        const messages = [];
        const worker = new SharedWorker("/nostos/nostos_broker.js", { type: "module" }).port;
        worker.start();
        worker.onmessage = (event) => messages.push(event.data);
        window.__atletFollowerProbe = { worker, messages };
        worker.postMessage({ id: 77001, cmd: "connect", url: endpoint,
          provider: "appwrite", projectId, functionId: "atlet_sync",
          userId: "atlet_admin_demo", token,
          tables: [{ name: "products" }] });
      }, { endpoint: process.env.ATLET_WEB_ENDPOINT,
        projectId: process.env.ATLET_WEB_PROJECT_ID, token: wrongAccountJwt });
      await page.waitForFunction(() => window.__atletFollowerProbe?.messages
        .some((message) => message.id === 77001), undefined, { timeout: 30000 });
      const followerReply = await page.evaluate(() => window.__atletFollowerProbe.messages
        .find((message) => message.id === 77001));
      evidence.mismatched_tab_rejected =
        followerReply?.error === "another signed-in session owns browser storage";
      if (!evidence.mismatched_tab_rejected) {
        throw new Error("Different-account browser Worker joined the admin session");
      }
      await page.waitForTimeout(500);
      const leakedFollowerPushes = await page.evaluate(() => window.__atletFollowerProbe.messages
        .filter((message) => message.type === "snapshot" ||
          (message.type === "status" && message.connected === true)));
      evidence.mismatched_tab_snapshots_blocked = leakedFollowerPushes.length === 0;
      await page.evaluate(() => window.__atletFollowerProbe.worker.close());
      if (!evidence.mismatched_tab_snapshots_blocked) {
        throw new Error("Different-account Worker received private leader pushes");
      }
      const followerJwt = process.env.ATLET_WEB_FOLLOWER_JWT;
      const rotatedJwt = process.env.ATLET_WEB_ROTATED_JWT;
      if (!followerJwt || !rotatedJwt || followerJwt === rotatedJwt) {
        throw new Error("Rust launcher did not provide distinct same-user JWTs");
      }
      await page.evaluate(({ endpoint, projectId, token }) => {
        const messages = [];
        const worker = new SharedWorker("/nostos/nostos_broker.js", { type: "module" }).port;
        worker.start();
        worker.onmessage = (event) => messages.push(event.data);
        window.__atletSameUserTab = { worker, messages };
        worker.postMessage({ id: 77101, cmd: "connect", url: endpoint,
          provider: "appwrite", projectId, functionId: "atlet_sync",
          userId: "atlet_admin_demo", token, tables: [{ name: "products" }] });
      }, { endpoint: process.env.ATLET_WEB_ENDPOINT,
        projectId: process.env.ATLET_WEB_PROJECT_ID, token: followerJwt });
      await page.waitForFunction(() => window.__atletSameUserTab?.messages
        .some((message) => message.id === 77101), undefined, { timeout: 30000 });
      evidence.same_user_tab_joined = await page.evaluate(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77101 && message.ok === true));
      if (!evidence.same_user_tab_joined) {
        throw new Error("Different JWT for the same admin account could not join the leader");
      }
      await page.evaluate(() => window.__atletSameUserTab.worker.postMessage({
        cmd: "watch", table: "products",
      }));
      try {
        await page.waitForFunction((name) => window.__atletSameUserTab.messages
          .some((message) => message.type === "snapshot" && message.table === "products" &&
            message.json.includes(name)), submittedProductName, { timeout: 10000 });
      } catch (error) {
        evidence.same_user_tab_trace = await page.evaluate((name) =>
          window.__atletSameUserTab.messages.map((message) => ({
            id: message.id ?? null, type: message.type ?? null,
            table: message.table ?? null, ok: message.ok ?? null,
            error: message.error ?? null, connected: message.connected ?? null,
            snapshot_chars: message.type === "snapshot" ? message.json.length : null,
            contains_demo_product: message.type === "snapshot" &&
              message.json.includes("Atlet web product"),
            contains_product: message.type === "snapshot" &&
              message.json.includes(name),
          })), submittedProductName);
        throw error;
      }
      evidence.same_user_tab_snapshot_visible = true;
      await page.evaluate((token) => window.__atletSameUserTab.worker.postMessage({
        id: 77102, cmd: "setToken", token,
      }), rotatedJwt);
      await page.waitForFunction(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77102), undefined, { timeout: 15000 });
      evidence.same_user_tab_rotated = await page.evaluate(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77102 && message.ok === true));
      if (!evidence.same_user_tab_rotated) {
        throw new Error("Same-account follower failed JWT refresh");
      }
      await page.evaluate(() => window.__atletSameUserTab.worker.postMessage({
        cmd: "watch", table: "products",
      }));
      await page.waitForFunction((name) => window.__atletSameUserTab.messages
        .filter((message) => message.type === "snapshot" && message.table === "products" &&
          message.json.includes(name)).length >= 2,
      submittedProductName, { timeout: 15000 });
      await page.evaluate((token) => window.__atletSameUserTab.worker.postMessage({
        id: 77104, cmd: "setToken", token,
      }), wrongAccountJwt);
      await page.waitForFunction(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77104), undefined, { timeout: 15000 });
      evidence.wrong_account_refresh_rejected = await page.evaluate(() =>
        window.__atletSameUserTab.messages.some((message) =>
          message.id === 77104 && Boolean(message.error)));
      if (!evidence.wrong_account_refresh_rejected) {
        throw new Error("Customer JWT refreshed an admin follower");
      }
      await page.evaluate(() => window.__atletSameUserTab.worker.postMessage({
        id: 77105, cmd: "query", sql: "SELECT * FROM products",
      }));
      await page.waitForFunction(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77105), undefined, { timeout: 5000 });
      evidence.revoked_follower_command_rejected = await page.evaluate(() =>
        window.__atletSameUserTab.messages.some((message) =>
          message.id === 77105 && Boolean(message.error)));
      if (!evidence.revoked_follower_command_rejected) {
        throw new Error("Revoked follower read the leader's products");
      }
      await page.evaluate(() => {
        window.__atletSameUserTab.worker.postMessage({ id: 77103, cmd: "close" });
      });
      await page.waitForFunction(() => window.__atletSameUserTab.messages
        .some((message) => message.id === 77103), undefined, { timeout: 10000 });
      await page.evaluate(() => window.__atletSameUserTab.worker.close());
      evidence.raw_bus_private_messages_blocked = await page.evaluate(() => {
        const messages = window.__atletRawBusProbe.messages;
        window.__atletRawBusProbe.bus.close();
        return messages.length === 0;
      });
      if (!evidence.raw_bus_private_messages_blocked) {
        throw new Error("Private browser messages leaked over BroadcastChannel");
      }
      // Flutter's CanvasKit TabBar paints this label without a Playwright text
      // node. The fixed 1280x800 viewport keeps the tab center deterministic.
      await page.mouse.click(1065, 115);
      await page.waitForTimeout(500);
      const userSemantics = await page.locator("flt-semantics").evaluateAll((nodes) =>
        nodes.map((node) => `${node.getAttribute("aria-label") ?? ""} ${node.textContent ?? ""}`)
          .filter((label) => /atlet_user_[ab]_demo|Atlet Customer [AB]/.test(label)));
      evidence.admin_user_semantics = userSemantics;
      evidence.admin_users_visible = userSemantics.some((label) => label.includes("atlet_user_a_demo")) &&
        userSemantics.some((label) => label.includes("atlet_user_b_demo"));
      if (!evidence.admin_users_visible) throw new Error("Admin cannot see both customer profiles");
      let orderId;
      for (const customerRole of ["customer_a", "customer_b"]) {
        const accountEmail = process.env[`ATLET_WEB_${customerRole.toUpperCase()}_EMAIL`];
        const accountPassword = process.env[`ATLET_WEB_${customerRole.toUpperCase()}_PASSWORD`];
        if (!accountEmail || !accountPassword) throw new Error(`Missing ${customerRole} browser credentials`);
        const customerContext = await browser.newContext({ viewport: { width: 1280, height: 800 } });
        let customerPage;
        try {
          customerPage = await customerContext.newPage();
          await signIn(customerPage, baseUrl, accountEmail, accountPassword);
          await customerPage.mouse.click(640, 750);
          await customerPage.getByText(submittedProductName).waitFor({ timeout: 45000 });
          evidence[`${customerRole}_sees_admin_product`] = true;
          evidence[`${customerRole}_admin_absent`] =
            await customerPage.getByText("Admin", { exact: true }).count() === 0;
          if (!evidence[`${customerRole}_admin_absent`]) throw new Error(`${customerRole} has admin navigation`);
          await customerPage.screenshot({
            path: path.join(path.dirname(evidencePath), `appwrite-web-${customerRole}-shop.png`),
          });
          if (customerRole === "customer_a") {
            await customerPage.getByText(submittedProductName).click();
            await customerPage.getByRole("button", { name: "Add to cart" }).click();
            await customerPage.getByRole("button", { name: "Add to cart" })
              .waitFor({ state: "hidden", timeout: 15000 });
            // Flutter's web semantics node can remain "unstable" to Playwright
            // while the visible FAB is clickable. Exercise its actual hit box.
            const cart = customerPage.getByRole("button", { name: /^Cart/ });
            const cartBox = await cart.boundingBox();
            if (!cartBox) throw new Error("Cart button is not visible");
            await customerPage.mouse.click(cartBox.x + cartBox.width / 2,
              cartBox.y + cartBox.height / 2);
            await customerPage.getByRole("button", { name: "Checkout" }).click();
            const orderPush = customerPage.waitForResponse((response) => {
              if (!response.url().includes("/functions/atlet_sync/executions")) return false;
              try {
                const request = response.request().postDataJSON();
                const body = JSON.parse(request.body);
                return request.path === "/sync/push" && body.table === "orders" &&
                  body.op === "upsert" && body.payload?.status === "paid";
              } catch (_) { return false; }
            }, { timeout: 60000 }).catch(() => null);
            await customerPage.getByRole("button", { name: /^Pay / }).click();
            await customerPage.getByText("Order placed").waitFor({ timeout: 20000 });
            const orderResponse = await orderPush;
            if (!orderResponse) throw new Error("Customer A order did not reach Appwrite");
            const orderExecution = await orderResponse.json();
            if (orderExecution.responseStatusCode < 200 || orderExecution.responseStatusCode >= 300) {
              throw new Error(`Customer A order rejected: ${orderExecution.responseStatusCode}`);
            }
            orderId = JSON.parse(orderResponse.request().postDataJSON().body).pk;
            evidence.order_id = orderId;
            evidence.customer_a_order_paid = true;
            await customerPage.screenshot({
              path: path.join(path.dirname(evidencePath), "appwrite-web-order-confirmation.png"),
            });
            const done = customerPage.getByRole("button", { name: "Done" });
            await done.click();
            await done.waitFor({ state: "hidden", timeout: 15000 });
          } else {
            await customerPage.mouse.click(1065, 750);
            await customerPage.waitForTimeout(1000);
            const labels = await customerPage.locator("flt-semantics").evaluateAll((nodes) =>
              nodes.map((node) => `${node.getAttribute("aria-label") ?? ""} ${node.textContent ?? ""}`));
            evidence.customer_b_order_isolated =
              !labels.some((label) => label.includes(orderId.slice(0, 8)));
            if (!evidence.customer_b_order_isolated) throw new Error("Customer B can see customer A order");
            await customerPage.screenshot({
              path: path.join(path.dirname(evidencePath), "appwrite-web-customer-b-history.png"),
            });
          }
          await customerPage.mouse.click(213, 755);
          await customerPage.getByRole("button", { name: "Sign out" }).waitFor({ timeout: 15000 });
          await customerPage.getByRole("button", { name: "Sign out" }).click();
          await customerPage.getByRole("button", { name: "Sign in" }).waitFor({ timeout: 30000 });
        } catch (error) {
          if (customerPage) {
            await customerPage.screenshot({
              path: path.join(path.dirname(evidencePath), `appwrite-web-${customerRole}-failure.png`),
            }).catch(() => {});
          }
          throw error;
        } finally {
          await customerContext.close();
        }
      }
      await page.mouse.click(640, 115);
      // Flutter paints the order title but merges it into a parent semantics
      // node. The Ship control is exposed; the matched mutation below proves
      // that the control belonged to this exact customer order.
      await page.getByRole("button", { name: "Ship" }).waitFor({ timeout: 45000 });
      const shipPush = page.waitForResponse((response) => {
        if (!response.url().includes("/functions/atlet_sync/executions")) return false;
        try {
          const request = response.request().postDataJSON();
          const body = JSON.parse(request.body);
          return request.path === "/sync/push" && body.table === "orders" &&
            body.pk === orderId && body.payload?.status === "shipped";
        } catch (_) { return false; }
      }, { timeout: 60000 }).catch(() => null);
      await page.getByRole("button", { name: "Ship" }).click();
      const shipped = await shipPush;
      if (!shipped || (await shipped.json()).responseStatusCode !== 200) {
        throw new Error("Admin ship write was not committed");
      }
      evidence.admin_sees_customer_order = true;
      const deliverPush = page.waitForResponse((response) => {
        if (!response.url().includes("/functions/atlet_sync/executions")) return false;
        try {
          const request = response.request().postDataJSON();
          const body = JSON.parse(request.body);
          return request.path === "/sync/push" && body.table === "orders" &&
            body.pk === orderId && body.payload?.status === "delivered";
        } catch (_) { return false; }
      }, { timeout: 60000 }).catch(() => null);
      await page.getByRole("button", { name: "Deliver" }).click();
      const delivered = await deliverPush;
      if (!delivered || (await delivered.json()).responseStatusCode !== 200) {
        throw new Error("Admin delivery write was not committed");
      }
      await page.getByRole("button", { name: "Deliver" })
        .waitFor({ state: "hidden", timeout: 15000 });
      evidence.admin_order_delivered = true;
      await page.screenshot({ path: path.join(path.dirname(evidencePath), "appwrite-web-delivered.png") });
      await page.mouse.click(160, 750);
      await page.waitForTimeout(500);
    } else {
      evidence.admin_nav_absent = await page.getByText("Admin", { exact: true }).count() === 0;
    }
    await page.route("**/functions/atlet_sync/executions", (route) => route.abort());
    await page.getByRole("button", { name: "Add session" }).focus();
    await page.keyboard.press("Enter");
    await page.waitForTimeout(1000);
    await page.screenshot({ path: path.join(path.dirname(evidencePath), "appwrite-web-form.png") });
    const title = `Atlet web offline ${Date.now()}`;
    const titleField = page.getByRole("textbox", { name: "Title" });
    await enterFlutterText(titleField, title);
    await enterFlutterText(page.getByRole("textbox", { name: "Km" }), "3");
    const submittedTitle = await titleField.inputValue();
    evidence.session_title = submittedTitle;
    const save = page.getByRole("button", { name: "Save" });
    await page.waitForFunction(() => {
      const button = [...document.querySelectorAll('[role="button"]')]
        .find((node) => node.textContent?.trim() === "Save");
      return button && button.getAttribute("aria-disabled") !== "true";
    }, undefined, { timeout: 10000 });
    await save.click({ timeout: 10000 });
    await save.waitFor({ state: "hidden", timeout: 15000 });
    await page.getByText(submittedTitle).waitFor({ timeout: 15000 });
    evidence.offline_local_render = true;
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.locator("flt-semantics-placeholder").waitFor({ timeout: 30000 });
    await page.evaluate(() => document.querySelector("flt-semantics-placeholder")?.click());
    await page.getByText(submittedTitle).waitFor({ timeout: 30000 });
    evidence.reload_retained_offline_write = true;
    const push = page.waitForResponse((response) => {
      if (!response.url().includes("/functions/atlet_sync/executions")) return false;
      try {
        const request = response.request().postDataJSON();
        const body = JSON.parse(request.body);
        return request.path === "/sync/push" && body.table === "sessions" &&
          body.payload?.title === submittedTitle;
      }
      catch (_) { return false; }
    }, { timeout: 45000 });
    await page.unroute("**/functions/atlet_sync/executions");
    const pushed = await push;
    const execution = await pushed.json();
    evidence.session_push_response_status = execution.responseStatusCode;
    evidence.push_trace = pushTrace;
    if (!pushed.ok() || execution.responseStatusCode < 200 || execution.responseStatusCode >= 300) {
      let message;
      try { message = JSON.parse(execution.responseBody ?? "null")?.error; }
      catch (_) { message = null; }
      throw new Error(`Appwrite Function rejected offline browser write: ${execution.responseStatusCode} ${String(message ?? "unknown").slice(0, 120)}`);
    }
    evidence.cloud_push_acknowledged = true;
    evidence.function_calls_after_resume = appwriteCalls.length;
    evidence.appwrite_function_calls = appwriteCalls.length;
    evidence.browser_errors = browserErrors.slice(0, 10);
    const mutations = new Map();
    for (const write of pushTrace) {
      const prior = mutations.get(write.mutation_id);
      if (prior && (prior.table !== write.table || prior.pk !== write.pk ||
          prior.op !== write.op || prior.title !== write.title)) {
        throw new Error("A browser mutation ID was reused for different writes");
      }
      mutations.set(write.mutation_id, write);
    }
    evidence.distinct_mutation_ids = mutations.size;
    if (role === "customer_a") {
      // Sign out with a genuinely pending offline write. A later sign-in as
      // the same account must not replay this discarded outbox entry.
      await page.route("**/functions/atlet_sync/executions", (route) => route.abort());
      await page.getByRole("button", { name: "Add session" }).click();
      const discardedTitle = `Atlet web discarded ${Date.now()}`;
      await enterFlutterText(page.getByRole("textbox", { name: "Title" }), discardedTitle);
      await enterFlutterText(page.getByRole("textbox", { name: "Km" }), "2");
      await page.waitForFunction(() => {
        const button = [...document.querySelectorAll('[role="button"]')]
          .find((node) => node.textContent?.trim() === "Save");
        return button && button.getAttribute("aria-disabled") !== "true";
      }, undefined, { timeout: 10000 });
      await page.getByRole("button", { name: "Save" }).click();
      await page.getByText(discardedTitle).waitFor({ timeout: 15000 });
      evidence.discarded_title = discardedTitle;
      evidence.discarded_local_visible = true;
    }
    await page.getByRole("button", { name: "Sign out" }).click();
    await page.getByRole("button", { name: "Sign in" }).waitFor({ timeout: 30000 });
    evidence.sign_out_returned_to_login = true;
    if (role === "customer_a") {
      await page.unroute("**/functions/atlet_sync/executions");
      await signIn(page, baseUrl, email, password);
      await page.waitForTimeout(3000);
      evidence.same_account_wipe =
        await page.getByText(evidence.discarded_title).count() === 0;
      if (!evidence.same_account_wipe) {
        throw new Error("Signed-out pending write survived same-account login");
      }
      await page.getByRole("button", { name: "Sign out" }).click();
      await page.getByRole("button", { name: "Sign in" }).waitFor({ timeout: 30000 });
      // Reuse this origin and OPFS database with a different real account.
      // Customer A's previously synced private session must not reappear.
      await signIn(page, baseUrl, process.env.ATLET_WEB_CUSTOMER_B_EMAIL,
        process.env.ATLET_WEB_CUSTOMER_B_PASSWORD);
      await page.waitForTimeout(3000);
      evidence.cross_account_cache_isolated =
        await page.getByText(evidence.session_title).count() === 0;
      if (!evidence.cross_account_cache_isolated) {
        throw new Error("Customer A session leaked into customer B browser cache");
      }
      await page.getByRole("button", { name: "Sign out" }).click();
      await page.getByRole("button", { name: "Sign in" }).waitFor({ timeout: 30000 });
    }
    evidence.appwrite_function_calls = appwriteCalls.length;
    evidence.success = evidence.appwrite_function_calls > 0 &&
      evidence.home_marker !== "sign_in" &&
      evidence.offline_local_render &&
      evidence.reload_retained_offline_write &&
      evidence.cloud_push_acknowledged &&
      evidence.sign_out_returned_to_login &&
      (role !== "customer_a" || (evidence.discarded_local_visible &&
        evidence.same_account_wipe && evidence.cross_account_cache_isolated)) &&
      evidence.storage === "durable" && evidence.sqlite_wasm_asset_loaded &&
      (role !== "admin" || (evidence.admin_users_visible &&
        evidence.mismatched_tab_rejected && evidence.mismatched_tab_snapshots_blocked &&
        evidence.raw_bus_private_messages_blocked &&
        evidence.same_user_tab_joined && evidence.same_user_tab_snapshot_visible &&
        evidence.same_user_tab_rotated && evidence.wrong_account_refresh_rejected &&
        evidence.revoked_follower_command_rejected &&
        evidence.customer_a_sees_admin_product && evidence.customer_b_sees_admin_product &&
        evidence.customer_a_order_paid && evidence.customer_b_order_isolated &&
        evidence.admin_order_delivered));
    if (!evidence.success) throw new Error("Atlet browser sync acceptance incomplete");
    console.log(`ATLET_EVIDENCE:${JSON.stringify({ home: true, appwrite_function_calls: appwriteCalls.length, storage: evidence.storage })}`);
  } catch (error) {
    evidence.error = String(error.message).slice(0, 300);
    evidence.browser_errors = browserErrors.slice(0, 10);
    await page.screenshot({ path: path.join(path.dirname(evidencePath), "appwrite-web-failure.png") }).catch(() => {});
    throw error;
  } finally {
    evidence.duration_ms = Date.now() - started;
    fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2));
    // Explicitly close the context before the browser (Playwright's lifecycle
    // contract). Bound teardown: an OPFS Worker has occasionally kept Chrome
    // open after every assertion and cloud write had completed.
    let teardownStep = "page unload";
    const teardownTimer = setTimeout(() => {
      evidence.success = false;
      evidence.teardown_error = `Chrome ${teardownStep} exceeded 15 seconds`;
      fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2));
      process.exit(1);
    }, 15000);
    try {
      // A reload leaves a second SQLite-WASM Worker in this context. Give
      // Chrome an explicit document unload before Playwright closes it.
      await page.goto("about:blank", { waitUntil: "commit", timeout: 5000 });
      await page.close();
      teardownStep = "context.close";
      await context.close();
      teardownStep = "browser.close";
      // Chrome sometimes keeps its OPFS process alive after this context is
      // gone. Playwright's process-exit handler kills its launched process
      // group, so bound only this final browser shutdown and record it.
      let browserCloseTimer;
      const browserClosed = await Promise.race([
        browser.close().then(() => true),
        new Promise((resolve) => {
          browserCloseTimer = setTimeout(() => resolve(false), 5000);
        }),
      ]);
      clearTimeout(browserCloseTimer);
      if (!browserClosed) {
        evidence.browser_close_forced = true;
        fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2));
        process.exit(evidence.success ? 0 : 1);
      }
      teardownStep = "server.close";
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    } catch (error) {
      evidence.success = false;
      evidence.teardown_error = String(error.message).slice(0, 200);
      fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2));
      throw error;
    } finally {
      clearTimeout(teardownTimer);
    }
  }
}

main().catch((error) => {
  console.error(String(error.message).slice(0, 300));
  process.exitCode = 1;
});
