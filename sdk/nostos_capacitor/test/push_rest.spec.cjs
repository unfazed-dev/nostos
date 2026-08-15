// ADR-0037 §3 / plan task 6.3 — push-token REST registration, JS side.
//
// Pure node (`node --test`, the sdk/nostos_web attachments.spec.cjs pattern):
// a mocked global fetch captures the exact wire (method, URL, headers, body)
// and a fake wasm socket ctor stands in for the engine so connect() runs
// without a browser. Pins the SAME pinned contract the Flutter
// (push_token_test.dart) and Node SDK tests pin, so wire drift fails here
// first. tenant/account are never sent — the server stamps them
// (ADR-0018 discipline).
//
//   POST /push-tokens           {"platform":"apns"|"fcm"|"webpush","token":"…"}
//                               auth: same JWT as /sync (Bearer) → 204
//   DELETE /push-tokens/{token} same auth → 204

const test = require("node:test");
const assert = require("node:assert");

const { NostosWeb } = require("../dist/web.js");

/** Fake wasm NostosSocket instance — enough surface for connect/signOut/close. */
class FakeSocket {
  constructor() {
    this.rowCount = 0;
    this.checkpoint = 0;
    this.closed = 0;
    this.cleared = 0;
  }
  rowsFor() {
    return [];
  }
  write() {}
  close() {
    this.closed++;
  }
  clearLocalState() {
    this.cleared++;
  }
}

/** Every socket the fake engine ever created (reconnect tests inspect them). */
const sockets = [];

/** NostosWeb with the wasm loader stubbed out — no browser needed. */
class NostosWebFake extends NostosWeb {
  async loadWasm() {
    return {
      connect: async () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s;
      },
    };
  }
}

/** One captured fetch: url, method, lowercased headers, verbatim body. */
const calls = [];

/**
 * Mock global fetch, capturing every request and replying [status] (+ [body]
 * for non-204). Restored by each test's finalizer.
 */
function mockFetch(status, body = "") {
  globalThis.fetch = async (input, init) => {
    const headers = {};
    new Headers(init?.headers ?? {}).forEach((v, k) => {
      headers[k.toLowerCase()] = v;
    });
    calls.push({
      url: String(input),
      method: init?.method,
      headers: headers,
      body: init?.body,
    });
    // 204 is a null-body status — Response forbids a body for it.
    return new Response(status === 204 ? null : body, { status: status });
  };
}

/** connect() a NostosWebFake against the fake engine + mocked REST wire. */
async function setup({ status = 204, body = "", token = "jwt-abc" } = {}) {
  mockFetch(status, body);
  const nostos = new NostosWebFake();
  // ws→http derivation is part of the pinned shape: the REST base comes from
  // the SAME server the sync session uses.
  await nostos.connect({ url: "ws://127.0.0.1:9/sync", token: token, table: "tasks" });
  return nostos;
}

test.afterEach(() => {
  calls.length = 0;
  sockets.length = 0;
});

test("registerPushToken POSTs the exact JSON body to /push-tokens with the sync JWT as Bearer", async () => {
  const nostos = await setup();

  await nostos.registerPushToken("apns", "tok-123");

  assert.equal(calls.length, 1);
  const r = calls[0];
  assert.equal(r.method, "POST");
  assert.equal(r.url, "http://127.0.0.1:9/push-tokens");
  assert.equal(r.headers.authorization, "Bearer jwt-abc");
  assert.equal(r.headers["content-type"], "application/json");
  assert.equal(r.body, '{"platform":"apns","token":"tok-123"}');
});

test("deregisterPushToken DELETEs the token path with the same auth", async () => {
  const nostos = await setup();

  await nostos.registerPushToken("fcm", "fcm-tok");
  await nostos.deregisterPushToken("fcm-tok");

  assert.equal(calls.length, 2);
  const r = calls[1];
  assert.equal(r.method, "DELETE");
  assert.equal(r.url, "http://127.0.0.1:9/push-tokens/fcm-tok");
  assert.equal(r.headers.authorization, "Bearer jwt-abc");
  assert.equal(r.body, undefined);
});

test("a token with URL-unsafe characters is percent-encoded on DELETE", async () => {
  const nostos = await setup();

  await nostos.deregisterPushToken("tok with spaces/+");

  assert.equal(calls[0].url, "http://127.0.0.1:9/push-tokens/tok%20with%20spaces%2F%2B");
});

test("a non-204 reply rejects with the status and body, and registers nothing", async () => {
  const nostos = await setup({ status: 401, body: '{"error":"unauthorized"}' });

  await assert.rejects(
    nostos.registerPushToken("fcm", "tok-123"),
    /401.*unauthorized/s,
  );

  // The failed registration must not be tracked — signOut sends no DELETE.
  await nostos.signOut();
  assert.equal(
    calls.filter((c) => c.method === "DELETE").length,
    0,
    "a failed register must not be deregistered on signOut",
  );
});

test("unknown platform / empty token throw before the wire", async () => {
  const nostos = await setup();

  await assert.rejects(nostos.registerPushToken("gcm", "tok"), /platform/);
  await assert.rejects(nostos.registerPushToken("fcm", ""), /token/);
  assert.equal(calls.length, 0, "validation must fail before the wire");
});

test("registerPushToken before connect() throws (no base URL yet)", async () => {
  mockFetch(204);
  const nostos = new NostosWebFake();
  await assert.rejects(nostos.registerPushToken("apns", "tok"), /connect/);
});

test("signOut deregisters every session-registered token (ADR-0037), idempotently", async () => {
  const nostos = await setup();
  await nostos.registerPushToken("apns", "tok-a");
  await nostos.registerPushToken("webpush", "tok-b");

  await nostos.signOut();

  const deletes = calls.filter((c) => c.method === "DELETE");
  assert.deepEqual(
    deletes.map((c) => c.url).sort(),
    ["http://127.0.0.1:9/push-tokens/tok-a", "http://127.0.0.1:9/push-tokens/tok-b"],
  );
  assert.ok(deletes.every((c) => c.headers.authorization === "Bearer jwt-abc"));
  // The core sign-out ran too: engine wiped + socket closed exactly once.
  assert.equal(sockets[0].cleared, 1);
  assert.equal(sockets[0].closed, 1);

  // Idempotent: a second signOut deregisters nothing (set already drained).
  const countBefore = calls.length;
  await nostos.signOut();
  assert.equal(calls.length, countBefore);
});

test("with no token set (NOSTOS_SYNC_AUTH=none), the REST call sends no Authorization header", async () => {
  const nostos = await setup({ token: null });

  await nostos.registerPushToken("apns", "tok");

  assert.equal(calls[0].headers.authorization, undefined);
});

test("reconnect() closes the live socket and re-opens with the last options (the foregroundPush wake path)", async () => {
  const nostos = await setup();

  const first = sockets[sockets.length - 1];
  const result = await nostos.reconnect();

  assert.equal(first.closed, 1, "old socket closed");
  assert.equal(sockets.length, 2, "a new socket was opened");
  assert.deepEqual(result, { rowCount: 0, checkpoint: 0 });
  // A second reconnect is equally fine (close+connect is idempotent).
  await nostos.reconnect();
  assert.equal(sockets.length, 3);
});

test("reconnect() before any connect() throws", async () => {
  const nostos = new NostosWebFake();
  await assert.rejects(nostos.reconnect(), /connect/);
});

test("registerForPushNotifications rejects on the web implementation (web push is plan task 6.2)", async () => {
  const nostos = await setup();
  await assert.rejects(nostos.registerForPushNotifications(), /web push/);
});
