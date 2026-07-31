// Offline Jest smoke for the reactive watch() facade.
//
// This test proves the watch() PUSH wiring WITHOUT a device: it mocks
// `NativeNostos.watchChanges` to capture the retained bridge callback (the RN
// analogue of napi's ThreadsafeFunction / UniFFI's SnapshotSink), then invokes
// that bridge the way the Wave-B native change pump would — initial snapshot
// first, then a change delta — and asserts the facade decodes + forwards each
// emission to the app's onSnapshot. Also covers late-watcher initial-snapshot
// synthesis, per-table fan-out, per-handle unsubscribe, native-pump teardown on
// the last unsubscribe, idempotent unsubscribe, and unsafe-name rejection.

import NativeNostos from "../src/NativeNostos";
import { NostosClient } from "../src/NostosClient";

// Replace the entire NativeNostos module with a typed mock. In a real RN app,
// `TurboModuleRegistry.getEnforcing("NativeNostos")` returns the Wave-B native
// instance; without a device it throws, so we mock — including the push seam.
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
  },
}));

const native = NativeNostos as jest.Mocked<typeof NativeNostos>;

/**
 * Drive `watchChanges` by capturing its bridge callback, the way the native
 * change pump holds + invokes it. Returns a LIVE holder object — read
 * `.push` (do NOT destructure it) so the mock's later assignment is visible.
 */
function captureBridge(): {
  push: ((rowsJson: string) => void) | null;
} {
  const ref: { push: ((rowsJson: string) => void) | null } = { push: null };
  native.watchChanges.mockImplementation(async (_table, onSnapshot) => {
    ref.push = onSnapshot;
  });
  return ref;
}

describe("@nostos-sync/react-native — reactive watch() (offline smoke)", () => {
  beforeEach(() => {
    jest.clearAllMocks();
  });

  it("emits the initial snapshot then each change via the native push pump", async () => {
    const bridge = captureBridge();

    const snapshots: object[][] = [];
    const client = new NostosClient();

    await client.watch("tasks", (rows) => {
      snapshots.push(rows);
    });

    // The facade registered exactly one bridge with the native pump.
    expect(native.watchChanges).toHaveBeenCalledWith(
      "tasks",
      expect.any(Function),
    );
    expect(bridge.push).not.toBeNull();
    const push = bridge.push as (rowsJson: string) => void;

    // (1) initial snapshot — the native pump invokes the bridge once on
    //     subscribe, with a FULL snapshot JSON string.
    push(JSON.stringify([{ id: "t1", title: "Walk dog", done: false }]));
    // (2) a change delta — invoked again after an applied change. Full snapshot
    //     per tick (NOT a diff), decoded the same way `query()` decodes.
    push(JSON.stringify([{ id: "t1", title: "Walk dog", done: true }]));

    expect(snapshots).toEqual([
      [{ id: "t1", title: "Walk dog", done: false }],
      [{ id: "t1", title: "Walk dog", done: true }],
    ]);
    // The poll path was never used — the first watcher's initial snapshot came
    // from the native push, not a synthesized query.
    expect(native.query).not.toHaveBeenCalled();
  });

  it("synthesizes an initial snapshot for a late watcher via a one-shot query", async () => {
    // First watcher starts the pump (native emits its own initial snapshot).
    native.watchChanges.mockResolvedValue(undefined);
    native.query.mockResolvedValue(JSON.stringify([{ id: "a" }]));

    const client = new NostosClient();
    const first: object[][] = [];
    await client.watch("tasks", (rows) => first.push(rows));
    // The first watcher did NOT synthesize — query untouched so far.
    expect(native.query).not.toHaveBeenCalled();

    // Late watcher: the pump is already running, so the facade synthesizes its
    // initial snapshot with a single SELECT *.
    const late: object[][] = [];
    await client.watch("tasks", (rows) => late.push(rows));
    expect(native.query).toHaveBeenCalledTimes(1);
    expect(native.query).toHaveBeenCalledWith("SELECT * FROM tasks");
    expect(late).toEqual([[{ id: "a" }]]);

    // Only ONE native pump for the table — the second watch did NOT start
    // another (multiplexed over the single per-table pump).
    expect(native.watchChanges).toHaveBeenCalledTimes(1);
  });

  it("fans each native tick out to every live handle on that table", async () => {
    const bridge = captureBridge();
    native.query.mockResolvedValue(JSON.stringify([{ id: "a" }]));

    const client = new NostosClient();
    const a: object[][] = [];
    const b: object[][] = [];
    await client.watch("tasks", (rows) => a.push(rows));
    await client.watch("tasks", (rows) => b.push(rows)); // late watcher

    const push = bridge.push as (rowsJson: string) => void;
    push(JSON.stringify([{ id: "x" }]));
    expect(a).toEqual([[{ id: "x" }]]);
    expect(b).toEqual([
      [{ id: "a" }], // synthesized initial snapshot
      [{ id: "x" }], // fanned-out native tick
    ]);
  });

  it("unsubscribe() stops forwarding to that handle; last unsubscribe tears down the pump", async () => {
    const bridge = captureBridge();
    native.unwatchChanges.mockResolvedValue(undefined);
    native.query.mockResolvedValue(JSON.stringify([]));

    const client = new NostosClient();
    const a: object[][] = [];
    const b: object[][] = [];
    const subA = await client.watch("tasks", (rows) => a.push(rows));
    const subB = await client.watch("tasks", (rows) => b.push(rows));

    const push = bridge.push as (rowsJson: string) => void;
    push(JSON.stringify([{ id: "x" }]));
    expect(a).toEqual([[{ id: "x" }]]);
    // B is a LATE watcher — it already received a synthesized initial snapshot
    // (query → []) before this fanned tick.
    expect(b).toEqual([[], [{ id: "x" }]]);

    // Close A — it stops receiving; the pump stays up for B (no unwatch yet).
    subA.unsubscribe();
    expect(native.unwatchChanges).not.toHaveBeenCalled();
    push(JSON.stringify([{ id: "y" }]));
    expect(a).toEqual([[{ id: "x" }]]); // A is closed — unchanged
    expect(b).toEqual([[], [{ id: "x" }], [{ id: "y" }]]);

    // Close B (the last handle) — the native pump is torn down.
    subB.unsubscribe();
    expect(native.unwatchChanges).toHaveBeenCalledTimes(1);
    expect(native.unwatchChanges).toHaveBeenCalledWith("tasks");

    // A native tick after teardown is not forwarded to anyone.
    push(JSON.stringify([{ id: "z" }]));
    expect(b).toEqual([[], [{ id: "x" }], [{ id: "y" }]]); // B closed — unchanged
  });

  it("unsubscribe() is idempotent (a second call does not re-teardown)", async () => {
    native.watchChanges.mockResolvedValue(undefined);
    native.query.mockResolvedValue(JSON.stringify([]));
    const client = new NostosClient();
    const sub = await client.watch("tasks", () => {
      /* no-op */
    });
    sub.unsubscribe();
    sub.unsubscribe(); // no-op — only one unwatchChanges.
    expect(native.unwatchChanges).toHaveBeenCalledTimes(1);
  });

  it("a fresh watch() after teardown restarts the native pump", async () => {
    const bridge = captureBridge();
    native.unwatchChanges.mockResolvedValue(undefined);
    native.query.mockResolvedValue(JSON.stringify([]));

    const client = new NostosClient();
    const sub = await client.watch("tasks", () => {
      /* no-op */
    });
    sub.unsubscribe(); // tears down the pump + drops the bundle
    expect(native.unwatchChanges).toHaveBeenCalledTimes(1);

    // Re-watching starts a NEW pump (watchChanges called twice total) and
    // re-registers a fresh bridge.
    await client.watch("tasks", () => {
      /* no-op */
    });
    expect(native.watchChanges).toHaveBeenCalledTimes(2);
    expect(bridge.push).not.toBeNull();
  });

  it("watch() rejects unsafe table names (the replay path builds SQL)", async () => {
    const client = new NostosClient();
    await expect(
      client.watch("tasks; DROP TABLE tasks;--", () => {
        /* no-op */
      }),
    ).rejects.toThrow(/unsafe table name/);
    expect(native.watchChanges).not.toHaveBeenCalled();
    expect(native.query).not.toHaveBeenCalled();
  });
});
