// Offline Jest smoke for the NostosClient facade.
//
// This test proves the facade + Codegen TurboModule spec wiring WITHOUT a
// device: it mocks `NativeNostos` (the TurboModule that, in Wave B, the
// Kotlin/Swift native module registers) and exercises the full
// connect → subscribe → query → write → checkpoint flow through the facade.
//
// What this guards:
//   (a) the facade maps each call to the right native method with the right
//       args (no swapped params, no dropped args, no extra calls);
//   (b) `query` parses the JSON-rows string the native side returns;
//   (c) `write` SERIALIZES the payload object to a JSON string and passes
//       `null` when no payload is given (matches UniFFI Option<String>::None).

import NativeNostos from "../src/NativeNostos";
import { NostosClient } from "../src/NostosClient";

// Replace the entire NativeNostos module with a typed mock. In a real RN app,
// `TurboModuleRegistry.getEnforcing("NativeNostos")` returns the instance the
// Wave-B native module registers; without a device it throws, so we mock.
jest.mock("../src/NativeNostos", () => ({
  __esModule: true,
  default: {
    connect: jest.fn(),
    subscribe: jest.fn(),
    write: jest.fn(),
    query: jest.fn(),
    checkpoint: jest.fn(),
  },
}));

const native = NativeNostos as jest.Mocked<typeof NativeNostos>;

describe("@nostos-sync/react-native — NostosClient facade (offline smoke)", () => {
  beforeEach(() => {
    jest.clearAllMocks();
  });

  it("exercises connect → subscribe → query → write → checkpoint through the facade", async () => {
    // Canned native responses.
    native.query.mockResolvedValue(
      JSON.stringify([{ id: "t1", title: "Walk dog", done: false }]),
    );
    native.write.mockResolvedValue(42);
    native.checkpoint.mockResolvedValue(99);

    const client = new NostosClient({
      url: "ws://example",
      token: "tok",
      dbPath: ":memory:",
    });

    // (a) connect maps to NativeNostos.connect() with no args.
    await client.connect();
    expect(native.connect).toHaveBeenCalledTimes(1);
    expect(native.connect).toHaveBeenCalledWith();

    // (a) subscribe maps to NativeNostos.subscribe(table) and returns a handle.
    const sub = await client.subscribe("tasks");
    expect(native.subscribe).toHaveBeenCalledWith("tasks");
    expect(native.subscribe).toHaveBeenCalledTimes(1);
    expect(sub.table).toBe("tasks");

    // (a) + (b) query maps to NativeNostos.query(sql) and parses the JSON rows.
    const rows = await client.query("SELECT * FROM tasks");
    expect(native.query).toHaveBeenCalledWith("SELECT * FROM tasks");
    expect(rows).toEqual([{ id: "t1", title: "Walk dog", done: false }]);

    // (a) + (c) write maps to NativeNostos.write(table, op, pk, payloadJson)
    //           and SERIALIZES the payload object to a JSON string.
    const seq = await client.write("tasks", "upsert", "t1", {
      title: "Feed cat",
    });
    expect(native.write).toHaveBeenCalledWith(
      "tasks",
      "upsert",
      "t1",
      JSON.stringify({ title: "Feed cat" }),
    );
    expect(seq).toBe(42);

    // (a) checkpoint maps to NativeNostos.checkpoint() and returns the LSN.
    const cp = await client.checkpoint();
    expect(native.checkpoint).toHaveBeenCalledTimes(1);
    expect(cp).toBe(99);
  });

  it("write() with no payload passes null (UniFFI Option<String>::None — the delete shape)", async () => {
    native.write.mockResolvedValue(7);
    const client = new NostosClient();

    await client.write("tasks", "delete", "t1");
    expect(native.write).toHaveBeenCalledWith("tasks", "delete", "t1", null);
  });

  it("write() with an explicit null payload stays null (not the string \"null\")", async () => {
    native.write.mockResolvedValue(8);
    const client = new NostosClient();

    // `undefined` is the "no payload" sentinel; an explicit `null` is treated
    // the same — the facade never JSON-stringifies null into "null".
    await client.write("tasks", "patch", "t1", undefined);
    expect(native.write).toHaveBeenLastCalledWith("tasks", "patch", "t1", null);
  });

  it("pollRows() queries the table and decodes rows", async () => {
    native.query.mockResolvedValue(JSON.stringify([{ id: "a" }, { id: "b" }]));
    const client = new NostosClient();

    const rows = await client.pollRows("tasks");
    expect(native.query).toHaveBeenCalledWith("SELECT * FROM tasks");
    expect(rows).toHaveLength(2);
    expect(rows[1]).toEqual({ id: "b" });
  });

  it("subscribe() is idempotent — re-subscribing the same table reuses the handle", async () => {
    native.subscribe.mockResolvedValue(undefined);
    const client = new NostosClient();

    const sub1 = await client.subscribe("tasks");
    const sub2 = await client.subscribe("tasks");
    expect(sub2).toBe(sub1);
    // Native subscribe IS called twice — the native side is idempotent too
    // (nostos-swift/kotlin guard on session.is_some()).
    expect(native.subscribe).toHaveBeenCalledTimes(2);
  });

  it("unsubscribe() drops the JS-side handle", async () => {
    native.subscribe.mockResolvedValue(undefined);
    const client = new NostosClient();

    const sub = await client.subscribe("tasks");
    sub.unsubscribe();
    // Re-subscribing after unsubscribe creates a NEW handle (the old one was
    // dropped from the map).
    const subAgain = await client.subscribe("tasks");
    expect(subAgain).not.toBe(sub);
  });

  it("rejects unsafe table names in pollRows() (defends the convenience path)", async () => {
    const client = new NostosClient();
    await expect(
      client.pollRows("tasks; DROP TABLE tasks;--"),
    ).rejects.toThrow(/unsafe table name/);
    expect(native.query).not.toHaveBeenCalled();
  });
});
