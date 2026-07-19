//! `SqliteStorage` — the real durable backend for the native client.
//!
//! Implements [`nostos_core::Storage`] (the apply/checkpoint surface) AND
//! [`nostos_core::Outbox`] (the durable write-queue surface) over `rusqlite`
//! (workspace `bundled` feature, so this crate brings its own SQLite binary —
//! zero external deps to run). Both surfaces share ONE SQLite file so a crash
//! can't strand one without the other (ADR-0013). Rows are stored as opaque
//! payload bytes keyed by `(table, pk)`; the LSN checkpoint lives in a
//! `cairn_meta` row; the pending write-queue lives in `cairn_outbox`.
//!
//! ## Why opaque bytes
//!
//! The wire frame carries the logical-replication tuple image as opaque hex.
//! There is no column decoder yet (that arrives with the dynamic predicate
//! engine, ADR-0012). Storing the bytes verbatim is honest — it's exactly what
//! the wire delivers — and it makes the row durable + resumable. A schema-aware
//! projection layer can be layered above this table later without re architecting
//! the apply path.
//!
//! ## Atomicity (the load-bearing property)
//!
//! [`Storage::apply_batch`] opens a single transaction, applies every `RowOp`
//! (upsert / delete by `(table, pk)`), writes the checkpoint, and commits. If
//! the process dies mid-batch the transaction rolls back — **no row is committed
//! without its checkpoint, no checkpoint advances past un-applied rows.** This
//! is what makes reconnect resume correct (ADR-0009 on the client side).
//!
//! The outbox methods ([`Outbox::enqueue`], [`Outbox::mark_done`]) each run in
//! their own transaction, so an enqueued write is durable the instant `enqueue`
//! returns — a crash between a user action and the server's ack leaves the
//! write queued, not lost.

use std::sync::Mutex;

use nostos_core::{Outbox, PendingWrite, Storage, StorageError, WriteOp};
use nostos_domain::{Lsn, RowOp};
use rusqlite::Connection;

/// The opaque-bytes row table + the meta table + the durable write outbox.
/// One row per `(table, pk)` in `cairn_data`; the LSN checkpoint is the single
/// `checkpoint` row in `cairn_meta`; `cairn_outbox` holds client writes that
/// have not yet been ack'd by the server (ADR-0013).
///
/// `cairn_outbox` carries two dead-letter-policy columns (ADR-0013 v2):
/// - `attempts` — bumped on every `WriteResult{ok:false}`; when it reaches the
///   configured `dead_letter_max_attempts`, the flush loop quarantines the row.
/// - `dlq` — 1 once the row has been dead-lettered; `pending()` excludes
///   `dlq = 1` rows so the queue head advances past a permanently-failing
///   write. The row is NOT deleted (it stays inspectable via
///   [`SqliteStorage::dead_letter_entries`]); `mark_done` is the only path that
///   deletes, and it only fires on `ok:true`.
const SCHEMA: &str = "\
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

/// The single meta key holding the last-applied LSN (`u64` decimal).
const CHECKPOINT_KEY: &str = "checkpoint";

/// A synced table's schema as the client sees it — the minimal projection of
/// the server's `SchemaDescriptor` (nostos-application) that the view layer
/// needs. Defined here (not reusing `SchemaDescriptor`) because nostos-client
/// may NOT depend on nostos-application (hexagonal dependency direction).
/// ponytail: `Deserialize` + `pg_oid`/`affinity` arrive when the `GET /schema`
/// fetch wiring (WS3) lands — a view over `json_extract` only needs names.
#[derive(Debug, Clone)]
pub struct ClientTable {
    /// Canonical table id — matches the wire `table` field / `cairn_data.table_name`.
    pub name: String,
    /// Primary-key column names. Informational for the view (the PK value is
    /// extracted from the JSON payload like any other column); carried for the
    /// future materialized-table path.
    pub primary_key: Vec<String>,
    /// Column names in tuple order.
    pub columns: Vec<String>,
}

/// A durable SQLite-backed store. Owns one connection; the client apply loop is
/// single-threaded by construction, so the `Mutex` is uncontended in practice —
/// it exists so `Storage` (which takes `&mut self`) is satisfiable and so a
/// future multi-reader path can share the connection safely.
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    /// Open or create the store at `path`. Runs the schema migration
    /// idempotently (CREATE IF NOT EXISTS).
    ///
    /// # Errors
    /// Returns [`StorageError::Backend`] if SQLite can't open or migrate the file.
    pub fn open(path: &str) -> Result<Self, StorageError> {
        let conn = Connection::open(path).map_err(rusqlite_err)?;
        Self::init(conn)
    }

    /// Open an in-memory database (`:memory:`). The store is process-scoped —
    /// it does NOT survive a drop, but it's real SQLite, so it exercises the
    /// exact SQL path the on-disk store uses. Used by tests + the chaos e2e.
    ///
    /// # Errors
    /// Returns [`StorageError::Backend`] only if SQLite fails to allocate.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory().map_err(rusqlite_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        conn.execute_batch(SCHEMA).map_err(rusqlite_err)?;
        Self::migrate_outbox_dlq(&conn)?;
        Self::migrate_applied_lsn(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// v1 migration: add the dead-letter columns (`attempts`, `dlq`) to
    /// `cairn_outbox` for databases created by a pre-DLQ binary (ADR-0013 v2).
    ///
    /// Why a column probe and not `PRAGMA user_version`: the `CREATE TABLE IF
    /// NOT EXISTS` in [`SCHEMA`] always emits the new columns on a fresh file,
    /// so a brand-new database already has them at `user_version = 0`. That
    /// makes `user_version` ambiguous between "fresh new-schema DB" (no work)
    /// and "old-schema DB predating the DLQ policy" (needs `ALTER TABLE`).
    /// Probing `PRAGMA table_info` for the `dlq` column resolves the ambiguity
    /// directly: present ⇒ nothing to do, absent ⇒ `ALTER TABLE ADD COLUMN`
    /// for both (they ship as a pair). Idempotent — safe to run on every open.
    ///
    /// `ALTER TABLE … ADD COLUMN` with a `DEFAULT` is constant-time on SQLite
    /// (it doesn't rewrite existing rows — the default is stored in the schema
    /// and applied on read), so this migration is cheap even on a large outbox.
    fn migrate_outbox_dlq(conn: &Connection) -> Result<(), StorageError> {
        if outbox_has_column(conn, "dlq")? {
            return Ok(());
        }
        // Add both columns as a pair. SQLite evaluates `ADD COLUMN` left-to-
        // right; if the first succeeds and the second somehow fails (disk
        // full mid-DDL), a re-open retries idempotently (the `dlq` probe
        // would then be true, skipping this branch — but `attempts` would
        // already be present because we add it first). The column probe
        // checks `dlq` specifically because it's the second column added —
        // its presence implies both landed.
        conn.execute(
            "ALTER TABLE cairn_outbox ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(rusqlite_err)?;
        conn.execute(
            "ALTER TABLE cairn_outbox ADD COLUMN dlq INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(rusqlite_err)?;
        Ok(())
    }

    /// v2 migration: add the per-row `applied_lsn` column to `cairn_data` for
    /// databases created by a pre-slice-4a binary (ADR-0025 slice 4a). The
    /// column drives per-row LSN gating (a stale replay/live op must not
    /// overwrite a newer row). Same probe-then-ALTER pattern as
    /// [`Self::migrate_outbox_dlq`]: `CREATE TABLE IF NOT EXISTS` in [`SCHEMA`]
    /// emits the column on a fresh file, so this only fires on an existing DB
    /// from an older binary. `ADD COLUMN … DEFAULT 0` is constant-time on SQLite
    /// (no row rewrite), so this is cheap even on a large `cairn_data`.
    fn migrate_applied_lsn(conn: &Connection) -> Result<(), StorageError> {
        if cairn_data_has_column(conn, "applied_lsn")? {
            return Ok(());
        }
        conn.execute(
            "ALTER TABLE cairn_data ADD COLUMN applied_lsn INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(rusqlite_err)?;
        Ok(())
    }

    /// Count the rows in the data table (test/diagnostics only). Used by the
    /// chaos e2e to assert exact row counts after a reconnect.
    #[doc(hidden)]
    pub fn row_count_for_test(&self) -> usize {
        let conn = self.conn.lock().expect("row_count: storage mutex poisoned");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .expect("count query is infallible on a valid schema");
        usize::try_from(count).expect("row count is non-negative")
    }

    /// Borrow the underlying connection under the mutex (test-only). Lets an
    /// integration test read rows out of `cairn_data` / `cairn_outbox` directly
    /// for assertions that aren't worth a public accessor (e.g. a round-trip
    /// payload check). The guard releases on drop, matching the internal usage.
    #[doc(hidden)]
    pub fn conn_for_test(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("conn_for_test: storage mutex poisoned")
    }

    /// Enumerate the `(pk, payload_bytes)` pairs currently held for `table`,
    /// sorted by pk. The `SqliteStorage` counterpart to
    /// [`nostos_core::InMemoryStorage::rows_for`] — same signature/shape, real
    /// SQLite-backed. Exists so an in-process readback consumer (the FFI
    /// bridges, e.g. `nostos_flutter`'s `watch()`) can render the engine's
    /// *current durable state* for a table without re-implementing the apply
    /// path or keeping a parallel in-memory index. A diagnostic/readback
    /// accessor — NOT part of the [`nostos_core::Storage`] trait, which stays
    /// minimal (`checkpoint` + `apply_batch`).
    ///
    /// # Errors
    /// Returns [`StorageError::Backend`] if the read query fails.
    pub fn rows_for(&self, table: &str) -> Result<Vec<(String, Vec<u8>)>, StorageError> {
        let conn = self.conn.lock().expect("rows_for: storage mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT pk, payload FROM cairn_data WHERE table_name = ?1 ORDER BY pk ASC")
            .map_err(rusqlite_err)?;
        let rows = stmt
            .query_map(rusqlite::params![table], |row| {
                let pk: String = row.get(0)?;
                let payload: Vec<u8> = row.get(1)?;
                Ok((pk, payload))
            })
            .map_err(rusqlite_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(rusqlite_err)?);
        }
        Ok(out)
    }

    /// All writes quarantined by the dead-letter policy (ADR-0013 v2), oldest
    /// first. Each entry is `(id, write)` mirroring [`Outbox::pending`]'s
    /// shape, but restricted to `dlq = 1` rows. These are writes the flush loop
    /// gave up on after `dead_letter_max_attempts` rejections — they are NOT
    /// deleted (the row stays in `cairn_outbox` for operator inspection and
    /// potential replay), they just don't block the queue head anymore.
    ///
    /// A read-only diagnostic accessor — NOT part of the [`nostos_core::Outbox`]
    /// trait (which stays WASM-clean and backend-agnostic). Lives on
    /// `SqliteStorage` because the DLQ rows are a SQLite-specific concern; a
    /// different backend would expose its own inspection surface.
    ///
    /// # Errors
    /// Returns [`StorageError::Backend`] if the read query fails.
    pub fn dead_letter_entries(&self) -> Result<Vec<(u64, PendingWrite)>, StorageError> {
        let conn = self
            .conn
            .lock()
            .expect("dead_letter_entries: storage mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT id, table_name, op, pk, payload FROM cairn_outbox WHERE dlq = 1 ORDER BY id ASC")
            .map_err(rusqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let table: String = row.get(1)?;
                let op_wire: String = row.get(2)?;
                let pk: String = row.get(3)?;
                let payload: Option<String> = row.get(4)?;
                let op = WriteOp::from_wire_str(&op_wire).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        format!("corrupt outbox op {op_wire:?}").into(),
                    )
                })?;
                Ok((
                    u64::try_from(id).expect("rowid is non-negative"),
                    PendingWrite {
                        table,
                        op,
                        pk,
                        payload_json: payload,
                    },
                ))
            })
            .map_err(rusqlite_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(rusqlite_err)?);
        }
        Ok(out)
    }

    /// Run an arbitrary read-only SQL query against the durable store and
    /// return each row as a `serde_json::Map<String, Value>` keyed by column
    /// name. This is the PowerSync-parity read surface (P1-Rust): a Flutter
    /// `watch(sql)` call can run any `SELECT` against `cairn_data` and render
    /// the result set directly, without a parallel query engine or a
    /// column-level decoder (ADR-0012 is still future work).
    ///
    /// ## JSON1 + opaque payload
    ///
    /// The dev writes `json_extract(payload, '$.col')` directly in their SQL
    /// — the bundled SQLite ships JSON1 (the workspace `rusqlite` `bundled`
    /// feature compiles it in), and `cairn_data.payload` stores the
    /// logical-replication tuple image as opaque bytes that, for a JSON-backed
    /// source, ARE valid JSON text. Example:
    ///
    /// ```text
    /// storage.query("SELECT pk, json_extract(payload, '$.title') AS title \
    ///                FROM cairn_data WHERE table_name = 'tasks'")
    /// ```
    ///
    /// returns a `Vec<Map>` where each map is `{"pk": "...", "title": "..."}`.
    /// Non-JSON payloads (future binary sources) return NULL from
    /// `json_extract`, not an error.
    ///
    /// ## Type mapping
    ///
    /// SQLite value → JSON value:
    /// - `NULL` → `Value::Null`
    /// - `INTEGER` → `Value::Number` (i64)
    /// - `REAL` → `Value::Number` (f64; NaN/Inf → `Value::Null`, JSON can't
    ///   represent them)
    /// - `TEXT` → `Value::String`
    /// - `BLOB` → `Value::String` holding the lowercase-hex encoding (matches
    ///   the wire payload's hex convention — see `client.rs::decode_hex`). A
    ///   reader who needs the raw bytes hex-decodes on the client side.
    ///
    /// Read-only by convention — there is no enforcement that `sql` is a
    /// `SELECT` (SQLite doesn't distinguish; a `DELETE` would execute and
    /// return an empty result set, bypassing the atomicity contract on the
    /// write side). Callers MUST NOT pass DML/DDL through this surface; the
    /// write path is `enqueue` → flush loop → `apply_batch`, full stop. A
    /// future hardening could parse the SQL and reject non-SELECT, but the
    /// trait boundary today is "this is a read-side accessor on the same
    /// `Mutex<Connection>` as the write path" — it does not change the
    /// [`Storage`] or [`Outbox`] traits (ADR-0013 v2 read-side addition).
    ///
    /// # Errors
    /// Returns [`StorageError::Backend`] if the SQL fails to prepare or a row
    /// fails to decode.
    pub fn query(
        &self,
        sql: &str,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, StorageError> {
        let conn = self.conn.lock().expect("query: storage mutex poisoned");
        let mut stmt = conn.prepare(sql).map_err(rusqlite_err)?;
        // Snapshot the column names BEFORE iterating: `column_name` borrows
        // from `stmt`, and `query_map` also borrows `stmt`. Collecting into
        // owned `String`s ends the borrow so the query iterator can proceed.
        // Unnamed columns (rare — e.g. `SELECT 1`) fall back to `colN` so the
        // map entry is always keyed predictably.
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| {
                stmt.column_name(i)
                    .map_or_else(|_| format!("col{i}"), str::to_string)
            })
            .collect();
        let rows = stmt
            .query_map([], |row| {
                let mut map = serde_json::Map::new();
                for (i, name) in col_names.iter().enumerate() {
                    let val: rusqlite::types::Value = row.get(i)?;
                    map.insert(name.clone(), sqlite_value_to_json(val));
                }
                Ok(map)
            })
            .map_err(rusqlite_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(rusqlite_err)?);
        }
        Ok(out)
    }

    /// Materialize one SQLite `VIEW` per synced table, projected over the opaque
    /// `cairn_data` BLOB via JSON1 (WS2 read foundation). After this, the dev
    /// writes natural PowerSync-style SQL — `SELECT title FROM tasks` — and it
    /// resolves against the view, which `json_extract`s each column out of the
    /// replication payload. The Pg path emits a column-named JSON object (see
    /// `tuple_to_json_payload` in nostos-infra), so column identity is IN the
    /// payload — no decoder, no inference, no apply-path change.
    ///
    /// `cairn_data` stays the single source of truth; the apply path is
    /// UNCHANGED. This is the lazy cousin of "materialized typed tables": zero
    /// new storage, zero migration, reversible (`DROP VIEW`). Ceiling: no non-PK
    /// column indexes (a view computes `json_extract` per row → full scan on
    /// `WHERE col = ?`). ponytail: fast-follow to real typed tables + indexes
    /// when a query needs them. FakeReplicator's non-JSON bytes degrade to NULL
    /// (dev fixture, not production).
    ///
    /// Each view is `DROP VIEW IF EXISTS` + `CREATE VIEW`, so re-applying a
    /// *changed* schema refreshes the projection in place — bumping the
    /// declared schema IS the client migration (PowerSync model: the synced
    /// `cairn_data` rows are schemaless; views are cheap, data is untouched).
    /// Runs at connect time, before any watch() statement is armed, so no
    /// cursor is open over the view mid-DDL.
    ///
    /// Idempotent for an unchanged schema. An inherent method (not on `Storage`,
    /// which stays minimal), like `query()` / `rows_for()`.
    ///
    /// # Errors
    /// [`StorageError::Backend`] if any DDL fails.
    pub fn apply_schema(&self, tables: &[ClientTable]) -> Result<(), StorageError> {
        let conn = self
            .conn
            .lock()
            .expect("apply_schema: storage mutex poisoned");
        for t in tables {
            // ponytail: the catalog always has ≥1 column; we don't special-case
            // an empty list (it would yield invalid `SELECT FROM`).
            // Lead with `pk AS _pk` so the view carries the row's replication
            // key — the same `_pk` the subscribe row-stream stamps
            // (`row_to_json_object` in nostos_flutter). Without it, a PowerSync-
            // style `SELECT * FROM <table>` returns no key, making write-back
            // (delete/edit, which key on `_pk`) impossible through the clean DX.
            let cols: Vec<String> = std::iter::once("pk AS _pk".to_string())
                .chain(
                    t.columns
                        .iter()
                        .map(|c| format!("json_extract(payload, '$.{c}') AS {}", quote_ident(c))),
                )
                .collect();
            // SQLite views are static schema objects — bind params are NOT
            // allowed in a view definition, so the table filter is an inlined,
            // escaped string literal (the name is trusted catalog data, but the
            // escape is cheap defense-in-depth).
            let view = quote_ident(&view_name(&t.name));
            conn.execute(&format!("DROP VIEW IF EXISTS {view}"), [])
                .map_err(rusqlite_err)?;
            let ddl = format!(
                "CREATE VIEW {view} AS SELECT {cols} \
                 FROM cairn_data WHERE table_name = {tbl}",
                cols = cols.join(", "),
                tbl = quote_string(&t.name),
            );
            conn.execute(&ddl, []).map_err(rusqlite_err)?;
        }
        Ok(())
    }
}

impl Storage for SqliteStorage {
    fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        let conn = self
            .conn
            .lock()
            .expect("checkpoint: storage mutex poisoned");
        let raw: String = conn
            .query_row(
                "SELECT value FROM cairn_meta WHERE key = ?1",
                rusqlite::params![CHECKPOINT_KEY],
                |row| row.get(0),
            )
            .map_err(rusqlite_err)?;
        let raw: u64 = raw.parse().map_err(|e: std::num::ParseIntError| {
            StorageError::Backend(format!("corrupt checkpoint value {raw:?}: {e}"))
        })?;
        Ok(Lsn::new(raw))
    }

    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &std::collections::HashSet<String>,
    ) -> nostos_core::Result<()> {
        // Snapshot the pending optimistic writes BEFORE taking the conn lock
        // (`pending()` locks self.conn internally → calling it after the lock
        // below would deadlock the non-reentrant Mutex). Replayed after the
        // server batch so optimistic state stays on top of the server image —
        // the reconnect-glitch fix, Piece B
        // (docs/plans/reconnect-glitch-fix-2026-07-19.md). `unwrap_or_default`
        // keeps the replay best-effort: a pending-read failure must NOT block
        // the authoritative server batch from landing.
        let pending = self.pending().unwrap_or_default();
        let mut conn = self
            .conn
            .lock()
            .expect("apply_batch: storage mutex poisoned");
        let tx = conn.transaction().map_err(rusqlite_err)?;

        {
            // ADR-0025 slice 4a: per-row LSN gating. Four statements — gated
            // (live/replay; `>= applied_lsn` so a stale op can't overwrite a
            // newer row) vs unconditional (tables in `snapshot_tables`, design
            // D — authoritative snapshot current-state always clobbers). SQLite
            // stores integers as i64; clamp u64 lsns (real PG lsns fit i64).
            let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
            let mut upsert_gated = tx
                .prepare_cached(
                    "INSERT INTO cairn_data (table_name, pk, payload, applied_lsn) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(table_name, pk) DO UPDATE SET \
                     payload = excluded.payload, applied_lsn = excluded.applied_lsn \
                     WHERE cairn_data.applied_lsn <= excluded.applied_lsn",
                )
                .map_err(rusqlite_err)?;
            let mut upsert_uncond = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO cairn_data (table_name, pk, payload, applied_lsn) \
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(rusqlite_err)?;
            let mut delete_gated = tx
                .prepare_cached(
                    "DELETE FROM cairn_data \
                     WHERE table_name = ?1 AND pk = ?2 AND applied_lsn <= ?3",
                )
                .map_err(rusqlite_err)?;
            let mut delete_uncond = tx
                .prepare_cached("DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2")
                .map_err(rusqlite_err)?;

            for (op, lsn) in ops {
                let lsn_i64 = to_i64(*lsn);
                match op {
                    RowOp::Insert { table, pk, payload } | RowOp::Update { table, pk, payload } => {
                        if snapshot_tables.contains(table.as_str()) {
                            upsert_uncond
                                .execute(rusqlite::params![table, pk, payload.as_ref(), lsn_i64])
                                .map_err(rusqlite_err)?;
                        } else {
                            upsert_gated
                                .execute(rusqlite::params![table, pk, payload.as_ref(), lsn_i64])
                                .map_err(rusqlite_err)?;
                        }
                    }
                    RowOp::Delete { table, pk, .. } => {
                        if snapshot_tables.contains(table.as_str()) {
                            delete_uncond
                                .execute(rusqlite::params![table, pk])
                                .map_err(rusqlite_err)?;
                        } else {
                            delete_gated
                                .execute(rusqlite::params![table, pk, lsn_i64])
                                .map_err(rusqlite_err)?;
                        }
                    }
                }
            }

            // Replay-on-top (reconnect-glitch Piece B): re-stamp each pending
            // optimistic write so a server snapshot/stream — which lacks the
            // un-flushed local edits — can't flash the stale image. Locally-
            // deleted rows stay deleted, locally-modified rows keep their edit,
            // until the outbox flush + echo reconciles. UNCONDITIONAL statements
            // (local writes always win on re-stamp — they're authoritative-local
            // with no real WAL lsn yet; stamp applied_lsn = checkpoint so a later
            // server op at the same lsn still gates in). Patch does a read-merge-
            // write via `merge_payload` (same helper `apply_local` uses).
            // Best-effort per write: a malformed pending row is skipped
            // (`let _ =`), never fatal — the server batch + checkpoint still
            // commit; that one row's echo reconciles later.
            let local_lsn = i64::try_from(checkpoint.raw()).unwrap_or(i64::MAX);
            for (_, write) in &pending {
                match write.op {
                    WriteOp::Upsert => {
                        let payload = write.payload_json.as_deref().unwrap_or("null").as_bytes();
                        let _ = upsert_uncond.execute(rusqlite::params![
                            write.table,
                            write.pk,
                            payload,
                            local_lsn
                        ]);
                    }
                    WriteOp::Delete => {
                        let _ = delete_uncond.execute(rusqlite::params![write.table, write.pk]);
                    }
                    WriteOp::Patch => {
                        let patch_json = write.payload_json.as_deref().unwrap_or("{}");
                        let existing: Vec<u8> = tx
                            .query_row(
                                "SELECT payload FROM cairn_data \
                                 WHERE table_name = ?1 AND pk = ?2",
                                rusqlite::params![write.table, write.pk],
                                |r| r.get::<_, Vec<u8>>(0),
                            )
                            .unwrap_or_default();
                        let merged = merge_payload(&existing, patch_json.as_bytes());
                        let _ = upsert_uncond.execute(rusqlite::params![
                            write.table,
                            write.pk,
                            merged,
                            local_lsn
                        ]);
                    }
                }
            }
        }

        // Advance the checkpoint inside the SAME transaction. Monotonic: only
        // write if the new value exceeds the stored one (a stale replay batch
        // must not drag the cursor backward).
        let stored: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM cairn_meta WHERE key = ?1",
                rusqlite::params![CHECKPOINT_KEY],
                |row| row.get(0),
            )
            .map_err(rusqlite_err)?;
        // Advance the checkpoint inside the SAME transaction. Monotonic: only
        // write if the new value exceeds the stored one. `stored` is i64 from
        // SQLite; clamp to non-negative before the unsigned max (a negative
        // stored value would be corruption — we don't silently let it drag the
        // cursor below zero).
        let stored_u64 = stored.max(0).cast_unsigned();
        let new_raw = checkpoint.raw().max(stored_u64);
        tx.execute(
            "UPDATE cairn_meta SET value = ?1 WHERE key = ?2",
            rusqlite::params![new_raw.to_string(), CHECKPOINT_KEY],
        )
        .map_err(rusqlite_err)?;

        tx.commit().map_err(rusqlite_err)?;
        Ok(())
    }

    fn pks_for_table(&self, table: &str) -> nostos_core::Result<Vec<String>> {
        // The snapshot-reconcile seed read. Scoped by `table_name` so the
        // orphan-candidate set is per-table (a `snapshot_begin/end` pair
        // bracket exactly one table). Read-only — no transaction needed; the
        // mutex guard serializes against the apply path on the same conn.
        let conn = self
            .conn
            .lock()
            .expect("pks_for_table: storage mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT pk FROM cairn_data WHERE table_name = ?1 ORDER BY pk ASC")
            .map_err(rusqlite_err)?;
        let rows = stmt
            .query_map(rusqlite::params![table], |row| {
                let pk: String = row.get(0)?;
                Ok(pk)
            })
            .map_err(rusqlite_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(rusqlite_err)?);
        }
        Ok(out)
    }

    fn delete_pks(&mut self, table: &str, pks: &[String]) -> nostos_core::Result<()> {
        // One transaction for the whole batch — the orphan-reap is atomic. An
        // empty slice is a cheap no-op (no transaction opened). Idempotent:
        // deleting an absent pk affects 0 rows, never errors. Auto-commits on
        // `tx.commit()` so the reconcile lands durably before the pump acks.
        if pks.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .conn
            .lock()
            .expect("delete_pks: storage mutex poisoned");
        let tx = conn.transaction().map_err(rusqlite_err)?;
        {
            let mut delete = tx
                .prepare_cached("DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2")
                .map_err(rusqlite_err)?;
            for pk in pks {
                delete
                    .execute(rusqlite::params![table, pk])
                    .map_err(rusqlite_err)?;
            }
        }
        tx.commit().map_err(rusqlite_err)?;
        Ok(())
    }
}

impl Outbox for SqliteStorage {
    fn enqueue(&mut self, write: PendingWrite) -> nostos_core::Result<u64> {
        let mut conn = self.conn.lock().expect("enqueue: storage mutex poisoned");
        // One-row transaction: the write is durable the instant this commits.
        // A crash between now and the server's ack leaves the write queued, not
        // lost — exactly the property the outbox exists for.
        let tx = conn.transaction().map_err(rusqlite_err)?;
        let op_wire = write.op.as_wire_str();
        tx.execute(
            "INSERT INTO cairn_outbox (table_name, op, pk, payload) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![write.table, op_wire, write.pk, write.payload_json],
        )
        .map_err(rusqlite_err)?;
        let id: i64 = tx
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .map_err(rusqlite_err)?;
        tx.commit().map_err(rusqlite_err)?;
        // AUTOINCREMENT guarantees monotonicity (never reuses a deleted id), so
        // the returned u64 is a stable correlation key on the wire.
        Ok(u64::try_from(id).expect("rowid is non-negative"))
    }

    fn pending(&self) -> nostos_core::Result<Vec<(u64, PendingWrite)>> {
        let conn = self.conn.lock().expect("pending: storage mutex poisoned");
        // `WHERE dlq = 0` is the dead-letter exclusion (ADR-0013 v2): a
        // quarantined write stays in the table (inspectable via
        // [`SqliteStorage::dead_letter_entries`]) but is no longer "pending,"
        // so the flush loop's queue head can advance past it.
        let mut stmt = conn
            .prepare("SELECT id, table_name, op, pk, payload FROM cairn_outbox WHERE dlq = 0 ORDER BY id ASC")
            .map_err(rusqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let table: String = row.get(1)?;
                let op_wire: String = row.get(2)?;
                let pk: String = row.get(3)?;
                let payload: Option<String> = row.get(4)?;
                let op = WriteOp::from_wire_str(&op_wire).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        format!("corrupt outbox op {op_wire:?}").into(),
                    )
                })?;
                Ok((
                    u64::try_from(id).expect("rowid is non-negative"),
                    PendingWrite {
                        table,
                        op,
                        pk,
                        payload_json: payload,
                    },
                ))
            })
            .map_err(rusqlite_err)?;
        // Drain the iterator into a Vec, surfacing any conversion error (a
        // corrupt op string would be db damage — fail the read rather than
        // silently dropping a queued write).
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(rusqlite_err)?);
        }
        Ok(out)
    }

    fn mark_done(&mut self, id: u64) -> nostos_core::Result<()> {
        let mut conn = self.conn.lock().expect("mark_done: storage mutex poisoned");
        let tx = conn.transaction().map_err(rusqlite_err)?;
        tx.execute(
            "DELETE FROM cairn_outbox WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(rusqlite_err)?;
        tx.commit().map_err(rusqlite_err)?;
        // Idempotent: deleting a row that's already gone affects 0 rows — not
        // an error (a redelivery after a partial flush must not fail).
        // Note: a dead-lettered row (dlq=1) is NOT deleted by mark_done —
        // mark_done only fires on `WriteResult{ok:true}`, and a dead-lettered
        // write never receives an ack. The two paths are disjoint by
        // construction (see the flush loop in `client.rs`).
        Ok(())
    }

    fn bump_attempts(&self, id: u64) -> nostos_core::Result<u32> {
        let mut conn = self
            .conn
            .lock()
            .expect("bump_attempts: storage mutex poisoned");
        // Own transaction: the bump is durable before the flush loop reads the
        // returned count. A crash between this and the next flush leaves the
        // bumped count in place — the next flush re-checks `count >= max` and
        // either dead-letters or retries. The count can only increase, so a
        // partial crash never under-counts (a permanently-failing write is
        // never left retrying forever because a bump was lost — the worst case
        // is it takes one extra rejection to reach the threshold).
        let tx = conn.transaction().map_err(rusqlite_err)?;
        let updated = tx
            .execute(
                "UPDATE cairn_outbox SET attempts = attempts + 1 WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(rusqlite_err)?;
        if updated == 0 {
            // Unknown id — a redelivery race after mark_done (the server
            // returned ok:false for an id we already removed), or a genuine
            // bug. Match the trait's default no-op semantics: return 0 so the
            // flush loop's `0 >= max` check is false for any positive max
            // (nothing to retry, nothing to dead-letter). Commit the empty tx
            // (the UPDATE was a no-op) for tidiness.
            tx.commit().map_err(rusqlite_err)?;
            return Ok(0);
        }
        let attempts: i64 = tx
            .query_row(
                "SELECT attempts FROM cairn_outbox WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .map_err(rusqlite_err)?;
        tx.commit().map_err(rusqlite_err)?;
        Ok(u32::try_from(attempts.max(0)).expect("attempts is a small counter"))
    }

    fn mark_dead_letter(&self, id: u64) -> nostos_core::Result<()> {
        let mut conn = self
            .conn
            .lock()
            .expect("mark_dead_letter: storage mutex poisoned");
        // Own transaction: the quarantine is durable. Idempotent — setting
        // dlq=1 on an already-dead-lettered row is a no-op (0 rows affected is
        // not an error, same contract as mark_done). We do NOT delete the row:
        // a dead-letter is an inspectable, replayable state, not data loss.
        // The row leaves the flush loop's view via `pending()`'s `WHERE dlq = 0`.
        let tx = conn.transaction().map_err(rusqlite_err)?;
        tx.execute(
            "UPDATE cairn_outbox SET dlq = 1 WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(rusqlite_err)?;
        tx.commit().map_err(rusqlite_err)?;
        Ok(())
    }

    /// Instant-local write (WS2 slice-2): render the row into `cairn_data` NOW
    /// so the view reflects the user's write before any server round-trip,
    /// WITHOUT advancing the checkpoint (the row isn't server-confirmed). The
    /// server's echo later UPSERTs the authoritative image (reconcile).
    fn apply_local(&mut self, write: &PendingWrite) -> nostos_core::Result<()> {
        let conn = self
            .conn
            .lock()
            .expect("apply_local: storage mutex poisoned");
        match write.op {
            WriteOp::Upsert => {
                // payload_json is the column-named JSON object the Pg path
                // emits — store its UTF-8 bytes as the BLOB so the view's
                // json_extract resolves it identically to a server echo.
                let payload = write.payload_json.as_deref().unwrap_or("null").as_bytes();
                conn.execute(
                    "INSERT OR REPLACE INTO cairn_data (table_name, pk, payload) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![write.table, write.pk, payload],
                )
                .map_err(rusqlite_err)?;
            }
            WriteOp::Delete => {
                conn.execute(
                    "DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2",
                    rusqlite::params![write.table, write.pk],
                )
                .map_err(rusqlite_err)?;
            }
            WriteOp::Patch => {
                // Column-level UPDATE: read the existing row, shallow-merge the
                // patch fields, write back. Without this the optimistic local
                // apply is a silent no-op and the edit stays invisible offline
                // until the server echo (the "patch edits don't render offline"
                // regression — providers/invoices/appointments status edits).
                // The server PATCH path (P3) remains source of truth; this only
                // renders the change immediately. Patching a row not yet in
                // `cairn_data` seeds it from the patch fields alone.
                let patch_json = write.payload_json.as_deref().unwrap_or("{}");
                let existing: Vec<u8> = conn
                    .query_row(
                        "SELECT payload FROM cairn_data \
                         WHERE table_name = ?1 AND pk = ?2",
                        rusqlite::params![write.table, write.pk],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .unwrap_or_default();
                let merged = merge_payload(&existing, patch_json.as_bytes());
                conn.execute(
                    "INSERT OR REPLACE INTO cairn_data (table_name, pk, payload) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![write.table, write.pk, merged],
                )
                .map_err(rusqlite_err)?;
            }
        }
        Ok(())
    }
}

/// Shallow-merge a JSON-object `patch` into an existing JSON-object `payload`
/// (`apply_local` Patch path). Patch fields overwrite existing; fields absent
/// from the patch are preserved. Graceful fallbacks: a missing/non-object
/// existing row merges onto `{}`, a malformed patch is ignored. This is the
/// instant-local optimistic render only — the server echo reconciles the
/// authoritative image on reconnect.
fn merge_payload(existing: &[u8], patch: &[u8]) -> Vec<u8> {
    let mut base: serde_json::Value =
        serde_json::from_slice(existing).unwrap_or_else(|_| serde_json::json!({}));
    let over: serde_json::Value =
        serde_json::from_slice(patch).unwrap_or_else(|_| serde_json::json!({}));
    if let (Some(base_obj), Some(over_obj)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in over_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::to_vec(&base).unwrap_or_else(|_| existing.to_vec())
}

/// Map a `rusqlite::Error` into the backend error variant, stringifying so the
/// engine treats every flavor uniformly as "this batch did not commit."
///
/// Takes `Error` by value deliberately: it's an error-conversion helper used as
/// `map_err(rusqlite_err)` at every SQLite call site (by-value is the natural
/// shape for `map_err`'s closure). Taking a reference would force a closure at
/// every call site for no readability gain.
#[allow(clippy::needless_pass_by_value)]
fn rusqlite_err(e: rusqlite::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

/// Map a wire table name to a safe SQLite view name. Bare `public` names pass
/// through; schema-qualified names (`myschema.tasks`) collapse to
/// `myschema_tasks` (SQLite has no schema-qualified local table here). ponytail:
/// collision risk if two schemas share a table name — add a schema-aware
/// namespace when that's observed.
fn view_name(table: &str) -> String {
    table.replace('.', "_")
}

/// Quote a SQLite identifier (double-quote, doubling embedded quotes) so a
/// catalog name can't break DDL. Names come from PG's catalog (trusted), but
/// this is cheap defense-in-depth against an odd identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a SQL string literal (single-quote, doubling embedded single-quotes).
/// Used to inline a table name into a view definition (SQLite views can't take
/// bind params). Same trusted-catalog + defense-in-depth stance as
/// [`quote_ident`].
fn quote_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Does `cairn_outbox` currently have a column named `needle`? Used by the v1
/// DLQ migration to decide whether `ALTER TABLE ADD COLUMN` is needed without
/// tracking `user_version` (which is ambiguous between a fresh new-schema DB
/// and an old-schema DB — see [`SqliteStorage::migrate_outbox_dlq`]).
///
/// `PRAGMA table_info` returns one row per column; column index 1 is the name.
/// We drain the iterator and look for a match — cheap (the outbox always has a
/// handful of columns) and runs once per open.
fn outbox_has_column(conn: &Connection, needle: &str) -> Result<bool, StorageError> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(cairn_outbox)")
        .map_err(rusqlite_err)?;
    let names: Vec<String> = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            Ok(name)
        })
        .map_err(rusqlite_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(names.iter().any(|n| n == needle))
}

/// Like [`outbox_has_column`] but for `cairn_data` (ADR-0025 slice 4a
/// `applied_lsn` migration probe).
fn cairn_data_has_column(conn: &Connection, needle: &str) -> Result<bool, StorageError> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(cairn_data)")
        .map_err(rusqlite_err)?;
    let names: Vec<String> = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            Ok(name)
        })
        .map_err(rusqlite_err)?
        .filter_map(Result::ok)
        .collect();
    Ok(names.iter().any(|n| n == needle))
}

/// Convert a `rusqlite::types::Value` (the tagged, type-erased SQLite value)
/// into a `serde_json::Value` for [`SqliteStorage::query`]'s result rows.
///
/// `i64` and `String` map losslessly; `f64` maps through
/// `Number::from_f64` (NaN/Inf → `Value::Null`, since JSON has no
/// representation for them — matches `serde_json`'s own f64 serialization).
/// `BLOB` → lowercase-hex `String`, matching the wire payload's hex convention
/// (see `client.rs::decode_hex`); JSON can't carry raw bytes, and hex is
/// lossless + dependency-free (no `base64` crate in the workspace).
fn sqlite_value_to_json(val: rusqlite::types::Value) -> serde_json::Value {
    match val {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(n) => {
            serde_json::Value::Number(serde_json::Number::from(n))
        }
        rusqlite::types::Value::Real(f) => serde_json::Number::from_f64(f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
        rusqlite::types::Value::Blob(b) => {
            // Two hex chars per byte; reuse the same lowercase format the wire
            // path uses so a reader hex-decodes the same way everywhere.
            let mut hex = String::with_capacity(b.len() * 2);
            for byte in b {
                // `write!` would pull in std::fmt; a manual byte→hex table is
                // allocation-free per char and avoids the formatting machinery.
                // `as char` is safe: the table is ASCII hex digits.
                hex.push(NIBBLE_TO_HEX[usize::from(byte >> 4)] as char);
                hex.push(NIBBLE_TO_HEX[usize::from(byte & 0x0F)] as char);
            }
            serde_json::Value::String(hex)
        }
    }
}

/// Lookup table for a single nibble (0–15) → lowercase hex char. Used by
/// [`sqlite_value_to_json`]'s BLOB path to avoid per-byte `format!` allocation
/// (a BLOB column can be large; this keeps the encode constant-time per byte).
const NIBBLE_TO_HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::HashSet;

    fn ins(table: &str, pk: &str, payload: &[u8]) -> RowOp {
        RowOp::Insert {
            table: table.into(),
            pk: pk.into(),
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn fresh_checkpoint_is_zero() {
        let s = SqliteStorage::open_in_memory().unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::ZERO);
    }

    #[test]
    fn apply_inserts_rows_and_advances_checkpoint() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        let ops = [ins("tasks", "1", b"alice"), ins("tasks", "2", b"bob")];
        s.apply_batch(
            &ops.iter().map(|o| (o.clone(), 100)).collect::<Vec<_>>(),
            Lsn::new(100),
            &HashSet::new(),
        )
        .unwrap();

        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));
        // Row count via the same SQLite path.
        let conn = s.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn apply_is_idempotent_reapply_does_not_duplicate() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();
        // Same pk again — must UPSERT, not insert a second row.
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();

        let conn = s.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "idempotent re-apply did not duplicate");
    }

    #[test]
    fn update_overwrites_payload_by_pk() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Update {
                    table: "tasks".into(),
                    pk: "1".into(),
                    payload: Bytes::copy_from_slice(b"v2"),
                },
                20,
            )],
            Lsn::new(20),
            &HashSet::new(),
        )
        .unwrap();

        let conn = s.conn.lock().unwrap();
        let payload: Vec<u8> = conn
            .query_row("SELECT payload FROM cairn_data WHERE pk = '1'", [], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .unwrap();
        assert_eq!(payload, b"v2");
    }

    #[test]
    fn delete_removes_row() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[(ins("tasks", "1", b"x"), 10)],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "1".into(),
                    old_payload: None,
                },
                20,
            )],
            Lsn::new(20),
            &HashSet::new(),
        )
        .unwrap();

        let conn = s.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rows_for_returns_sorted_pk_payload_pairs_scoped_to_table() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[
                (ins("tasks", "2", b"bob"), 10),
                (ins("tasks", "1", b"alice"), 10),
                (ins("notes", "1", b"other-table"), 10),
            ],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();

        let rows = s.rows_for("tasks").unwrap();
        assert_eq!(
            rows,
            vec![
                ("1".to_string(), b"alice".to_vec()),
                ("2".to_string(), b"bob".to_vec()),
            ]
        );
        assert!(s.rows_for("nonexistent").unwrap().is_empty());
    }

    #[test]
    fn rows_for_excludes_deleted_rows() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[(ins("tasks", "1", b"x"), 10)],
            Lsn::new(10),
            &HashSet::new(),
        )
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "1".into(),
                    old_payload: None,
                },
                20,
            )],
            Lsn::new(20),
            &HashSet::new(),
        )
        .unwrap();
        assert!(s.rows_for("tasks").unwrap().is_empty());
    }

    #[test]
    fn checkpoint_is_monotonic_stale_lsn_does_not_regress() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(
            &[(ins("tasks", "1", b"x"), 100)],
            Lsn::new(100),
            &HashSet::new(),
        )
        .unwrap();
        // A replay batch carrying a stale LSN must not move the cursor back.
        s.apply_batch(
            &[(ins("tasks", "2", b"y"), 50)],
            Lsn::new(50),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));
    }

    #[test]
    fn empty_batch_still_advances_checkpoint() {
        // A commit boundary with no rows must still ack the LSN.
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(&[], Lsn::new(42), &HashSet::new()).unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(42));
    }

    // ---- DURABILITY: the property that distinguishes this from InMemoryStorage ----

    #[test]
    fn checkpoint_survives_drop_and_reopen_on_disk() {
        // The whole point of ADR-0016: a restart reads the durable checkpoint
        // and resumes from it rather than re-taking a full snapshot.
        let dir = tempfile_dir();
        let path = format!("{dir}/nostos-durability.sqlite");

        {
            let mut s = SqliteStorage::open(&path).unwrap();
            s.apply_batch(
                &[(ins("tasks", "1", b"durable"), 777)],
                Lsn::new(777),
                &HashSet::new(),
            )
            .unwrap();
            // drop → connection closes, file is flushed to disk.
        }

        // Re-open the SAME file — the checkpoint + row must be there.
        let s2 = SqliteStorage::open(&path).unwrap();
        assert_eq!(s2.checkpoint().unwrap(), Lsn::new(777));
        let conn = s2.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// Build a fresh temp dir for a durability test. Uses stdlib only (no
    /// `tempfile` crate in the workspace) — the dir is leaked intentionally;
    /// tests are short-lived and the OS reclaims on exit.
    fn tempfile_dir() -> String {
        let base = std::env::temp_dir();
        let mut nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
            .to_string();
        nanos.push_str("-nostos-test");
        let dir = base.join(nanos);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.to_string_lossy().into_owned()
    }

    // ---- DEAD-LETTER POLICY (ADR-0013 v2) ----
    //
    // The property that distinguishes this from the pre-DLQ outbox: a
    // permanently-failing write is quarantined after a bounded number of
    // rejections, so the queue head advances and subsequent writes still flush.

    /// A write that always comes back `ok:false` is bumped up to the
    /// configured `dead_letter_max_attempts`, then quarantined. After
    /// quarantine it MUST be excluded from `pending()` (so the queue head
    /// advances) but still inspectable via `dead_letter_entries()`. A
    /// SUBSEQUENT enqueue must still appear in `pending()` — the head is not
    /// blocked by the dead-lettered write. This is the load-bearing property
    /// the DLQ policy exists to provide.
    #[test]
    fn dead_letter_quarantines_permanent_failure_and_unblocks_queue_head() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        let max: u32 = 3; // small threshold so the test doesn't spin

        // Enqueue a write that will "always return ok:false."
        let id1 = s
            .enqueue(PendingWrite {
                table: "tasks".into(),
                op: WriteOp::Upsert,
                pk: "1".into(),
                payload_json: Some(r#"{"title":"fail"}"#.into()),
            })
            .unwrap();
        assert_eq!(s.pending().unwrap().len(), 1);

        // Simulate `max` rejections from the server, mirroring the flush loop's
        // DLQ wiring in `client.rs` (bump on every ok:false; dead-letter once
        // the count reaches the threshold). This is the exact logic the client
        // runs on each `WriteResult{ok:false}`.
        for _ in 0..max {
            let count = s.bump_attempts(id1).unwrap();
            if count >= max {
                s.mark_dead_letter(id1).unwrap();
            }
        }

        // The dead-lettered write is excluded from pending()...
        assert!(
            s.pending().unwrap().is_empty(),
            "dead-lettered write must not appear in pending()"
        );
        // ...but is inspectable via dead_letter_entries() (NOT deleted).
        let dlq = s.dead_letter_entries().unwrap();
        assert_eq!(dlq.len(), 1, "exactly one dead-lettered write");
        assert_eq!(dlq[0].0, id1);
        assert_eq!(dlq[0].1.pk, "1");

        // A SUBSEQUENT enqueue must still flush — the queue head is not
        // blocked by the quarantined write. This is the regression the DLQ
        // policy prevents.
        let id2 = s
            .enqueue(PendingWrite {
                table: "tasks".into(),
                op: WriteOp::Upsert,
                pk: "2".into(),
                payload_json: Some(r#"{"title":"ok"}"#.into()),
            })
            .unwrap();
        let pending = s.pending().unwrap();
        assert_eq!(pending.len(), 1, "head advanced past the dead-letter");
        assert_eq!(pending[0].0, id2, "the new write is at the head");
    }

    /// Re-opening a pre-DLQ database (the old `cairn_outbox` schema without the
    /// `attempts` / `dlq` columns) MUST migrate it forward idempotently without
    /// losing legacy rows. This is the upgrade path for existing deployments —
    /// a user's device has an old SQLite file the day they install the new
    /// binary, and the migration runs on the first open.
    #[test]
    fn migrate_outbox_dlq_adds_columns_to_legacy_database_idempotently() {
        let dir = tempfile_dir();
        let path = format!("{dir}/nostos-migrate.sqlite");

        // Phase 1: hand-craft a database with the OLD outbox schema (no
        // attempts/dlq columns), simulating a file written by a pre-DLQ binary.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cairn_outbox (\
                    id INTEGER PRIMARY KEY AUTOINCREMENT,\
                    table_name TEXT NOT NULL,\
                    op TEXT NOT NULL,\
                    pk TEXT NOT NULL,\
                    payload TEXT\
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cairn_outbox (table_name, op, pk, payload) VALUES ('t', 'upsert', '1', '{}')",
                [],
            )
            .unwrap();
        }

        // Phase 2: reopen via SqliteStorage — migrate_outbox_dlq runs on init
        // and ALTERs the table. The legacy row MUST survive (ALTER ADD COLUMN
        // with a default doesn't rewrite rows — the default is schema-level).
        let s = SqliteStorage::open(&path).unwrap();
        // bump_attempts reads the `attempts` column — would error if missing.
        let count = s.bump_attempts(1).unwrap();
        assert_eq!(
            count, 1,
            "legacy row preserved + attempts column added by the migration"
        );
        // pending() filters on `dlq` — would error if the column were missing.
        assert_eq!(s.pending().unwrap().len(), 1, "legacy row survived");

        // Phase 3: re-open is idempotent (the column probe short-circuits).
        drop(s);
        let _s2 = SqliteStorage::open(&path)
            .expect("re-opening an already-migrated DB is a no-op (idempotent)");
    }

    // ---- QUERY SURFACE (P1-Rust) ----
    //
    // Prove the bundled SQLite ships JSON1 and that an arbitrary SELECT against
    // `cairn_data` returns correctly-typed, column-keyed JSON maps.

    /// `query()` runs an arbitrary SELECT against `cairn_data`, and the bundled
    /// SQLite's JSON1 lets the dev `json_extract` straight out of the opaque
    /// payload BLOB. This is the PowerSync-parity read surface: a Flutter
    /// `watch(sql)` can run any SELECT and render the result set directly,
    /// without a column decoder or a parallel query engine.
    #[test]
    fn query_runs_select_with_json1_against_opaque_payload() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        // Payload is a JSON object — the shape a JSON-backed source delivers
        // and what json_extract operates on. Stored as opaque bytes (BLOB).
        let payload = br#"{"title":"hello","n":42}"#;
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "t1".into(),
                    pk: "1".into(),
                    payload: Bytes::copy_from_slice(payload),
                },
                1,
            )],
            Lsn::new(1),
            &HashSet::new(),
        )
        .unwrap();

        let rows = s
            .query(
                "SELECT pk, \
                 json_extract(payload, '$.title') AS title, \
                 json_extract(payload, '$.n') AS n \
                 FROM cairn_data WHERE table_name = 't1'",
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        // Column names → map keys. Types: TEXT → String, INTEGER extract → i64.
        assert_eq!(row.get("pk").and_then(serde_json::Value::as_str), Some("1"));
        assert_eq!(
            row.get("title").and_then(serde_json::Value::as_str),
            Some("hello")
        );
        assert_eq!(row.get("n").and_then(serde_json::Value::as_i64), Some(42));
    }

    /// WS2 read foundation: `apply_schema` materializes a `VIEW` per table over
    /// the opaque `cairn_data` BLOB, so `SELECT col FROM <table>` returns typed
    /// values — PowerSync-style read DX WITHOUT materialized typed tables. This
    /// is the load-bearing claim of slice-1, asserted end-to-end.
    #[test]
    fn apply_schema_creates_queryable_view_over_opaque_payload() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into(), "completed".into()],
        }])
        .unwrap();
        // Payload is the column-named JSON object the Pg path emits
        // (tuple_to_json_payload keyed by column name).
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "tasks".into(),
                    pk: "t1".into(),
                    payload: Bytes::copy_from_slice(
                        br#"{"id":"t1","title":"buy milk","completed":false}"#,
                    ),
                },
                1,
            )],
            Lsn::new(1),
            &HashSet::new(),
        )
        .unwrap();

        // The dev's natural SQL resolves against the `tasks` VIEW, not the raw
        // `cairn_data` BLOB — and the values come back typed (str/bool), not as
        // opaque bytes.
        let rows = s.query("SELECT id, title, completed FROM tasks").unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.get("id").and_then(serde_json::Value::as_str), Some("t1"));
        assert_eq!(
            r.get("title").and_then(serde_json::Value::as_str),
            Some("buy milk")
        );
        // SQLite models JSON booleans as INTEGER 0/1 (no native bool type —
        // same as PowerSync's SQLite layer). The Dart API (WS3) maps 0/1 ↔ bool.
        assert_eq!(
            r.get("completed").and_then(serde_json::Value::as_i64),
            Some(0)
        );

        // The view carries the replication key as `_pk` (matches the subscribe
        // row-stream's convention) ÔÇö write-back (delete/edit) keys on it, and
        // `SELECT * FROM tasks` exposes it for the PowerSync-style DX.
        let pk_rows = s.query("SELECT _pk FROM tasks").unwrap();
        assert_eq!(
            pk_rows[0].get("_pk").and_then(serde_json::Value::as_str),
            Some("t1")
        );
        let star = s.query("SELECT * FROM tasks").unwrap();
        assert!(
            star[0].get("_pk").is_some(),
            "SELECT * FROM <view> must carry _pk for write-back"
        );

        // Idempotent: re-applying the SAME schema must not error (views are
        // dropped + recreated, so this is a no-op from the caller's view).
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into(), "completed".into()],
        }])
        .unwrap();
        let rows = s.query("SELECT title FROM tasks").unwrap();
        assert_eq!(
            rows[0].get("title").and_then(serde_json::Value::as_str),
            Some("buy milk")
        );

        // A DELETE on cairn_data propagates through the view (the view is live,
        // not a snapshot).
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "t1".into(),
                    old_payload: None,
                },
                2,
            )],
            Lsn::new(2),
            &HashSet::new(),
        )
        .unwrap();
        assert!(s.query("SELECT id FROM tasks").unwrap().is_empty());
    }

    /// Schema migration: re-applying a CHANGED schema (added column) must
    /// refresh the view — `apply_schema` drops + recreates each table view, so
    /// the Flutter app's simple migration path (bump schema, reconnect) works
    /// without a manual DROP. This would fail with the old
    /// `CREATE VIEW IF NOT EXISTS` behavior (stale column list kept).
    #[test]
    fn apply_schema_migration_refreshes_view_columns() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        // v1 schema: no `due` column.
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into()],
        }])
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "tasks".into(),
                    pk: "t1".into(),
                    payload: Bytes::copy_from_slice(
                        br#"{"id":"t1","title":"buy milk","due":"2026-03-01"}"#,
                    ),
                },
                1,
            )],
            Lsn::new(1),
            &HashSet::new(),
        )
        .unwrap();
        // v1 view does not expose `due`.
        assert!(s.query("SELECT due FROM tasks").is_err());

        // v2 schema: `due` added. Same data, no re-sync needed — the payload
        // already carried the column; only the view over it changes.
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into(), "due".into()],
        }])
        .unwrap();
        let rows = s.query("SELECT id, title, due FROM tasks").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("due").and_then(serde_json::Value::as_str),
            Some("2026-03-01"),
            "migrated view must expose the new column from existing payloads"
        );

        // v3 schema: column REMOVED. The view must drop it (not error, not
        // keep serving the stale column).
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into()],
        }])
        .unwrap();
        assert!(
            s.query("SELECT due FROM tasks").is_err(),
            "removed column must disappear from the recreated view"
        );
        assert_eq!(s.query("SELECT id FROM tasks").unwrap().len(), 1);
    }

    /// WS2 slice-2: an instant-local write (`Outbox::apply_local`) renders the
    /// row in the view IMMEDIATELY — before any server round-trip — and the
    /// checkpoint does NOT advance (the row isn't server-confirmed). The
    /// server's echo (`apply_batch`) then UPSERTs the authoritative image
    /// (reconcile, last-writer-wins). This is the load-bearing claim of slice-2.
    #[test]
    fn apply_local_renders_instantly_and_echo_reconciles() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_schema(&[ClientTable {
            name: "tasks".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "title".into()],
        }])
        .unwrap();
        let checkpoint_before = s.checkpoint().unwrap();

        // The user writes — instant-local: visible in the view NOW, offline.
        s.apply_local(&PendingWrite {
            table: "tasks".into(),
            op: WriteOp::Upsert,
            pk: "t1".into(),
            payload_json: Some(r#"{"id":"t1","title":"optimistic"}"#.into()),
        })
        .unwrap();
        let rows = s.query("SELECT title FROM tasks").unwrap();
        assert_eq!(
            rows[0].get("title").and_then(serde_json::Value::as_str),
            Some("optimistic")
        );
        // The checkpoint did NOT move — the row is the user's intent, not a
        // server-confirmed replication event (moving it here would break resume).
        assert_eq!(s.checkpoint().unwrap(), checkpoint_before);

        // The server's echo arrives with the authoritative image — UPSERT wins.
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "tasks".into(),
                    pk: "t1".into(),
                    payload: Bytes::copy_from_slice(br#"{"id":"t1","title":"authoritative"}"#),
                },
                100,
            )],
            Lsn::new(100),
            &HashSet::new(),
        )
        .unwrap();
        let rows = s.query("SELECT title FROM tasks").unwrap();
        assert_eq!(
            rows[0].get("title").and_then(serde_json::Value::as_str),
            Some("authoritative")
        );
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));

        // An instant-local DELETE removes it from the view before the echo too.
        s.apply_local(&PendingWrite {
            table: "tasks".into(),
            op: WriteOp::Delete,
            pk: "t1".into(),
            payload_json: None,
        })
        .unwrap();
        assert!(s.query("SELECT id FROM tasks").unwrap().is_empty());
    }

    /// Regression: `apply_local` with `WriteOp::Patch` must shallow-merge the
    /// patch fields into the existing row and render immediately offline (the
    /// providers/invoices/appointments status-edit path). Previously the Patch
    /// branch was a no-op stub, so a status edit made offline stayed invisible
    /// until the server echo — a silent local-apply gap masquerading as
    /// "sync only works when wifi is back".
    #[test]
    fn apply_local_patch_merges_fields_and_renders_offline() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_schema(&[ClientTable {
            name: "providers".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "name".into(), "status".into()],
        }])
        .unwrap();

        // Seed an existing provider the way a server echo would.
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "providers".into(),
                    pk: "p1".into(),
                    payload: Bytes::copy_from_slice(
                        br#"{"id":"p1","name":"Ada","status":"pending"}"#,
                    ),
                },
                1,
            )],
            Lsn::new(1),
            &HashSet::new(),
        )
        .unwrap();

        // User flips the status OFFLINE — a Patch carrying only the changed col.
        s.apply_local(&PendingWrite {
            table: "providers".into(),
            op: WriteOp::Patch,
            pk: "p1".into(),
            payload_json: Some(r#"{"status":"active"}"#.into()),
        })
        .unwrap();

        // The view reflects the patched status AND preserves untouched `name`
        // (shallow merge — patch overwrites listed fields only).
        let rows = s.query("SELECT name, status FROM providers").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("name").and_then(serde_json::Value::as_str),
            Some("Ada"),
            "patch must preserve fields absent from the patch payload"
        );
        assert_eq!(
            rows[0].get("status").and_then(serde_json::Value::as_str),
            Some("active"),
            "patched field must render immediately offline"
        );
    }

    /// Phase-1 reconnect-glitch fix (Piece B): `apply_batch` MUST replay pending
    /// optimistic writes on top of the incoming server batch, so a reconnect
    /// snapshot/stream that lacks the un-flushed local edits can't flash the
    /// stale server image (deleted rows reappearing, modified rows reverting).
    /// Once the outbox flush is acked (`mark_done`), the replay stops and the
    /// server's authoritative echo wins. See
    /// `docs/plans/reconnect-glitch-fix-2026-07-19.md`.
    #[test]
    fn apply_batch_replays_pending_optimistic_writes_no_reconnect_flash() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_schema(&[ClientTable {
            name: "providers".into(),
            primary_key: vec!["id".into()],
            columns: vec!["id".into(), "status".into()],
        }])
        .unwrap();

        // Server has provider p1 = pending.
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "providers".into(),
                    pk: "p1".into(),
                    payload: Bytes::copy_from_slice(br#"{"id":"p1","status":"pending"}"#),
                },
                1,
            )],
            Lsn::new(1),
            &HashSet::new(),
        )
        .unwrap();

        // User flips status to 'active' offline: enqueue (outbox) + apply_local.
        let write = PendingWrite {
            table: "providers".into(),
            op: WriteOp::Patch,
            pk: "p1".into(),
            payload_json: Some(r#"{"status":"active"}"#.into()),
        };
        s.enqueue(write.clone()).unwrap();
        s.apply_local(&write).unwrap();
        assert_eq!(
            s.query("SELECT status FROM providers").unwrap()[0]
                .get("status")
                .and_then(serde_json::Value::as_str),
            Some("active"),
        );

        // Reconnect: a server batch lands WITHOUT the local edit (outbox hasn't
        // flushed) — server still says 'pending'. Pre-fix this clobbered
        // 'active' → the flash.
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "providers".into(),
                    pk: "p1".into(),
                    payload: Bytes::copy_from_slice(br#"{"id":"p1","status":"pending"}"#),
                },
                2,
            )],
            Lsn::new(2),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            s.query("SELECT status FROM providers").unwrap()[0]
                .get("status")
                .and_then(serde_json::Value::as_str),
            Some("active"),
            "apply_batch must replay pending optimistic writes on top — no reconnect flash",
        );

        // The outbox flush is acked → the write leaves pending → no more replay.
        let id = s.pending().unwrap()[0].0;
        s.mark_done(id).unwrap();
        s.apply_batch(
            &[(
                RowOp::Insert {
                    table: "providers".into(),
                    pk: "p1".into(),
                    payload: Bytes::copy_from_slice(br#"{"id":"p1","status":"authoritative"}"#),
                },
                3,
            )],
            Lsn::new(3),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            s.query("SELECT status FROM providers").unwrap()[0]
                .get("status")
                .and_then(serde_json::Value::as_str),
            Some("authoritative"),
            "after the outbox drains, apply_batch stops replaying — server wins",
        );
    }
}
