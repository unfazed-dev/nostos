// T6 attachment driver — node round-trip + dead-letter tests (ADR-0034).
//
// Pure-node: uses node:test + an in-memory fake gateway + fake adapter (no
// browser, no OPFS, no @supabase/supabase-js, no wasm). Proves the same
// queue→reconnect→upload→second-client-download→dead-letter path the Flutter
// suite covers, against the SAME state machine, so the two SDK drivers are
// guarded against divergence (the advisor's HIGH risk).
//
// Run: node --test sdk/nostos_web/e2e/attachments.spec.cjs
//
// NOT exercised here: the metadata plane's actual replication (the browser
// Worker + live transport — the existing worker.spec.cjs + the Wave-2 durable
// suite cover that path); the real Supabase-Storage round-trip (called out as
// untested-environment in the wave report — no project configured here).

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");

const {
  Attachments,
  STATE,
  COL,
} = require("../attachments.js");

/** In-memory metadata gateway — stands in for the Worker postMessage gateway. */
class FakeGateway {
  constructor() {
    this.rows = new Map(); // id → row object
  }
  async queuedRows() {
    const queued = new Set([STATE.queuedUpload, STATE.queuedDownload, STATE.queuedDelete]);
    return [...this.rows.values()].filter((r) => queued.has(r[COL.state]));
  }
  async patchState(id, state) {
    const r = this.rows.get(id);
    if (r) r[COL.state] = state;
  }
  async upsertRow(row) {
    this.rows.set(row[COL.id], { ...row });
  }
  async currentState(id) {
    return this.rows.get(id)?.[COL.state] ?? STATE.synced;
  }
}

/** In-memory local blob store. */
class MemBlobStore {
  constructor() {
    this.data = new Map();
    this.wipeCalls = 0;
  }
  async put(id, bytes) {
    this.data.set(id, bytes);
  }
  async get(id) {
    return this.data.get(id) ?? null;
  }
  async remove(id) {
    this.data.delete(id);
  }
  async wipe() {
    this.wipeCalls++;
    this.data.clear();
  }
  has(id) {
    return this.data.has(id);
  }
}

/** Fake remote adapter — stands in for Supabase Storage. Shared between clients. */
class FakeAdapter {
  constructor() {
    this.remote = new Map();
    this.uploadCalls = 0;
    this.downloadCalls = 0;
    this.deleteCalls = 0;
    this.uploadError = null; // when set, every upload throws
  }
  async upload(path, bytes) {
    this.uploadCalls++;
    if (this.uploadError) throw this.uploadError;
    this.remote.set(path, bytes);
  }
  async download(path) {
    this.downloadCalls++;
    const b = this.remote.get(path);
    if (!b) throw new Error("not found: " + path);
    return b;
  }
  async delete(path) {
    this.deleteCalls++;
    this.remote.delete(path); // idempotent
  }
}

const online = () => async () => true;

test("queueUpload caches bytes locally and enqueues a queued_upload row", async () => {
  const gw = new FakeGateway();
  const blob = new MemBlobStore();
  const adapter = new FakeAdapter();
  const driver = new Attachments({ gateway: gw, adapter, blobStore: blob, isOnline: online() });

  const id = await driver.queueUpload({
    filename: "photo.png",
    bytes: Uint8Array.of(1, 2, 3, 4),
    mediaType: "image/png",
  });

  assert.ok(blob.has(id), "bytes cached locally");
  assert.equal(gw.rows.get(id)[COL.state], STATE.queuedUpload);
  assert.equal(gw.rows.get(id)[COL.size], 4);
  assert.equal(adapter.uploadCalls, 0, "not yet pumped");
});

test("pump uploads on reconnect and flips state to synced", async () => {
  const gw = new FakeGateway();
  const blob = new MemBlobStore();
  const adapter = new FakeAdapter();
  const driver = new Attachments({ gateway: gw, adapter, blobStore: blob, isOnline: online() });

  const id = await driver.queueUpload({
    filename: "a.bin",
    bytes: Uint8Array.of(9, 9),
    mediaType: "application/octet-stream",
  });
  await driver.pump();

  assert.equal(adapter.uploadCalls, 1);
  assert.deepEqual(adapter.remote.get(id), Uint8Array.of(9, 9));
  assert.equal(gw.rows.get(id)[COL.state], STATE.synced);
  assert.equal(driver.lastErrorFor(id), null);
});

test("second client downloads the blob through the shared bucket", async () => {
  // Client A uploads.
  const gwA = new FakeGateway();
  const blobA = new MemBlobStore();
  const adapter = new FakeAdapter(); // shared "remote bucket"
  const driverA = new Attachments({ gateway: gwA, adapter, blobStore: blobA, isOnline: online() });
  const id = await driverA.queueUpload({
    filename: "shared.png",
    bytes: Uint8Array.of(5, 6, 7),
    mediaType: "image/png",
  });
  await driverA.pump();
  assert.deepEqual(adapter.remote.get(id), Uint8Array.of(5, 6, 7));

  // Client B: simulate the synced metadata arriving via replication.
  const gwB = new FakeGateway();
  const blobB = new MemBlobStore();
  const driverB = new Attachments({ gateway: gwB, adapter, blobStore: blobB, isOnline: online() });
  await gwB.upsertRow({
    [COL.id]: id,
    [COL.state]: STATE.synced,
    [COL.filename]: "shared.png",
    [COL.mediaType]: "image/png",
    [COL.size]: 3,
  });
  await driverB.queueDownload(id);
  assert.equal(gwB.rows.get(id)[COL.state], STATE.queuedDownload);
  await driverB.pump();

  assert.equal(adapter.downloadCalls, 1);
  assert.ok(blobB.has(id), "bytes landed on client B");
  assert.deepEqual(blobB.data.get(id), Uint8Array.of(5, 6, 7));
  assert.equal(gwB.rows.get(id)[COL.state], STATE.synced);
});

test("adapter failure retries with backoff then dead-letters to archived", async () => {
  const gw = new FakeGateway();
  const blob = new MemBlobStore();
  const adapter = new FakeAdapter();
  adapter.uploadError = new Error("bucket unreachable");
  let now = new Date("2026-01-01T00:00:00Z");
  const driver = new Attachments({
    gateway: gw,
    adapter,
    blobStore: blob,
    isOnline: online(),
    maxAttempts: 2,
    now: () => now,
  });

  const id = await driver.queueUpload({
    filename: "failing.bin",
    bytes: Uint8Array.of(1),
    mediaType: "application/octet-stream",
  });

  // Attempt 1: fails, schedules backoff.
  await driver.pump();
  assert.equal(adapter.uploadCalls, 1);
  assert.equal(gw.rows.get(id)[COL.state], STATE.queuedUpload, "still queued after fail");
  assert.match(driver.lastErrorFor(id), /bucket unreachable/);

  // Same instant: backoff not elapsed → skipped.
  await driver.pump();
  assert.equal(adapter.uploadCalls, 1);

  // After backoff: attempt 2 → fail → dead-letter (max=2).
  now = new Date(now.getTime() + 3000);
  await driver.pump();
  assert.equal(adapter.uploadCalls, 2);
  assert.equal(gw.rows.get(id)[COL.state], STATE.archived);
  assert.match(driver.lastErrorFor(id), /bucket unreachable/);
});

test("delete archives the blob in the remote bucket", async () => {
  const gw = new FakeGateway();
  const blob = new MemBlobStore();
  const adapter = new FakeAdapter();
  const driver = new Attachments({ gateway: gw, adapter, blobStore: blob, isOnline: online() });
  const id = await driver.queueUpload({
    filename: "gone.bin",
    bytes: Uint8Array.of(1),
    mediaType: "application/octet-stream",
  });
  await driver.pump(); // upload + synced
  assert.ok(adapter.remote.has(id));

  await driver.remove(id); // queue delete
  assert.equal(gw.rows.get(id)[COL.state], STATE.queuedDelete);
  await driver.pump(); // adapter.delete → archived
  assert.equal(adapter.deleteCalls, 1);
  assert.ok(!adapter.remote.has(id));
  assert.equal(gw.rows.get(id)[COL.state], STATE.archived);
});

test("driver does not dispatch while offline", async () => {
  const gw = new FakeGateway();
  const blob = new MemBlobStore();
  const adapter = new FakeAdapter();
  let isOnline = false;
  const driver = new Attachments({
    gateway: gw,
    adapter,
    blobStore: blob,
    isOnline: async () => isOnline,
  });
  const id = await driver.queueUpload({
    filename: "offline.bin",
    bytes: Uint8Array.of(1),
    mediaType: "application/octet-stream",
  });
  await driver.pump(); // offline → no-op
  assert.equal(adapter.uploadCalls, 0);
  assert.equal(gw.rows.get(id)[COL.state], STATE.queuedUpload);

  isOnline = true; // reconnect
  await driver.pump();
  assert.equal(adapter.uploadCalls, 1);
  assert.equal(gw.rows.get(id)[COL.state], STATE.synced);
});

test("wipe on the blob store clears local bytes (sign-out parity)", async () => {
  // The driver doesn't call wipe itself; the host calls blobStore.wipe() on
  // signOut (ADR-0029), mirroring Flutter's registerSignOutHook. This test
  // pins the BlobStore contract the host relies on.
  const blob = new MemBlobStore();
  await blob.put("x", Uint8Array.of(1));
  await blob.wipe();
  assert.equal(blob.wipeCalls, 1);
  assert.ok(!blob.has("x"), "wipe cleared local bytes");
});
