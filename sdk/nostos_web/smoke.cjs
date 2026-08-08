// Node smoke for @nostos-sync/web — proves the package `require()`s in node 22
// and drives the wasm apply engine through the PowerSync-style facade.
//
// REDUCED-SCOPE: does NOT exercise NostosSocket.connect() (the live WS
// transport) — see index.js header. Apply engine only.

"use strict";

const { NostosClient } = require("./index.js");

async function main() {
  const failures = [];
  function check(label, cond) {
    console.log(`${cond ? "✓" : "✗"} ${label}`);
    if (!cond) failures.push(label);
  }

  // --- require(): the package loads, the wasm instantiates ---
  const client = new NostosClient({
    url: "ws://localhost:8080/sync",
    token: "tok",
    table: "tasks",
  });
  check("@nostos-sync/web require()d without crash", typeof client === "object");

  // --- connect(): resolves (no live WS in node; reduced-scope) ---
  await client.connect();
  check("connect() resolved (reduced-scope: no live WS)", true);

  // --- subscribe(): predicate stored on the engine ---
  client.subscribe("tasks", "priority > 5");

  // --- write(): frame fed + flushed, row observable ---
  const w = client.write("tasks", "1", [1, 2, 3]);
  check("write() returned an Outcome", typeof w === "object" && w !== null);
  check("write() checkpoint > 0", w.checkpoint > 0);
  check("write() rowsApplied === 1", w.rowsApplied === 1);

  // --- query(): row read back through the apply engine ---
  const rows = client.query("tasks");
  check("query() returns 1 row", rows.length === 1);
  check("query() pk matches what was written", rows[0] && rows[0].pk === "1");
  check(
    "query() payload matches (Buffer compare)",
    rows[0] && Buffer.compare(rows[0].payload, Buffer.from([1, 2, 3])) === 0
  );

  // --- watch(): snapshot fires immediately (no live stream in node) ---
  let watched = null;
  client.watch("tasks", (snapshot) => {
    watched = snapshot;
  });
  check("watch() fired the snapshot callback", Array.isArray(watched));
  check("watch() snapshot has 1 row", watched && watched.length === 1);

  // --- second write: checkpoint advances, rowCount grows ---
  client.write("tasks", "2", [9, 9]);
  check("rowCount is 2 after second write", client.rowCount === 2);

  // --- typed Tier-1 surface (ADR-0030 / ADR-0032 T3+T4) ---------------------
  // Tag tables, then exercise writeBatch + OR-set + PN-counter + dead-letter
  // visibility. These take the CLIENT-WRITE outbox path (distinct from the
  // Frame-feed write() above = replication path); with no server to ack them
  // they stay pending, which is what pendingCount asserts.
  client.setCrdtTables(["tags"], ["likes"]);

  const ids = client.writeBatch([
    { table: "tasks", op: "upsert", pk: "b1", payloadJson: '{"title":"batch"}' },
  ]);
  check("writeBatch() returns a plain Array", Array.isArray(ids));
  check("writeBatch() returns 1 id", Array.isArray(ids) && ids.length === 1);
  check(
    "writeBatch() id is a non-negative number",
    ids.length === 1 && typeof ids[0] === "number" && ids[0] >= 0
  );

  const orId = client.orSetAdd("tags", "t1", "red");
  check("orSetAdd() returns a write id", typeof orId === "number" && orId >= 0);
  client.orSetRemove("tags", "t1", "red"); // add-wins tombstone; must not throw

  const incId = client.counterIncrement("likes", "l1", 5);
  check("counterIncrement() returns a write id", typeof incId === "number" && incId >= 0);
  client.counterDecrement("likes", "l1", 2); // must not throw

  // 5 client writes above (1 batch op + orSetAdd + orSetRemove + inc + dec),
  // none ack'd (no server in node) → the outbox holds them all.
  check("pendingCount reflects enqueued typed writes", client.pendingCount >= 5);
  check("deadLetteredCount is 0 (no failures)", client.deadLetteredCount === 0);
  // Memory storage returns None → wasm surfaces as undefined (falsy); a real
  // dead-letter error is a non-empty truthy string. So "no failure" = falsy.
  check("lastError is falsy before any failure", !client.lastError);

  console.log("");
  console.log(failures.length === 0 ? "SMOKE_OK" : `SMOKE_FAIL: ${failures.length} check(s) failed`);
  process.exitCode = failures.length === 0 ? 0 : 1;
}

main().catch((err) => {
  console.error("SMOKE_CRASH:", err);
  process.exit(1);
});
