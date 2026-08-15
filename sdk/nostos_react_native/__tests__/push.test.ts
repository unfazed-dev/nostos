// Offline Jest smoke for the push/wake surface (ADR-0037, plan task 5.3).
//
// Proves the facade wiring WITHOUT a device by mocking `NativeNostos` (the way
// signout.test.ts does) AND the REST endpoint (global fetch — the seam the
// registration methods ride; RN and Node both provide the global):
//   • disconnect()/resume() delegate to the TurboModule spec methods that
//     bridge UniFFI's non-destructive teardown pair (plan task 5.1);
//   • registerPushToken POSTs the pinned JSON body to the derived HTTP base
//     with the sync JWT as Bearer (tenant/account never sent);
//   • deregisterPushToken DELETEs /push-tokens/{token} with the same auth;
//   • the strict-204 contract: any other status (including 2xx variants)
//     rejects with a typed NostosPushError carrying status + body;
//   • signOut deregisters session-registered tokens best-effort using the JWT
//     captured BEFORE the wipe, and still resolves when a DELETE fails.

import NativeNostos from "../src/NativeNostos";
import { NostosClient, NostosPushError } from "../src/NostosClient";

// Replace the entire NativeNostos module with a typed mock (signout.test.ts
// pattern) — the facade's disconnect/resume delegation is under test, not the
// native layer.
jest.mock("../src/NativeNostos", () => ({
  __esModule: true,
  default: {
    connect: jest.fn(),
    subscribe: jest.fn(),
    write: jest.fn(),
    query: jest.fn(),
    checkpoint: jest.fn(),
    watchChanges: jest.fn(),
    unwatchChanges: jest.fn(),
    setToken: jest.fn(),
    signOut: jest.fn(),
    disconnect: jest.fn(),
    resume: jest.fn(),
  },
}));

const native = NativeNostos as jest.Mocked<typeof NativeNostos>;

// The REST seam: jest's node env provides fetch (Node ≥18), but we mock the
// global so request shape + auth are assertable without a server.
const fetchMock = jest.fn<Promise<{ status: number; text(): Promise<string> }>, unknown[]>();

/** Minimal 204-style response with the exact surface the facade reads. */
function res(status: number, body = ""): { status: number; text(): Promise<string> } {
  return { status, text: () => Promise.resolve(body) };
}

function newClient(token: string | null = "jwt-1"): NostosClient {
  return new NostosClient({ url: "ws://example:8080/sync", token });
}

describe("@nostos-sync/react-native — push tokens (offline smoke)", () => {
  beforeEach(() => {
    jest.clearAllMocks();
    (globalThis as { fetch: unknown }).fetch = fetchMock;
    fetchMock.mockResolvedValue(res(204));
  });

  it("registerPushToken POSTs the pinned JSON with the sync JWT as Bearer", async () => {
    const client = newClient("rn-jwt");
    await client.registerPushToken("fcm", "tok-1");

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [
      string,
      { method: string; headers: Record<string, string>; body: string },
    ];
    // ws→http derivation + path strip: the /sync path never leaks into REST.
    expect(url).toBe("http://example:8080/push-tokens");
    expect(init.method).toBe("POST");
    expect(init.headers["Authorization"]).toBe("Bearer rn-jwt");
    expect(init.headers["Content-Type"]).toBe("application/json");
    // The exact pinned body — server routes are built against this pin.
    expect(init.body).toBe('{"platform":"fcm","token":"tok-1"}');
  });

  it("registerPushToken with no token sends NO Authorization header (anonymous)", async () => {
    const client = newClient(null);
    await client.registerPushToken("apns", "tok-1");

    const [, init] = fetchMock.mock.calls[0] as unknown as [
      string,
      { headers: Record<string, string> },
    ];
    expect(init.headers["Authorization"]).toBeUndefined();
  });

  it("derives https from wss (wss→https mirrors node's http_base)", async () => {
    const client = new NostosClient({ url: "wss://nostos.example/sync" });
    await client.registerPushToken("webpush", "tok-1");

    const [url] = fetchMock.mock.calls[0] as unknown as [string];
    expect(url).toBe("https://nostos.example/push-tokens");
  });

  it("a non-204 rejects with a typed NostosPushError carrying status + body", async () => {
    fetchMock.mockResolvedValue(res(401, '{"error":"unauthorized"}'));
    const client = newClient("stale-jwt");

    await expect(client.registerPushToken("fcm", "tok-1")).rejects.toMatchObject({
      name: "NostosPushError",
      operation: "register",
      status: 401,
      body: '{"error":"unauthorized"}',
    });
  });

  it("a 2xx that is not 204 still rejects (strict contract)", async () => {
    fetchMock.mockResolvedValue(res(200, '{"ok":true}'));
    const client = newClient();

    await expect(client.registerPushToken("fcm", "tok-1")).rejects.toBeInstanceOf(
      NostosPushError,
    );
  });

  it("a failed register does NOT track the token (signOut sends no DELETE)", async () => {
    fetchMock.mockResolvedValue(res(500, "boom"));
    native.signOut.mockResolvedValue(undefined);
    const client = newClient();

    await expect(client.registerPushToken("fcm", "tok-1")).rejects.toBeInstanceOf(
      NostosPushError,
    );
    fetchMock.mockResolvedValue(res(204));
    await client.signOut();
    // Only the failed POST — no best-effort DELETE for an untracked token.
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("an unknown platform fails before the wire", async () => {
    const client = newClient();
    // The platform is typed, but JS callers can bypass types — runtime-guard
    // like the facade's unsafe-table-name defense.
    await expect(
      client.registerPushToken("gcm" as never, "tok-1"),
    ).rejects.toThrow(/unknown push platform/);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("a transport failure maps to NostosPushError without a status", async () => {
    fetchMock.mockRejectedValue(new Error("network down"));
    const client = newClient();

    const err = await client.registerPushToken("fcm", "tok-1").catch((e) => e);
    expect(err).toBeInstanceOf(NostosPushError);
    expect((err as NostosPushError).status).toBeUndefined();
    expect((err as NostosPushError).operation).toBe("register");
    expect((err as NostosPushError).message).toContain("network down");
  });

  it("deregisterPushToken DELETEs the token path with Bearer auth", async () => {
    const client = newClient("rn-jwt");
    await client.deregisterPushToken("tok-1");

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as unknown as [
      string,
      { method: string; headers: Record<string, string> },
    ];
    expect(url).toBe("http://example:8080/push-tokens/tok-1");
    expect(init.method).toBe("DELETE");
    expect(init.headers["Authorization"]).toBe("Bearer rn-jwt");
  });

  it("signOut deregisters session-registered tokens with the JWT captured BEFORE the wipe", async () => {
    native.signOut.mockResolvedValue(undefined);
    const client = newClient("jwt-A");
    await client.registerPushToken("fcm", "tok-1");
    await client.registerPushToken("apns", "tok-2");
    fetchMock.mockClear();

    await client.signOut();

    expect(native.signOut).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    const urls = fetchMock.mock.calls.map(
      (c) => (c as unknown as [string])[0],
    );
    expect(urls).toEqual([
      "http://example:8080/push-tokens/tok-1",
      "http://example:8080/push-tokens/tok-2",
    ]);
    // The DELETEs authorize as the signing-out principal, even though the
    // facade already mirrored the token clear (config.token === null).
    expect(client.config.token).toBeNull();
    for (const call of fetchMock.mock.calls) {
      const init = (call as unknown as [string, { headers: Record<string, string> }])[1];
      expect(init.headers["Authorization"]).toBe("Bearer jwt-A");
    }
  });

  it("signOut still resolves when a best-effort DELETE fails", async () => {
    native.signOut.mockResolvedValue(undefined);
    fetchMock.mockResolvedValueOnce(res(204)).mockResolvedValueOnce(res(503, "down"));
    const client = newClient("jwt-A");
    await client.registerPushToken("fcm", "tok-1");
    fetchMock.mockClear();

    // A failed DELETE must not break the sign-out flow (server-side rail
    // pruning covers the stale row).
    await expect(client.signOut()).resolves.toBeUndefined();
  });
});

describe("@nostos-sync/react-native — disconnect()/resume() (offline smoke)", () => {
  beforeEach(() => {
    jest.clearAllMocks();
    (globalThis as { fetch: unknown }).fetch = fetchMock;
  });

  it("disconnect() delegates to NativeNostos.disconnect (non-destructive pause)", async () => {
    native.disconnect.mockResolvedValue(undefined);
    const client = newClient();

    await client.disconnect();

    expect(native.disconnect).toHaveBeenCalledTimes(1);
    expect(native.disconnect).toHaveBeenCalledWith();
    // Non-destructive: the facade's session bookkeeping survives (contrast
    // signOut, which clears it — proven in signout.test.ts).
    expect(client.config.token).toBe("jwt-1");
  });

  it("resume() delegates to NativeNostos.resume (the push wake primitive)", async () => {
    native.resume.mockResolvedValue(undefined);
    const client = newClient();

    await client.resume();

    expect(native.resume).toHaveBeenCalledTimes(1);
    expect(native.resume).toHaveBeenCalledWith();
  });
});
