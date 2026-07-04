// Node smoke test for nostos-ffi-wasm — proves the JS↔Rust boundary end-to-end.
// Loads the wasm-pack output, drives the apply engine, asserts the contract.
import { NostosEngine, Frame } from "./pkg/nostos_ffi_wasm.js";

let failures = 0;
function check(label, cond) {
  console.log(`${cond ? "✓" : "✗"} ${label}`);
  if (!cond) failures++;
}

// --- fresh engine: zero checkpoint, zero rows ---
const eng = new NostosEngine();
check("fresh checkpoint is 0", eng.checkpoint === 0);
check("fresh row_count is 0", eng.rowCount === 0);

// --- feed a frame: buffered (no commit yet), so checkpoint stays 0 ---
const r1 = eng.feed(new Frame(10, "insert", "tasks", "1", [1, 2, 3], null));
check("feed (buffered) returns undefined", r1 === undefined);
check("checkpoint still 0 before flush", eng.checkpoint === 0);

// --- flush: atomic commit advances the checkpoint ---
const flush1 = eng.flush();
check("flush returns an Outcome", typeof flush1 === "object" && flush1 !== null);
check("flush checkpoint = 10", flush1.checkpoint === 10);
check("flush rows_applied = 1", flush1.rowsApplied === 1);
check("engine checkpoint now 10", eng.checkpoint === 10);
check("engine row_count now 1", eng.rowCount === 1);

// --- idempotency: re-apply the same frame → no duplicate row ---
eng.feed(new Frame(10, "insert", "tasks", "1", [1, 2, 3], null));
eng.flush();
check("idempotent re-apply: row_count still 1", eng.rowCount === 1);
check("checkpoint unchanged by stale re-apply", eng.checkpoint === 10);

// --- more frames advance the checkpoint ---
eng.feed(new Frame(20, "insert", "tasks", "2", [9, 9], null));
eng.feed(new Frame(30, "insert", "tasks", "3", [7], null));
eng.flush();
check("checkpoint advanced to 30", eng.checkpoint === 30);
check("row_count is 3", eng.rowCount === 3);

// --- delete removes a row ---
eng.feed(new Frame(40, "delete", "tasks", "2", null, null));
eng.flush();
check("delete dropped row_count to 2", eng.rowCount === 2);
check("checkpoint advanced to 40", eng.checkpoint === 40);

// --- txn-batched frames commit together at the boundary ---
eng.feed(new Frame(50, "insert", "tasks", "a", [0], 100)); // txn 100
eng.feed(new Frame(51, "insert", "tasks", "b", [0], 100)); // txn 100 (buffered)
const r3 = eng.feed(new Frame(52, "insert", "tasks", "c", [0], null)); // closes txn → flush
check("txn boundary fires a commit", typeof r3 === "object" && r3.rowsApplied === 2);

// --- where_sql: the engine stores the predicate for the transport (E1) ---
// The apply engine does NOT evaluate where_sql (the server filters upstream);
// it just holds the string so E1's transport can attach it to the subscribe
// frame. Getter/setter round-trip + null-clear are the contract.
check("fresh whereSql is null", eng.whereSql === null);
eng.setWhereSql("priority > 5");
check("setWhereSql stores the predicate", eng.whereSql === "priority > 5");
eng.setWhereSql(null);
check("setWhereSql(null) clears it", eng.whereSql === null);
eng.setWhereSql("status = open AND priority >= 3");
check("complex predicate round-trips", eng.whereSql === "status = open AND priority >= 3");

console.log(`\n${failures === 0 ? "ALL PASS" : failures + " FAILED"}`);
process.exit(failures === 0 ? 0 : 1);
