// sqlite_wasm_glue.js — JS wrapper around @sqlite.org/sqlite-wasm for nostos.
//
// ADR-0033: the browser-durable backend. The Rust `SqliteWasmStorage` (in
// nostos-ffi-wasm) holds a `js_sys::Object` handle to the wrapper returned by
// `openNostosDb()` and delegates every Storage/Outbox method to it via
// `js_sys::Reflect` + `Function::apply`. The `opfs-sahpool` VFS gives
// synchronous FileSystemSyncAccessHandle writes — no SharedArrayBuffer, no
// COOP/COEP, no async needed at the Rust↔JS boundary.
//
// This module is browser-Worker-only: it imports `@sqlite.org/sqlite-wasm`,
// which requires OPFS sync handles (Worker-only by spec). Node smoke tests
// never load this file — they use InMemoryStorage via NostosEngine::new().
//
// The wrapper exposes exactly the methods Rust calls:
//   exec(sql, bind?)             — parameterized or bare SQL, no results
//   selectValue(sql, bind?)      — first column of first row as string|null
//   selectRows(sql, bind?)       — array of arrays (rowMode: "array")
//   selectObjects(sql, bind?)    — array of column-named objects (query_json)
//   applyBatch(ops, checkpoint, snapshotTables) — one transaction
//   clearAll()                   — sign-out wipe (rows + outbox + checkpoint=0)
//   close()                      — close the db handle

// The schema mirrors SqliteStorage::SCHEMA verbatim
// (crates/nostos-client/src/sqlite.rs). Three tables: nostos_data (row payloads +
// per-row applied_lsn), nostos_meta (checkpoint/epoch), nostos_outbox (durable
// write queue with attempts/dlq dead-letter columns — ADR-0013 v2 / ADR-0027).
const SCHEMA_SQL = `
CREATE TABLE IF NOT EXISTS nostos_data (
    table_name TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload BLOB NOT NULL,
    applied_lsn INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (table_name, pk)
);
CREATE TABLE IF NOT EXISTS nostos_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT OR IGNORE INTO nostos_meta (key, value) VALUES ('checkpoint', '0');
CREATE TABLE IF NOT EXISTS nostos_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    table_name TEXT NOT NULL,
    op TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    dlq INTEGER NOT NULL DEFAULT 0
);
`;

// Upsert with per-row LSN gate (live/replay path).
const SQL_UPSERT_GATED =
  "INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) " +
  "VALUES (?, ?, ?, ?) " +
  "ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn " +
  "WHERE nostos_data.applied_lsn <= ?";

// Upsert unconditional (snapshot-table path — authoritative current-state).
const SQL_UPSERT_UNCOND =
  "INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) " +
  "VALUES (?, ?, ?, ?) " +
  "ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn";

// Delete with per-row LSN gate.
const SQL_DELETE_GATED =
  "DELETE FROM nostos_data WHERE table_name = ? AND pk = ? AND applied_lsn <= ?";

// Delete unconditional (snapshot-table path).
const SQL_DELETE_UNCOND =
  "DELETE FROM nostos_data WHERE table_name = ? AND pk = ?";

/**
 * Async-init sqlite-wasm with opfs-sahpool and return the wrapper object.
 *
 * @returns {Promise<object>} the JS wrapper the Rust SqliteWasmStorage delegates to.
 * @throws {Error} if OPFS is unavailable, sqlite-wasm fails to init, or the
 *   schema migration fails. The caller (nostos.worker.js) catches and degrades
 *   to InMemoryStorage.
 */
export async function openNostosDb() {
  // Dynamic import so the Worker can boot even if the package isn't installed
  // (Node smoke path never reaches here). The path resolves relative to this
  // module's URL inside the Worker. The package's default export is
  // `sqlite3InitModule` (the low-level init function).
  let mod;
  try {
    // Flutter serves its own pinned copy; OPFS must also boot with the network
    // offline after the app shell has been cached.
    mod = await import("./sqlite-wasm/index.mjs");
  } catch (_) {
    // The SDK's standalone Worker smoke serves npm dependencies separately.
    mod = await import(
      /* @vite-ignore */ /* webpackIgnore: true */
      resolveSqliteWasmPath()
    );
  }
  const sqlite3 = await mod.default({
    print: () => {},
    printErr: () => {},
  });

  // Install the opfs-sahpool VFS explicitly. This is the synchronous
  // FileSystemSyncAccessHandle-based VFS (ADR-0017 Decision: option 1).
  // It requires Worker context + OPFS support — on Safari Private Browsing /
  // old browsers, this throws, and the caller degrades to InMemoryStorage.
  //
  // NOTE: OpfsDb (the convenience class) uses a DIFFERENT async OPFS VFS.
  // The sahpool VFS must be installed explicitly via installOpfsSAHPoolVfs(),
  // then a regular oo1.DB is opened with vfs=opfs-sahpool.
  if (typeof sqlite3.installOpfsSAHPoolVfs !== "function") {
    throw new Error(
      "sqlite3.installOpfsSAHPoolVfs unavailable (sqlite-wasm build too old?)",
    );
  }
  const pool = await sqlite3.installOpfsSAHPoolVfs();

  // Open the DB with the sahpool VFS. The filename is an OPFS path
  // (relative to the origin's OPFS root). ADR-0048: an origin holding only the
  // pre-rename file keeps using it, so its rows and unsent writes stay.
  const names = pool.getFileNames();
  const file =
    names.includes("/cairn.sqlite") && !names.includes("/nostos.sqlite") // rename:hold
      ? "cairn.sqlite" // rename:hold
      : "nostos.sqlite";
  const db = new sqlite3.oo1.DB(`file:${file}?vfs=opfs-sahpool`);

  // Pre-rename tables are renamed in place first (ADR-0048), so the schema
  // migration (idempotent — CREATE TABLE IF NOT EXISTS) finds them.
  for (const t of ["data", "meta", "outbox"]) {
    const legacy = `cairn_${t}`; // rename:hold
    if (db.selectValue("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", legacy)) {
      db.exec(`ALTER TABLE ${legacy} RENAME TO nostos_${t}`);
    }
  }
  db.exec(SCHEMA_SQL);

  // The Appwrite Function deduplicates mutations by an opaque ID derived from
  // this device ID and the durable AUTOINCREMENT outbox ID. Keep the device ID
  // across sign-out; clearAll() intentionally leaves nostos_meta's device_id.
  let deviceId = db.selectValue(
    "SELECT value FROM nostos_meta WHERE key = 'device_id'",
  );
  if (!deviceId) {
    deviceId = crypto.randomUUID();
    db.exec({
      sql: "INSERT OR IGNORE INTO nostos_meta (key, value) VALUES ('device_id', ?)",
      bind: [deviceId],
    });
    deviceId = db.selectValue(
      "SELECT value FROM nostos_meta WHERE key = 'device_id'",
    );
  }

  return makeWrapper(db, deviceId);
}

/**
 * Resolve the import path for @sqlite.org/sqlite-wasm relative to this module.
 * In the static-HTTP test server, the module is served at
 * `/worker/sqlite_wasm_glue.js`, so the package is at
 * `/node_modules/@sqlite.org/sqlite-wasm/dist/index.mjs`.
 */
function resolveSqliteWasmPath() {
  // import.meta.url is the full URL of this module inside the Worker.
  // Go up one level (worker/ -> sdk/nostos_web/) then into node_modules.
  const base = new URL(".", import.meta.url);
  return new URL(
    "../node_modules/@sqlite.org/sqlite-wasm/dist/index.mjs",
    base,
  ).href;
}

/**
 * Build the wrapper object around the sqlite-wasm db instance.
 * @param {object} db — the sqlite3.oo1.OpfsDb instance.
 * @returns {object} the wrapper with exec/selectValue/selectRows/applyBatch/clearAll/close.
 */
function makeWrapper(db, deviceId) {
  return {
    deviceId,
    exec(sql, bind) {
      if (bind && bind.length > 0) {
        db.exec({ sql, bind });
      } else {
        db.exec(sql);
      }
    },

    selectValue(sql, bind) {
      let result;
      if (bind && bind.length > 0) {
        result = db.selectValue(sql, bind);
      } else {
        result = db.selectValue(sql);
      }
      if (result === undefined || result === null) {
        return null;
      }
      return String(result);
    },

    selectRows(sql, bind) {
      let result;
      if (bind && bind.length > 0) {
        result = db.selectArrays(sql, bind);
      } else {
        result = db.selectArrays(sql);
      }
      return result || [];
    },

    // Column-named rows ([{col: val}, ...]) — the shape Rust's query_json
    // prefers and the SDK's watchQuery decodes (array-of-arrays from
    // selectRows makes Dart's .cast<Map>() produce nothing/throw).
    selectObjects(sql, bind) {
      let result;
      if (bind && bind.length > 0) {
        result = db.selectObjects(sql, bind);
      } else {
        result = db.selectObjects(sql);
      }
      return result || [];
    },

    applyBatch(ops, checkpoint, snapshotTables) {
      const snap = new Set(snapshotTables || []);
      db.exec("BEGIN");
      try {
        for (const op of ops) {
          const isSnap = snap.has(op.table);
          if (op.kind === "delete") {
            const sql = isSnap ? SQL_DELETE_UNCOND : SQL_DELETE_GATED;
            const bind = isSnap
              ? [op.table, op.pk]
              : [op.table, op.pk, op.lsn];
            db.exec({ sql, bind });
          } else {
            const sql = isSnap ? SQL_UPSERT_UNCOND : SQL_UPSERT_GATED;
            const bind = isSnap
              ? [op.table, op.pk, op.payload, op.lsn]
              : [op.table, op.pk, op.payload, op.lsn, op.lsn];
            db.exec({ sql, bind });
          }
        }
        db.exec({
          sql: "UPDATE nostos_meta SET value = ? WHERE key = 'checkpoint'",
          bind: [String(checkpoint)],
        });
        db.exec("COMMIT");
      } catch (e) {
        try {
          db.exec("ROLLBACK");
        } catch (_) {
          /* already rolled back or txn not open */
        }
        throw e;
      }
    },

    clearAll() {
      db.exec("BEGIN");
      try {
        db.exec("DELETE FROM nostos_data");
        db.exec("DELETE FROM nostos_outbox");
        db.exec("UPDATE nostos_meta SET value = '0' WHERE key = 'checkpoint'");
        db.exec("DELETE FROM nostos_meta WHERE key IN ('horizon', 'principal')");
        db.exec("COMMIT");
      } catch (error) {
        try { db.exec("ROLLBACK"); } catch (_) {}
        throw error;
      }
    },

    close() {
      try {
        db.close();
      } catch (_) {
        /* already closed */
      }
    },
  };
}
