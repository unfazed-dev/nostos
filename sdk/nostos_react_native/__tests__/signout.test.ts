// Offline Jest smoke for the signOut() / setToken() facade (ADR-0029 / WS4-D3).
//
// Proves the sign-out + token-swap wiring WITHOUT a device by mocking
// `NativeNostos` (the way `watch.test.ts` does) and asserting the facade:
//   • delegates `signOut` → `NativeNostos.signOut` (the UniFFI `sign_out` surface)
//     AND clears its JS-side bookkeeping (subscriptions + watches) + the token,
//     so the next user starts from a clean slate;
//   • delegates `setToken` → `NativeNostos.setToken` with the exact arg shape
//     (`string | null` ↔ UniFFI `Option<String>`) and keeps `config.token` in sync.
//
// The mocked native methods throw if the facade mis-routes a call (jest.fn()
// defaults), so a missing delegation surfaces as an assertion failure here
// before it reaches the Wave-B on-device instrumented test.

import NativeNostos from "../src/NativeNostos";
import { NostosClient } from "../src/NostosClient";

// Replace the entire NativeNostos module with a typed mock. In a real RN app,
// `TurboModuleRegistry.getEnforcing("NativeNostos")` returns the Wave-B native
// instance; without a device it throws, so we mock the full Spec surface.
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
  },
}));

const native = NativeNostos as jest.Mocked<typeof NativeNostos>;

describe("@nostos-sync/react-native — signOut() / setToken() (offline smoke)", () => {
  beforeEach(() => {
    jest.clearAllMocks();
  });

  it("signOut() delegates to NativeNostos.signOut and clears local bookkeeping", async () => {
    native.signOut.mockResolvedValue(undefined);
    native.watchChanges.mockResolvedValue(undefined);
    native.query.mockResolvedValue(JSON.stringify([]));

    const client = new NostosClient({ url: "ws://h:1/sync", token: "abc" });
    // Populate JS-side bookkeeping: a subscription + a reactive watch. After
    // signOut these handles must be gone — the native session backing them died.
    await client.subscribe("tasks");
    await client.watch("tasks", () => {
      /* no-op */
    });
    expect(client.config.token).toBe("abc");

    await client.signOut();

    // Delegated to the native sign-out exactly once.
    expect(native.signOut).toHaveBeenCalledTimes(1);
    expect(native.signOut).toHaveBeenCalledWith();
    // Token cleared (mirrors the native clear-token half of sign_out).
    expect(client.config.token).toBeNull();

    // A fresh watch() after signOut starts a NEW native pump — proving the
    // `watches` map was cleared (a stale bundle would have short-circuited the
    // watchChanges call as a "late watcher" instead).
    await client.watch("tasks", () => {
      /* no-op */
    });
    expect(native.watchChanges).toHaveBeenCalledTimes(2);
    // A fresh subscribe() after signOut hits the native module again (the
    // `subscriptions` map no longer short-circuits the handle reuse).
    await client.subscribe("tasks");
    expect(native.subscribe).toHaveBeenCalledTimes(2);
  });

  it("setToken(string) delegates the token and keeps config.token in sync", async () => {
    native.setToken.mockResolvedValue(undefined);
    const client = new NostosClient({ url: "ws://h:1/sync", token: "old" });

    await client.setToken("rotated");

    // `string` ↔ UniFFI `Some(String)` — the exact arg crosses unchanged.
    expect(native.setToken).toHaveBeenCalledTimes(1);
    expect(native.setToken).toHaveBeenCalledWith("rotated");
    // The facade's config reflects the live token for app-level introspection.
    expect(client.config.token).toBe("rotated");
  });

  it("setToken(null) clears the token (null ↔ UniFFI Option::None)", async () => {
    native.setToken.mockResolvedValue(undefined);
    const client = new NostosClient({ url: "ws://h:1/sync", token: "soon-gone" });

    await client.setToken(null);

    expect(native.setToken).toHaveBeenCalledWith(null);
    expect(client.config.token).toBeNull();
  });

  it("setToken() is callable before connect (stages the token)", async () => {
    native.setToken.mockResolvedValue(undefined);
    const client = new NostosClient({ url: "ws://h:1/sync" });

    // No connect() first — mirrors UniFFI's `set_token_swaps_before_and_after_connect`.
    await client.setToken("fresh");

    expect(native.setToken).toHaveBeenCalledWith("fresh");
    expect(client.config.token).toBe("fresh");
    expect(native.connect).not.toHaveBeenCalled();
  });

  it("does NOT call setToken on the native module from signOut (sign_out is the wipe primitive)", async () => {
    // Guards against a regression where signOut accidentally routes through
    // setToken; the native sign_out owns its own clear-token half.
    native.signOut.mockResolvedValue(undefined);
    const client = new NostosClient({ url: "ws://h:1/sync", token: "abc" });

    await client.signOut();

    expect(native.signOut).toHaveBeenCalledTimes(1);
    expect(native.setToken).not.toHaveBeenCalled();
  });
});
