//! `SqliteWasmStorage` — the browser-durable backend (ADR-0017 follow-up / ADR-0033).
//!
//! Implements [`nostos_core::Storage`] AND [`nostos_core::Outbox`] over official
//! SQLite-WASM with the `opfs-sahpool` VFS, mirroring `SqliteStorage`'s schema
//! (`nostos_data`, `nostos_meta`, `nostos_outbox`) and transaction shape exactly.
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
/// - `nostos_data` — opaque row payloads keyed by `(table_name, pk)`, with
///   per-row `applied_lsn` for LSN gating (ADR-0025 slice 4a).
/// - `nostos_meta` — key/value (the `checkpoint`, `epoch`).
/// - `nostos_outbox` — durable write queue (ADR-0013) with `attempts`/`dlq`
///   dead-letter columns (ADR-0013 v2 / ADR-0027).
///
/// Used by the JS glue (`sqlite_wasm_glue.js`) — the Rust side delegates via
/// `call_void`. Kept here as the authoritative schema reference.
#[allow(dead_code)]
const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS nostos_data (\
    table_name TEXT NOT NULL,\
    pk TEXT NOT NULL,\
    payload BLOB NOT NULL,\
    applied_lsn INTEGER NOT NULL DEFAULT 0,\
    PRIMARY KEY (table_name, pk)\
);\
CREATE TABLE IF NOT EXISTS nostos_meta (\
    key TEXT PRIMARY KEY,\
    value TEXT NOT NULL\
);\
INSERT OR IGNORE INTO nostos_meta (key, value) VALUES ('checkpoint', '0');\
CREATE TABLE IF NOT EXISTS nostos_outbox (\
    id INTEGER PRIMARY KEY AUTOINCREMENT,\
    table_name TEXT NOT NULL,\
    op TEXT NOT NULL,\
    pk TEXT NOT NULL,\
    payload TEXT,\
    attempts INTEGER NOT NULL DEFAULT 0,\
    dlq INTEGER NOT NULL DEFAULT 0,\
    last_error TEXT,\
    dead_lettered_at INTEGER\
);\
";

/// Upsert with per-row LSN gate (live/replay path — `>= applied_lsn`).
/// The actual SQL runs in JS (`sqlite_wasm_glue.js`'s `applyBatch`); this is
/// the auditable reference mirroring `SqliteStorage`.
#[allow(dead_code)]
const SQL_UPSERT_GATED: &str = "\
INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) \
VALUES (?1, ?2, ?3, ?4) \
ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn \
WHERE nostos_data.applied_lsn <= ?4";

/// Upsert unconditional (snapshot-table path — authoritative current-state).
#[allow(dead_code)]
const SQL_UPSERT_UNCOND: &str = "\
INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) \
VALUES (?1, ?2, ?3, ?4) \
ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload, applied_lsn = excluded.applied_lsn";

/// Delete with per-row LSN gate.
#[allow(dead_code)]
const SQL_DELETE_GATED: &str = "\
DELETE FROM nostos_data WHERE table_name = ?1 AND pk = ?2 AND applied_lsn <= ?3";

/// Delete unconditional (snapshot-table path).
#[allow(dead_code)]
const SQL_DELETE_UNCOND: &str = "\
DELETE FROM nostos_data WHERE table_name = ?1 AND pk = ?2";

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
    /// Tables whose payload is an add-wins OR-set (ADR-0030): applies MERGE
    /// element-wise by HLC instead of clobbering. Empty by default. Mirrors
    /// `SqliteStorage::or_set_tables` (native L117).
    or_set_tables: HashSet<String>,
    /// Tables whose payload is a PN-Counter CRDT (ADR-0030 addendum): applies
    /// MERGE per-replica element-wise max. Mirrors `SqliteStorage::counter_tables`
    /// (native L121).
    counter_tables: HashSet<String>,
}

impl SqliteWasmStorage {
    /// Wrap a pre-initialized JS db wrapper. The Worker calls this after
    /// async sqlite-wasm init + schema migration. The JS wrapper runs
    /// `initSchema()` in its constructor, so the schema is ready on return.
    ///
    /// Runs the v3 dead-letter migration (`last_error` + `dead_lettered_at`)
    /// best-effort — a failure degrades to the old `dlq`-only path (correct,
    /// just missing the error columns for inspection). Mirrors native
    /// `SqliteStorage::migrate_outbox_dlq` (sqlite.rs L217).
    #[must_use]
    pub fn new(db: Object) -> Self {
        let storage = Self {
            db,
            or_set_tables: HashSet::new(),
            counter_tables: HashSet::new(),
        };
        // Best-effort migration: if the DB predates ADR-0027 v3, ALTER the
        // columns in. A fresh DB already has them (SCHEMA_SQL). Failure is
        // non-fatal — mark_dead_letter_with_error falls back to dlq-only.
        let _ = storage.migrate_outbox_dlq();
        storage
    }

    /// Builder: tag tables whose payload is an add-wins OR-set (ADR-0030).
    /// Mirrors `SqliteStorage::with_or_set_tables` (native L150).
    #[must_use]
    pub fn with_or_set_tables(mut self, tables: HashSet<String>) -> Self {
        self.or_set_tables = tables;
        self
    }

    /// Builder: tag tables whose payload is a PN-Counter CRDT (ADR-0030
    /// addendum). Mirrors `SqliteStorage::with_counter_tables` (native L161).
    /// A table MUST NOT be in both `or_set_tables` and `counter_tables`
    /// (counter wins the first branch checked in `apply_local`).
    #[must_use]
    pub fn with_counter_tables(mut self, tables: HashSet<String>) -> Self {
        self.counter_tables = tables;
        self
    }

    /// Set the OR-set tables post-construction (Wave 4a).
    pub(crate) fn set_or_set_tables(&mut self, tables: HashSet<String>) {
        self.or_set_tables = tables;
    }

    /// Set the counter tables post-construction (Wave 4a).
    pub(crate) fn set_counter_tables(&mut self, tables: HashSet<String>) {
        self.counter_tables = tables;
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
    /// row as a `JsValue` (string, number, or null). SQLite-WASM returns numeric
    /// columns as JS numbers, so convert safe integers explicitly.
    fn select_value_str(&self, sql: &str, bind: Option<&js_sys::Array>) -> Option<String> {
        let sql_val = JsValue::from_str(sql);
        let result = match bind {
            Some(b) => self
                .call("selectValue", &[sql_val, b.clone().into()])
                .ok()?,
            None => self.call("selectValue", &[sql_val]).ok()?,
        };
        if result.is_null() || result.is_undefined() {
            return None;
        }
        result.as_string().or_else(|| {
            result
                .as_f64()
                .filter(|value| {
                    value.is_finite()
                        && value.fract() == 0.0
                        && *value >= 0.0
                        && *value <= 9_007_199_254_740_991.0
                })
                .map(|value| format!("{value:.0}"))
        })
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

    /// Read the durable checkpoint from `nostos_meta`. Returns 0 on a fresh DB.
    fn read_checkpoint(&self) -> Lsn {
        self.select_value_str(
            "SELECT value FROM nostos_meta WHERE key = 'checkpoint'",
            None,
        )
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(Lsn::ZERO, Lsn::new)
    }

    /// Count rows in `nostos_data` (diagnostics — mirrors `SqliteStorage::row_count_for_test`).
    #[allow(dead_code)] // surfaced via WebStorage::row_count
    pub fn row_count(&self) -> usize {
        self.select_value_str("SELECT COUNT(*) FROM nostos_data", None)
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }

    /// Enumerate `(pk, payload)` pairs for `table`, sorted by pk (mirrors
    /// `SqliteStorage::rows_for` — the readback the FFI surfaces to JS).
    #[allow(dead_code)] // surfaced via WebStorage::rows_for
    pub fn rows_for(&self, table: &str) -> Vec<(String, Vec<u8>)> {
        let sql = "SELECT pk, payload FROM nostos_data WHERE table_name = ?1 ORDER BY pk ASC";
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

    // ---- ADR-0027 v3: dead-letter columns migration + read/engine primitives ----

    /// Check whether `nostos_outbox` has a column named `col`. Mirrors native
    /// `outbox_has_column` (sqlite.rs L201): `PRAGMA table_info` → scan names.
    fn outbox_has_column(&self, col: &str) -> bool {
        // PRAGMA table_info returns rows of (cid, name, type, notnull, dflt, pk).
        // We only need the `name` column (index 1). `selectRows` in array mode.
        self.select_rows("PRAGMA table_info(nostos_outbox)", None)
            .is_ok_and(|rows| {
                (0..rows.length()).any(|i| {
                    let row = js_sys::Array::from(&rows.get(i));
                    row.get(1).as_string().is_some_and(|n| n == col)
                })
            })
    }

    /// v3 migration: add `last_error TEXT` and `dead_lettered_at INTEGER` to
    /// `nostos_outbox` (ADR-0027 / ADR-0032 T5). Idempotent — checks
    /// `PRAGMA table_info` first, ALTERs only if missing. Mirrors native
    /// `SqliteStorage::migrate_outbox_dlq` (sqlite.rs L217-233).
    fn migrate_outbox_dlq(&self) -> Result<(), StorageError> {
        if !self.outbox_has_column("last_error") {
            self.exec("ALTER TABLE nostos_outbox ADD COLUMN last_error TEXT", None)?;
        }
        if !self.outbox_has_column("dead_lettered_at") {
            self.exec(
                "ALTER TABLE nostos_outbox ADD COLUMN dead_lettered_at INTEGER",
                None,
            )?;
        }
        Ok(())
    }

    /// Read the raw payload bytes for `(table, pk)`, or `None` if the row is
    /// absent. Used by the PN-Counter CRDT's read-modify-write (ADR-0030
    /// addendum). Mirrors native `SqliteStorage::read_payload` (sqlite.rs
    /// L889-903).
    pub fn read_payload(&self, table: &str, pk: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(table));
        bind.push(&JsValue::from_str(pk));
        match self.select_rows(
            "SELECT payload FROM nostos_data WHERE table_name = ?1 AND pk = ?2",
            Some(&bind),
        ) {
            Ok(rows) if rows.length() > 0 => {
                let row = js_sys::Array::from(&rows.get(0));
                let payload_val = row.get(0);
                if payload_val.is_string() {
                    // TEXT fallback (shouldn't happen for BLOB, but defensive)
                    Ok(Some(
                        payload_val.as_string().unwrap_or_default().into_bytes(),
                    ))
                } else {
                    // Uint8Array (the normal path for BLOB columns)
                    Ok(Some(Uint8Array::new(&payload_val).to_vec()))
                }
            }
            Ok(_) => Ok(None), // zero rows
            Err(e) => Err(e),
        }
    }

    /// Run an arbitrary `SELECT` and return rows as a JSON string. Mirrors
    /// native `SqliteStorage::query` (sqlite.rs L416-449). The JS sqlite-wasm
    /// instance supports `db.selectObjects(sql)` which returns `[{col: val}]`
    /// directly — if the JS glue exposes it, we use it; otherwise we fall back
    /// to `selectRows` (array mode) and return the raw array.
    /// ponytail: the JS glue (`sqlite_wasm_glue.js`) should expose
    /// `selectObjects` for column-named output parity; rewire when convenient.
    pub fn query_json(&self, sql: &str) -> Result<String, StorageError> {
        // Try the JS glue's `selectObjects` first (column-named object rows).
        if let Ok(v) = self.call("selectObjects", &[JsValue::from_str(sql)]) {
            return js_sys::JSON::stringify(&v)
                .map(|s| s.as_string().unwrap_or_else(|| "[]".to_string()))
                .map_err(|e| {
                    StorageError::Backend(format!("SqliteWasm query_json stringify: {e:?}"))
                });
        }
        // Fallback: `selectRows` (array mode). The caller gets an array-of-
        // arrays — column names are position-dependent. This is correct for
        // simple queries where the caller knows the column order.
        let rows = self.select_rows(sql, None)?;
        js_sys::JSON::stringify(&rows)
            .map(|s| s.as_string().unwrap_or_else(|| "[]".to_string()))
            .map_err(|e| StorageError::Backend(format!("SqliteWasm query_json stringify: {e:?}")))
    }

    /// Materialize one SQLite `VIEW` per synced table, projected over the
    /// opaque `nostos_data` BLOB via JSON1 (WS2 read foundation). Mirrors
    /// native `SqliteStorage::apply_schema` (sqlite.rs L479-514).
    pub fn apply_schema(&self, tables: &[(String, Vec<String>)]) -> Result<(), StorageError> {
        for (name, columns) in tables {
            // Lead with `pk AS _pk` so the view carries the row's replication
            // key (same as native). Then json_extract per column.
            let mut cols: Vec<String> = vec!["pk AS _pk".to_string()];
            for c in columns {
                cols.push(format!("json_extract(payload, '$.{c}') AS {c}"));
            }
            // DROP + CREATE so a changed schema refreshes the projection.
            self.exec(&format!("DROP VIEW IF EXISTS {name}"), None)?;
            self.exec(
                &format!(
                    "CREATE VIEW {name} AS SELECT {} \
                     FROM nostos_data WHERE table_name = '{}'",
                    cols.join(", "),
                    name.replace('\'', "''")
                ),
                None,
            )?;
        }
        Ok(())
    }

    /// Count pending (non-dead-lettered) writes. Used by `watchWriteStatus`.
    pub fn pending_count(&self) -> u64 {
        self.select_value_str("SELECT COUNT(*) FROM nostos_outbox WHERE dlq = 0", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// Count dead-lettered writes. Used by `watchWriteStatus`.
    pub fn dead_letter_count(&self) -> u64 {
        self.select_value_str("SELECT COUNT(*) FROM nostos_outbox WHERE dlq = 1", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    /// The last error from the most recent dead-lettered write. Used by
    /// `watchWriteStatus`. Returns `None` if no dead-lettered writes or the
    /// `last_error` column is absent.
    pub fn last_dead_letter_error(&self) -> Option<String> {
        self.select_value_str(
            "SELECT last_error FROM nostos_outbox WHERE dlq = 1 \
             ORDER BY dead_lettered_at DESC LIMIT 1",
            None,
        )
        .filter(|s| !s.is_empty())
    }
}

// ---- Storage impl ----

impl Storage for SqliteWasmStorage {
    fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        Ok(self.read_checkpoint())
    }

    fn epoch(&self) -> nostos_core::Result<u64> {
        Ok(self
            .select_value_str("SELECT value FROM nostos_meta WHERE key = 'epoch'", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0))
    }

    /// The direct-mode `xid8` horizon, mirroring native `SqliteStorage`
    /// (sqlite.rs L626) down to the `nostos_meta` key.
    ///
    /// Overriding this is NOT optional even though the trait has a default:
    /// `Ok(None)` means "fresh database", so a browser client would re-pull
    /// the entire retained change log on every single launch and never say
    /// why. The default is the honest degrade for a backend that has no
    /// durable store; this one does. (Caught by the browser leg of
    /// `nostos_core::conformance` — the Rust legs could not see it.)
    fn horizon(&self) -> nostos_core::Result<Option<String>> {
        Ok(self.select_value_str("SELECT value FROM nostos_meta WHERE key = 'horizon'", None))
    }

    /// Persist the horizon. Called after the batch it covers has committed, so
    /// a crash in the window leaves the horizon behind the rows and the next
    /// pull re-applies them idempotently.
    fn save_horizon(&mut self, horizon: &str) -> nostos_core::Result<()> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(horizon));
        bind.push(&JsValue::from_str(horizon));
        self.exec(
            "INSERT INTO nostos_meta (key, value) VALUES ('horizon', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            Some(&bind),
        )?;
        Ok(())
    }

    fn principal(&self) -> nostos_core::Result<Option<String>> {
        Ok(self.select_value_str(
            "SELECT value FROM nostos_meta WHERE key = 'principal'",
            None,
        ))
    }

    fn save_principal(&mut self, principal: &str) -> nostos_core::Result<()> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(principal));
        self.exec(
            "INSERT INTO nostos_meta (key, value) VALUES ('principal', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            Some(&bind),
        )?;
        Ok(())
    }

    fn save_epoch(&self, epoch: u64) -> nostos_core::Result<()> {
        // INSERT OR IGNORE ensures the key exists, then UPDATE sets the value.
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_str(&epoch.to_string()));
        bind.push(&JsValue::from_str("epoch"));
        self.exec(
            "INSERT INTO nostos_meta (key, value) VALUES ('epoch', '0') ON CONFLICT(key) DO UPDATE SET value = ?1",
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
            "SELECT pk FROM nostos_data WHERE table_name = ?1 ORDER BY pk ASC",
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
                "DELETE FROM nostos_data WHERE table_name = ?1 AND pk = ?2",
                Some(&bind),
            )?;
        }
        Ok(())
    }

    fn clear(&mut self) -> nostos_core::Result<()> {
        // ADR-0029 / ADR-0051: clearAll atomically wipes rows, outbox,
        // checkpoint, horizon and principal while retaining device_id.
        // The checkpoint → 0 is load-bearing (resume-without-snapshot guard).
        self.call_void("clearAll", &[])?;
        // Keep this idempotent delete for older JS hosts whose clearAll only
        // knows about the LSN checkpoint. A surviving horizon would make the
        // next principal resume a log they have no rows from.
        self.exec(
            "DELETE FROM nostos_meta WHERE key IN ('horizon', 'principal')",
            None,
        )?;
        Ok(())
    }

    /// Override to read the raw payload bytes from `nostos_data` (ADR-0030
    /// addendum). Mirrors native `SqliteStorage::read_payload` (sqlite.rs
    /// L889-903). Used by the PN-Counter CRDT's client-side RMW so
    /// `counter_apply_delta` can read the current value before applying.
    fn read_payload(&self, table: &str, pk: &str) -> nostos_core::Result<Option<Vec<u8>>> {
        SqliteWasmStorage::read_payload(self, table, pk)
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
            "INSERT INTO nostos_outbox (table_name, op, pk, payload) VALUES (?1, ?2, ?3, ?4)",
            Some(&bind),
        )?;
        // Read the AUTOINCREMENT id. `selectValue` returns the last rowid.
        let id = self
            .select_value_str("SELECT last_insert_rowid()", None)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|id| *id > 0)
            .ok_or_else(|| StorageError::Backend("SQLite-WASM returned no outbox ID".into()))?;
        Ok(id)
    }

    /// Transactional enqueue: all writes in one SQLite txn or none. Mirrors
    /// native `SqliteStorage::enqueue_batch` (sqlite.rs L1015-1038). The
    /// sequential per-op default the trait provides is NOT atomic — a mid-batch
    /// failure would leave partial rows, violating ADR-0032 T3.
    fn enqueue_batch(&mut self, writes: Vec<PendingWrite>) -> nostos_core::Result<Vec<u64>> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }
        // BEGIN → loop INSERT → COMMIT. On any failure, ROLLBACK so nothing
        // commits (atomicity contract). The JS sqlite-wasm instance runs
        // `exec("BEGIN")` / `exec("COMMIT")` / `exec("ROLLBACK")` as DML
        // statements — same as rusqlite's `transaction()` under the hood.
        self.exec("BEGIN", None)?;
        let mut ids = Vec::with_capacity(writes.len());
        let result: nostos_core::Result<()> = (|| {
            for w in &writes {
                let bind = js_sys::Array::new();
                bind.push(&JsValue::from_str(&w.table));
                bind.push(&JsValue::from_str(w.op.as_wire_str()));
                bind.push(&JsValue::from_str(&w.pk));
                match &w.payload_json {
                    Some(p) => {
                        bind.push(&JsValue::from_str(p));
                    }
                    None => {
                        bind.push(&JsValue::NULL);
                    }
                }
                self.exec(
                    "INSERT INTO nostos_outbox (table_name, op, pk, payload) VALUES (?1, ?2, ?3, ?4)",
                    Some(&bind),
                )?;
                let id = self
                    .select_value_str("SELECT last_insert_rowid()", None)
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|id| *id > 0)
                    .ok_or_else(|| {
                        StorageError::Backend("SQLite-WASM returned no outbox ID".into())
                    })?;
                ids.push(id);
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.exec("COMMIT", None)?;
                Ok(ids)
            }
            Err(e) => {
                // Best-effort rollback — ignore errors here (the original error
                // is what matters).
                let _ = self.exec("ROLLBACK", None);
                Err(e)
            }
        }
    }

    fn pending(&self) -> nostos_core::Result<Vec<(u64, PendingWrite)>> {
        let rows = self.select_rows(
            "SELECT id, table_name, op, pk, payload FROM nostos_outbox WHERE dlq = 0 ORDER BY id ASC",
            None,
        )?;
        let mut out = Vec::new();
        for i in 0..rows.length() {
            let row = js_sys::Array::from(&rows.get(i));
            let id = row
                .get(0)
                .as_string()
                .or_else(|| {
                    row.get(0).as_f64().and_then(|value| {
                        (value.is_finite()
                            && value.fract() == 0.0
                            && value > 0.0
                            && value <= 9_007_199_254_740_991.0)
                            .then(|| format!("{value:.0}"))
                    })
                })
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|id| *id > 0)
                .ok_or_else(|| StorageError::Backend("invalid SQLite-WASM outbox ID".into()))?;
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
        self.exec("DELETE FROM nostos_outbox WHERE id = ?1", Some(&bind))?;
        Ok(())
    }

    fn bump_attempts(&self, id: u64) -> nostos_core::Result<u32> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        self.exec(
            "UPDATE nostos_outbox SET attempts = attempts + 1 WHERE id = ?1 AND dlq = 0",
            Some(&bind),
        )?;
        let bind2 = js_sys::Array::new();
        bind2.push(&JsValue::from_f64(id as f64));
        Ok(self
            .select_value_str(
                "SELECT attempts FROM nostos_outbox WHERE id = ?1",
                Some(&bind2),
            )
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0))
    }

    fn mark_dead_letter(&self, id: u64) -> nostos_core::Result<()> {
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        self.exec(
            "UPDATE nostos_outbox SET dlq = 1 WHERE id = ?1",
            Some(&bind),
        )?;
        Ok(())
    }

    /// Override to persist the server's error message + a Unix epoch-ms
    /// timestamp alongside the `dlq` flag (ADR-0032 T5). Mirrors native
    /// `SqliteStorage::mark_dead_letter_with_error` (sqlite.rs L1168-1185).
    fn mark_dead_letter_with_error(&self, id: u64, error: Option<&str>) -> nostos_core::Result<()> {
        let now_ms: i64 = (js_sys::Date::now() * 1000.0) as i64;
        let bind = js_sys::Array::new();
        bind.push(&JsValue::from_f64(id as f64));
        match error {
            Some(e) => {
                bind.push(&JsValue::from_str(e));
            }
            None => {
                bind.push(&JsValue::NULL);
            }
        }
        bind.push(&JsValue::from_f64(now_ms as f64));
        // If the v3 columns don't exist (pre-migration DB), this UPDATE fails;
        // fall back to the dlq-only path.
        let result = self.exec(
            "UPDATE nostos_outbox SET dlq = 1, last_error = ?2, dead_lettered_at = ?3 WHERE id = ?1",
            Some(&bind),
        );
        if result.is_ok() {
            Ok(())
        } else {
            // Fallback: old schema without the columns.
            let bind2 = js_sys::Array::new();
            bind2.push(&JsValue::from_f64(id as f64));
            self.exec(
                "UPDATE nostos_outbox SET dlq = 1 WHERE id = ?1",
                Some(&bind2),
            )
        }
    }

    fn apply_local(&mut self, write: &PendingWrite) -> nostos_core::Result<()> {
        // Optimistic instant-local write (WS2 slice-2): store the row NOW so
        // the view reflects the user's write before any server round-trip.
        // For OR-set/counter tables the optimistic edit MERGES element-wise
        // by HLC/per-replica max (ADR-0030) instead of clobbering. Mirrors
        // native `SqliteStorage::apply_local` (sqlite.rs L1191-1242).
        // ponytail: mirrors SyncClient::write → apply_local; rewire to share
        // when convenient.
        match write.op {
            WriteOp::Upsert => {
                let incoming = write.payload_json.as_deref().unwrap_or("null");
                let incoming_bytes = incoming.as_bytes();
                let bytes = if self.or_set_tables.contains(write.table.as_str()) {
                    let existing = self
                        .read_payload(&write.table, &write.pk)?
                        .unwrap_or_default();
                    nostos_domain::merge_or_set_or_lww(&existing, incoming_bytes)
                } else if self.counter_tables.contains(write.table.as_str()) {
                    let existing = self
                        .read_payload(&write.table, &write.pk)?
                        .unwrap_or_default();
                    nostos_domain::merge_counter_or_lww(&existing, incoming_bytes)
                } else {
                    incoming_bytes.to_vec()
                };
                let bind = js_sys::Array::new();
                bind.push(&JsValue::from_str(&write.table));
                bind.push(&JsValue::from_str(&write.pk));
                bind.push(&Uint8Array::from(&bytes[..]).into());
                self.exec(
                    "INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) \
                     VALUES (?1, ?2, ?3, 0) \
                     ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload",
                    Some(&bind),
                )?;
            }
            WriteOp::Delete => {
                let bind = js_sys::Array::new();
                bind.push(&JsValue::from_str(&write.table));
                bind.push(&JsValue::from_str(&write.pk));
                self.exec(
                    "DELETE FROM nostos_data WHERE table_name = ?1 AND pk = ?2",
                    Some(&bind),
                )?;
            }
            WriteOp::Patch => {
                // Patch is not yet implemented on any backend (native included);
                // treat as upsert for now (the server path clobbers on patch
                // too — the PATCH semantic is a server-side feature).
                let payload = write.payload_json.as_deref().unwrap_or("{}");
                let bind = js_sys::Array::new();
                bind.push(&JsValue::from_str(&write.table));
                bind.push(&JsValue::from_str(&write.pk));
                bind.push(&Uint8Array::from(payload.as_bytes()).into());
                self.exec(
                    "INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) \
                     VALUES (?1, ?2, ?3, 0) \
                     ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload",
                    Some(&bind),
                )?;
            }
            WriteOp::Increment => {
                // Increment is handled by the counter CRDT path (counterIncrement
                // verb) — it should never reach apply_local as a bare Increment
                // op. If it does, treat as an upsert (the payload carries the
                // counter value). Mirrors native behavior.
                if let Some(payload) = &write.payload_json {
                    let bind = js_sys::Array::new();
                    bind.push(&JsValue::from_str(&write.table));
                    bind.push(&JsValue::from_str(&write.pk));
                    bind.push(&Uint8Array::from(payload.as_bytes()).into());
                    self.exec(
                        "INSERT INTO nostos_data (table_name, pk, payload, applied_lsn) \
                         VALUES (?1, ?2, ?3, 0) \
                         ON CONFLICT(table_name, pk) DO UPDATE SET payload = excluded.payload",
                        Some(&bind),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn clear(&mut self) -> nostos_core::Result<()> {
        // Wipe the outbox + dead-letter queue (ADR-0029). Storage::clear wipes
        // rows + checkpoint; this is the outbox-only half.
        self.exec("DELETE FROM nostos_outbox", None)?;
        Ok(())
    }
}

// ---- helpers ----

/// Build a closure that maps a `JsValue` error to `StorageError::Backend`.
fn js_err(ctx: &str) -> impl Fn(JsValue) -> StorageError + '_ {
    move |e| StorageError::Backend(format!("SqliteWasm {ctx}: {e:?}"))
}
