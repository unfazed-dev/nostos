//! `SqliteWasmStorage` — the browser-durable backend (ADR-0017 follow-up / ADR-0033).
//!
//! Implements [`nostos_core::Storage`] AND [`nostos_core::Outbox`] over official
//! SQLite-WASM with the `opfs-sahpool` VFS, mirroring `SqliteStorage`'s schema
//! (`cairn_data`, `cairn_meta`, `cairn_outbox`) and transaction shape exactly.
//!
//! ## Architecture
//!
//! `createSyncAccessHandle` (the sync-OPFS primitive `opfs-sahpool` uses) is
//! Worker-only by spec — so this struct lives in `nostos-ffi-wasm` (NOT
//! `nostos-core`, which stays WASM-clean with no SQLite deps). The struct holds a
//! `js_sys::Object` handle to a JS wrapper around the sqlite-wasm `db` instance.
//! Each `Storage`/`Outbox` method delegates to the JS wrapper's sync methods via
//! `js_sys::Reflect` / `js_sys::Function::call`. The VFS gives synchronous
//! `FileSystemSyncAccessHandle` writes — no `SharedArrayBuffer`/`Atomics`, so
//! NO COOP/COEP cross-origin-isolation is required (ADR-0017 Decision).
//!
//! ## Why mirror, not import
//!
//! `SqliteStorage` (nostos-client) uses `rusqlite`, which is NOT WASM-clean.
//! This module is the mechanical port: same schema, same SQL, same transaction
//! boundaries. The SQL logic lives in Rust (easy to audit against the reference
//! impl); only the raw `exec` / `selectValue` / `selectRows` calls delegate to
//! the JS sqlite-wasm instance.
//!
//! ## Browser-only
//!
//! The JS methods (`db.exec`, `db.selectValue`) exist only when compiled to
//! WASM and run in a Worker with OPFS. Host cargo tests exercise ONLY the
//! `InMemoryStorage` variant (via `WebStorage::Memory`); this path is proven by
//! the Playwright browser test (ADR-0033).

use nostos_core::{Lsn, Outbox, PendingWrite, RowOp, Storage, StorageError, WriteOp};
use std::collections::HashSet;

use js_sys::{Function, Object, Reflect, Uint8Array};
use wasm_bindgen::JsValue;

/// The schema — verbatim mirror of `SqliteStorage::SCHEMA`
/// (`crates/nostos-client/src/sqlite.rs`). Three tables:
/// - `cairn_data` — opaque row payloads keyed by `(table_name, pk)`, with
///   per-row `applied_lsn` for LSN gating (ADR-0025 slice 4a).
/// - `cairn_meta` — key/value (the `checkpoint`, `epoch`).
/// - `cairn_outbox` — durable write queue (ADR-0013) with `attempts`/`dlq`
///   dead-letter columns (ADR-0013 v2 / ADR-0027).
///
/// Used by the JS glue (`sqlite_wasm_glue.js`) — the Rust side delegates via
/// `call_void`. Kept here as the authoritative schema reference.
#[allow(dead_code)]
const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS cairn_data (\
    table_name TEXT NOT NULL,\
    pk TEXT NOT NULL,\
    payload BLOB NOT NULL,\
    applied_lsn INTEGER NOT NULL DEFAULT 0,\
    PRIMARY KEY (table_name, pk)\
);\
CREATE TABLE IF NOT EXISTS cairn_meta (\
    key TEXT PRIMARY KEY,\
    value TEXT NOT NULL\
);\
INSERT OR IGNORE INTO cairn_meta (key, value) VALUES ('checkpoint', '0');\
CREATE TABLE IF NOT EXISTS cairn_outbox (\
    id INTEGER PRIMARY KEY AUTOINCREMENT,\
    table_name TEXT NOT NULL,\
    op TEXT NOT NULL,\
    pk TEXT NOT NULL,\
    payload TEXT,\
    attempts INTEGER NOT NULL DEFAULT 0,\
    dlq INTEGER NOT NULL DEFAULT 0\
);\
";

/// Upsert with per-row LSN gate (live/replay path — `>= applied_lsn`).
/// The actual SQL runs in JS (`sqlite_wasm_glue.js`'s `applyBatch`); this is
/// the auditable reference mirroring `SqliteStorage`.
#[allow(dead_code)]
const SQL_UPSERT_GATED: &str = "\
INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) \
VALUES (?1, ?2, ?3, ?4) \
ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn \
WHERE cairn_data.applied_lsn <= ?4";

/// Upsert unconditional (snapshot-table path — authoritative current-state).
#[allow(dead_code)]
const SQL_UPSERT_UNCOND: &str = "\
INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) \
VALUES (?1, ?2, ?3, ?4) \
ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn";

/// Delete with per-row LSN gate.
#[allow(dead_code)]
const SQL_DELETE_GATED: &str = "\
DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2 AND applied_lsn <= ?3";

/// Delete unconditional (snapshot-table path).
#[allow(dead_code)]
const SQL_DELETE_UNCOND: &str = "\
DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2";

/// The browser-durable storage backend (ADR-0033). Holds a handle to a JS
/// wrapper object around the sqlite-wasm `db` instance (initialized by the
/// Worker with `opfs-sahpool`). Every method calls a JS method synchronously —
/// `opfs-sahpool` provides sync access handles, so no async is needed.
///
/// Created by the Worker (see `nostos.worker.js`) after the async sqlite-wasm
/// init resolves. Passed to Rust via [`crate::NostosSocket::connect`] as the
/// optional 5th arg (the db handle); `connect` wraps it in
/// [`crate::WebStorage::SqliteWasm`].
pub struct SqliteWasmStorage {
    /// The JS wrapper object (instance of `NostosSqliteDb` from
    /// `sqlite_wasm_glue.js`). Methods: `exec(sql, bind?)`,
    /// `selectValue(sql, bind?)`, `selectRows(sql, bind?)`,
    /// `applyBatch(ops, checkpoint, snapshotTables)`, `clearAll()`, `close()`.
    db: Object,
}

impl SqliteWasmStorage {
    /// Wrap a pre-initialized JS db wrapper. The Worker calls this after
    /// async sqlite-wasm init + schema migration. The JS wrapper runs
    /// `initSchema()` in its constructor, so the schema is ready on return.
    #[must_use]
    pub fn new(db: Object) -> Self {
        Self { db }
    }

    // ---- JS-call helpers (uniform via Function::apply) ----

    /// Call a method on the JS db wrapper with zero or more args. Returns the
    /// raw `JsValue` result; callers interpret per-method.
    fn call(&self, method: &str, args: &[JsValue]) -> Result<JsValue, StorageError> {
        let m =
            Reflect::get(&self.db, &JsValue::from_str(method)).map_err(js_err("Reflect.get"))?;
        let f = Function::from(m);
        let arr = js_sys::Array::new();
        for a in args {
            arr.push(a);
        }
        f.apply(&self.db, &arr)
            .map_err(js_err(&format!("db.{method}()")))
    }

    /// Call a void method (discard the result).
    fn call_void(&self, method: &str, args: &[JsValue]) -> Result<(), StorageError> {
        let _ = self.call(method, args)?;
        Ok(())
    }

    /// Run `exec(sql, bind?)` — parameterized or bare SQL, no results.
    fn exec(&self, sql: &str, bind: Option<&js_sys::Array>) -> Result<(), StorageError> {
        let sql_val = JsValue::from_str(sql);
        match bind {
            Some(b) => self.call_void("exec", &[sql_val, b.clone().into()]),
            None => self.call_void("exec", &[sql_val]),
        }
    }

    /// Run `selectValue(sql, bind?)` — returns the first column of the first
    /// row as a `JsValue` (string, number, or null). The JS wrapper converts to
    /// string; Rust parses.
    fn select_value_str(&self, sql: &str, bind: Option<&js_sys::Array>) -> Option<String> {
        let sql_val = JsValue::from_str(sql);
        let result = match bind {
            Some(b) => self
                .call("selectValue", &[sql_val, b.clone().into()])
                .ok()?,
            None => self.call("selectValue", &[sql_val]).ok()?,
        };
        // The JS wrapper returns a string or null. `as_string` extracts.
        if result.is_null() || result.is_undefined() {
            return None;
        }
        result.as_string()
    }

    /// Run `selectRows(sql, bind?)` — returns a JS array of arrays (rowMode:
    /// "array"). Each inner array is the row's column values in order.
    fn select_rows(
        &self,
        sql: &str,
        bind: Option<&js_sys::Array>,
    ) -> Result<js_sys::Array, StorageError> {
        let sql_val = JsValue::from_str(sql);
        let result = match bind {
            Some(b) => self.call("selectRows", &[sql_val, b.clone().into()])?,
            None => self.call("selectRows", &[sql_val])?,
        };
        Ok(js_sys::Array::from(&result))
    }

    /// Read the durable checkpoint from `cairn_meta`. Returns 0 on a fresh DB.
    fn read_checkpoint(&self) -> Lsn {
        self.select_value_str(
            "SELECT value FROM cairn_meta WHERE key = 'checkpoint'",
            None,
        )
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(Lsn::ZERO, Lsn::new)
    }

    /// Count rows in `cairn_data` (diagnostics — mirrors `SqliteStorage::row_count_for_test`).
    #[allow(dead_code)] // surfaced via WebStorage::row_count
    pub fn row_count(&self) -> usize {
        self.select_value_str("SELECT COUNT(*) FROM cairn_data", None)
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }

    /// Enumerate `(pk, payload)` pairs for `table`, sorted by pk (mirrors
    /// `SqliteStorage::rows_for` — the readback the FFI surfaces to JS).
    #[allow(dead_code)] // surfaced via WebStorage::rows_for
    pub fn rows_for(&self, table: &str) -> Vec<(String, Vec<u8>)> {
        let sql = "SELECT pk, payload FROM cairn_data WHERE table_name = ?1 ORDER BY pk ASC";
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(table));
        let Ok(rows) = self.select_rows(sql, Some(&bind)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for i in 0..rows.length() {
            let row = js_sys::Array::from(&rows.get(i));
            let pk = row.get(0).as_string().unwrap_or_default();
            let payload_val = row.get(1);
            let payload = if payload_val.is_string() {
                // TEXT fallback (shouldn't happen for BLOB, but be defensive)
                payload_val.as_string().unwrap_or_default().into_bytes()
            } else {
                // Uint8Array (the normal path for BLOB columns)
                Uint8Array::new(&payload_val).to_vec()
            };
            out.push((pk, payload));
        }
        out
    }

    /// Close the underlying sqlite-wasm db handle (for sign-out file removal).
    pub fn close(&self) {
        let _ = self.call_void("close", &[]);
    }
}

// ---- Storage impl ----

impl Storage for SqliteWasmStorage {
    fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        Ok(self.read_checkpoint())
    }

    fn epoch(&self) -> nostos_core::Result<u64> {
        Ok(self
            .select_value_str("SELECT value FROM cairn_meta WHERE key = 'epoch'", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0))
    }

    fn save_epoch(&self, epoch: u64) -> nostos_core::Result<()> {
        // INSERT OR IGNORE ensures the key exists, then UPDATE sets the value.
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(&epoch.to_string()));
        bind.push(&JsValue::from_str("epoch"));
        self.exec(
            "INSERT INTO cairn_meta (key, value) VALUES ('epoch', '0') ON CONFLICT(key) DO UPDATE SET value = ?1",
            Some(&bind),
        )?;
        Ok(())
    }

    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &HashSet<String>,
    ) -> nostos_core::Result<()> {
        // Build the ops as a JS array — one boundary crossing for the whole
        // transaction. Each op is {kind, table, pk, payload?: Uint8Array, lsn}.
        // The JS wrapper runs BEGIN → per-op SQL → checkpoint UPDATE → COMMIT.
        let js_ops = js_sys::Array::new();
        for (op, lsn) in ops {
            let entry = Object::new();
            let (kind, has_payload) = match op {
                RowOp::Insert { .. } => ("insert", true),
                RowOp::Update { .. } => ("update", true),
                RowOp::Delete { .. } => ("delete", false),
            };
            let _ = Reflect::set(&entry, &"kind".into(), &JsValue::from_str(kind));
            let (table, pk) = match op {
                RowOp::Insert { table, pk, .. }
                | RowOp::Update { table, pk, .. }
                | RowOp::Delete { table, pk, .. } => (table.as_str(), pk.as_str()),
            };
            let _ = Reflect::set(&entry, &"table".into(), &JsValue::from_str(table));
            let _ = Reflect::set(&entry, &"pk".into(), &JsValue::from_str(pk));
            if has_payload {
                if let RowOp::Insert { payload, .. } | RowOp::Update { payload, .. } = op {
                    let arr = Uint8Array::from(payload.as_ref());
                    let _ = Reflect::set(&entry, &"payload".into(), &arr.into());
                }
            }
            let _ = Reflect::set(
                &entry,
                &"lsn".into(),
                &JsValue::from_f64(i64::try_from(*lsn).unwrap_or(i64::MAX) as f64),
            );
            js_ops.push(&entry);
        }
        let snap = js_sys::Array::new();
        for t in snapshot_tables {
            snap.push(&JsValue::from_str(t));
        }
        let checkpoint_val = JsValue::from_str(&checkpoint.raw().to_string());
        // The JS applyBatch runs the entire transaction; a throw → ROLLBACK +
        // Err. The per-row LSN gate and snapshot-table unconditional paths are
        // decided in JS (SQL WHERE clauses), mirroring SqliteStorage.
        self.call_void("applyBatch", &[js_ops.into(), checkpoint_val, snap.into()])?;
        Ok(())
    }

    fn pks_for_table(&self, table: &str) -> nostos_core::Result<Vec<String>> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(table));
        let rows = self.select_rows(
            "SELECT pk FROM cairn_data WHERE table_name = ?1 ORDER BY pk ASC",
            Some(&bind),
        )?;
        let mut pks = Vec::new();
        for i in 0..rows.length() {
            let row = js_sys::Array::from(&rows.get(i));
            pks.push(row.get(0).as_string().unwrap_or_default());
        }
        Ok(pks)
    }

    fn delete_pks(&mut self, table: &str, pks: &[String]) -> nostos_core::Result<()> {
        // Build a parameterized DELETE for each pk in one pass. SQLite parameter
        // limits are high (>999), and the orphan-reap set is small (one table's
        // worth of PKs), so this is safe. Auto-commits per call (the trait
        // permits this for `delete_pks`).
        for pk in pks {
            let bind = js_sys::Array::new();
            bind.push(&JsValue::from_str(table));
            bind.push(&JsValue::from_str(pk));
            self.exec(
                "DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2",
                Some(&bind),
            )?;
        }
        Ok(())
    }

    fn clear(&mut self) -> nostos_core::Result<()> {
        // ADR-0029: reset to fresh-database state. `clearAll()` runs:
        //   DELETE FROM cairn_data; DELETE FROM cairn_outbox;
        //   UPDATE cairn_meta SET value='0' WHERE key='checkpoint';
        // The checkpoint → 0 is load-bearing (resume-without-snapshot guard).
        self.call_void("clearAll", &[])?;
        Ok(())
    }
}

// ---- Outbox impl ----

impl Outbox for SqliteWasmStorage {
    fn enqueue(&mut self, write: PendingWrite) -> nostos_core::Result<u64> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(&write.table));
        bind.push(&JsValue::from_str(write.op.as_wire_str()));
        bind.push(&JsValue::from_str(&write.pk));
        match &write.payload_json {
            Some(p) => {
                bind.push(&JsValue::from_str(p));
            }
            None => {
                bind.push(&JsValue::NULL);
            }
        }
        self.exec(
            "INSERT INTO cairn_outbox (table_name, op, pk, payload) VALUES (?1, ?2, ?3, ?4)",
            Some(&bind),
        )?;
        // Read the AUTOINCREMENT id. `selectValue` returns the last rowid.
        let id = self
            .select_value_str("SELECT last_insert_rowid()", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Ok(id)
    }

    fn pending(&self) -> nostos_core::Result<Vec<(u64, PendingWrite)>> {
        let rows = self.select_rows(
            "SELECT id, table_name, op, pk, payload FROM cairn_outbox WHERE dlq = 0 ORDER BY id ASC",
            None,
        )?;
        let mut out = Vec::new();
        for i in 0..rows.length() {
            let row = js_sys::Array::from(&rows.get(i));
            let id = row
                .get(0)
                .as_string()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let table = row.get(1).as_string().unwrap_or_default();
            let op_wire = row.get(2).as_string().unwrap_or_default();
            let pk = row.get(3).as_string().unwrap_or_default();
            let payload_val = row.get(4);
            let payload_json = if payload_val.is_null() || payload_val.is_undefined() {
                None
            } else {
                payload_val.as_string()
            };
            let op = WriteOp::from_wire_str(&op_wire).unwrap_or(WriteOp::Upsert);
            out.push((
                id,
                PendingWrite {
                    table,
                    op,
                    pk,
                    payload_json,
                },
            ));
        }
        Ok(out)
    }

    fn mark_done(&mut self, id: u64) -> nostos_core::Result<()> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        self.exec("DELETE FROM cairn_outbox WHERE id = ?1", Some(&bind))?;
        Ok(())
    }

    fn bump_attempts(&self, id: u64) -> nostos_core::Result<u32> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        self.exec(
            "UPDATE cairn_outbox SET attempts = attempts + 1 WHERE id = ?1 AND dlq = 0",
            Some(&bind),
        )?;
        let bind2 = js_sys::Array::new();
        bind2.push(&JsValue::from_f64(id as f64));
        Ok(self
            .select_value_str(
                "SELECT attempts FROM cairn_outbox WHERE id = ?1",
                Some(&bind2),
            )
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0))
    }

    fn mark_dead_letter(&self, id: u64) -> nostos_core::Result<()> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        self.exec("UPDATE cairn_outbox SET dlq = 1 WHERE id = ?1", Some(&bind))?;
        Ok(())
    }

    fn apply_local(&mut self, write: &PendingWrite) -> nostos_core::Result<()> {
        // Optimistic instant-local write: store the row NOW (applied_lsn = 0 so
        // the first server frame at LSN > 0 overwrites it). Deletes are a no-op
        // here (the server echo removes the row); mirroring SqliteStorage.
        if let Some(payload) = &write.payload_json {
            let bind = js_sys::Array::new();
            bind.push(&JsValue::from_str(&write.table));
            bind.push(&JsValue::from_str(&write.pk));
            bind.push(&Uint8Array::from(payload.as_bytes()).into());
            self.exec(
                "INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) \
                 VALUES (?1, ?2, ?3, 0) \
                 ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload",
                Some(&bind),
            )?;
        }
        Ok(())
    }

    fn clear(&mut self) -> nostos_core::Result<()> {
        // Wipe the outbox + dead-letter queue (ADR-0029). Storage::clear wipes
        // rows + checkpoint; this is the outbox-only half.
        self.exec("DELETE FROM cairn_outbox", None)?;
        Ok(())
    }
}

// ---- helpers ----

/// Build a closure that maps a `JsValue` error to `StorageError::Backend`.
fn js_err(ctx: &str) -> impl Fn(JsValue) -> StorageError + '_ {
    move |e| StorageError::Backend(format!("SqliteWasm {ctx}: {e:?}"))
}
