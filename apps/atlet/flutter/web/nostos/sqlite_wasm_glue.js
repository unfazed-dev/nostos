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
// (crates/nostos-client/src/sqlite.rs). Three tables: cairn_data (row payloads +
// per-row applied_lsn), cairn_meta (checkpoint/epoch), cairn_outbox (durable
// write queue with attempts/dlq dead-letter columns — ADR-0013 v2 / ADR-0027).
const SCHEMA_SQL = `
CREATE TABLE IF NOT EXISTS cairn_data (
    table_name TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload BLOB NOT NULL,
    applied_lsn INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (table_name, pk)
);
CREATE TABLE IF NOT EXISTS cairn_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT OR IGNORE INTO cairn_meta (key, value) VALUES ('checkpoint', '0');
CREATE TABLE IF NOT EXISTS cairn_outbox (
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
  "INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) " +
  "VALUES (?, ?, ?, ?) " +
  "ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn " +
  "WHERE cairn_data.applied_lsn <= ?";

// Upsert unconditional (snapshot-table path — authoritative current-state).
const SQL_UPSERT_UNCOND =
  "INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) " +
  "VALUES (?, ?, ?, ?) " +
  "ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn";

// Delete with per-row LSN gate.
const SQL_DELETE_GATED =
  "DELETE FROM cairn_data WHERE table_name = ? AND pk = ? AND applied_lsn <= ?";

// Delete unconditional (snapshot-table path).
const SQL_DELETE_UNCOND =
  "DELETE FROM cairn_data WHERE table_name = ? AND pk = ?";

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
  const mod = await import(
    /* @vite-ignore */ /* webpackIgnore: true */
    resolveSqliteWasmPath()
  );
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
  await sqlite3.installOpfsSAHPoolVfs();

  // Open the DB with the sahpool VFS. The filename is an OPFS path
  // (relative to the origin's OPFS root).
  const db = new sqlite3.oo1.DB("file:cairn.sqlite?vfs=opfs-sahpool");

  // Run the schema migration (idempotent — CREATE TABLE IF NOT EXISTS).
  db.exec(SCHEMA_SQL);

  return makeWrapper(db);
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
function makeWrapper(db) {
  return {
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
          sql: "UPDATE cairn_meta SET value = ? WHERE key = 'checkpoint'",
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
      db.exec("DELETE FROM cairn_data");
      db.exec("DELETE FROM cairn_outbox");
      db.exec("UPDATE cairn_meta SET value = '0' WHERE key = 'checkpoint'");
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
