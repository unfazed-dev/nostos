//! `SqliteStorage` — the real durable backend for the native client.
//!
//! Implements [`nostos_core::Storage`] over `rusqlite` (workspace `bundled`
//! feature, so this crate brings its own SQLite binary — zero external deps to
//! run). Rows are stored as opaque payload bytes keyed by `(table, pk)`; the
//! LSN checkpoint lives in a `cairn_meta` row.
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

use std::sync::Mutex;

use nostos_core::{Storage, StorageError};
use nostos_domain::{Lsn, RowOp};
use rusqlite::Connection;

/// The opaque-bytes row table. One row per `(table, pk)`.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS cairn_data (\
    table_name TEXT NOT NULL,\
    pk TEXT NOT NULL,\
    payload BLOB NOT NULL,\
    PRIMARY KEY (table_name, pk)\
);\
CREATE TABLE IF NOT EXISTS cairn_meta (\
    key TEXT PRIMARY KEY,\
    value TEXT NOT NULL\
);\
INSERT OR IGNORE INTO cairn_meta (key, value) VALUES ('checkpoint', '0');\
";

/// The single meta key holding the last-applied LSN (`u64` decimal).
const CHECKPOINT_KEY: &str = "checkpoint";

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
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

    fn apply_batch(&mut self, ops: &[RowOp], checkpoint: Lsn) -> nostos_core::Result<()> {
        let mut conn = self
            .conn
            .lock()
            .expect("apply_batch: storage mutex poisoned");
        let tx = conn.transaction().map_err(rusqlite_err)?;

        {
            let mut upsert = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO cairn_data (table_name, pk, payload) VALUES (?1, ?2, ?3)",
                )
                .map_err(rusqlite_err)?;
            let mut delete = tx
                .prepare_cached("DELETE FROM cairn_data WHERE table_name = ?1 AND pk = ?2")
                .map_err(rusqlite_err)?;

            for op in ops {
                match op {
                    RowOp::Insert { table, pk, payload } | RowOp::Update { table, pk, payload } => {
                        upsert
                            .execute(rusqlite::params![table, pk, payload.as_ref()])
                            .map_err(rusqlite_err)?;
                    }
                    RowOp::Delete { table, pk } => {
                        delete
                            .execute(rusqlite::params![table, pk])
                            .map_err(rusqlite_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

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
        s.apply_batch(&ops, Lsn::new(100)).unwrap();

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
        s.apply_batch(&[ins("tasks", "1", b"v1")], Lsn::new(10))
            .unwrap();
        // Same pk again — must UPSERT, not insert a second row.
        s.apply_batch(&[ins("tasks", "1", b"v1")], Lsn::new(10))
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
        s.apply_batch(&[ins("tasks", "1", b"v1")], Lsn::new(10))
            .unwrap();
        s.apply_batch(
            &[RowOp::Update {
                table: "tasks".into(),
                pk: "1".into(),
                payload: Bytes::copy_from_slice(b"v2"),
            }],
            Lsn::new(20),
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
        s.apply_batch(&[ins("tasks", "1", b"x")], Lsn::new(10))
            .unwrap();
        s.apply_batch(
            &[RowOp::Delete {
                table: "tasks".into(),
                pk: "1".into(),
            }],
            Lsn::new(20),
        )
        .unwrap();

        let conn = s.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cairn_data", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn checkpoint_is_monotonic_stale_lsn_does_not_regress() {
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(&[ins("tasks", "1", b"x")], Lsn::new(100))
            .unwrap();
        // A replay batch carrying a stale LSN must not move the cursor back.
        s.apply_batch(&[ins("tasks", "2", b"y")], Lsn::new(50))
            .unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));
    }

    #[test]
    fn empty_batch_still_advances_checkpoint() {
        // A commit boundary with no rows must still ack the LSN.
        let mut s = SqliteStorage::open_in_memory().unwrap();
        s.apply_batch(&[], Lsn::new(42)).unwrap();
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
            s.apply_batch(&[ins("tasks", "1", b"durable")], Lsn::new(777))
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
}
