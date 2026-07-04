//! In-memory `Storage` + `Outbox` — the test double and the executable contract
//! reference.
//!
//! This exists so the apply engine (and every unit test) can be exercised
//! without a SQLite build. The [`InMemoryStorage`] implements [`crate::Storage`]
//! AND [`crate::Outbox`] with the exact semantics the traits document: atomic
//! batch apply (all rows + the checkpoint move together), idempotent upsert-by-pk,
//! monotonic LSN; and a monotonic-id write queue that mirrors `cairn_outbox`.
//!
//! The data model mirrors what `SqliteStorage` will persist: a row keyed by
//! `(table, pk)` holding the opaque payload bytes, plus a single checkpoint LSN.

use std::collections::BTreeMap;

use nostos_domain::{Lsn, RowOp};

use crate::{Outbox, PendingWrite, Storage, StorageError};

/// An in-memory store: rows keyed by `(table, pk)`, plus the durable checkpoint
/// and a write outbox.
///
/// "Durable" here means "survives the engine's apply loop" — it does NOT survive
/// a process crash (there's no disk). It is the reference for the trait contract
/// and the backing store for unit tests; `SqliteStorage` adds real durability.
#[derive(Debug, Default)]
pub struct InMemoryStorage {
    rows: BTreeMap<(String, String), Vec<u8>>,
    checkpoint: Lsn,
    /// The write outbox: `(id, PendingWrite)` pairs, oldest first. The next id
    /// to assign is `next_write_id` (monotonic, mirrors AUTOINCREMENT).
    outbox: BTreeMap<u64, PendingWrite>,
    next_write_id: u64,
}

impl InMemoryStorage {
    /// A fresh store at LSN zero (the client will take a full snapshot).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read back a row's opaque payload (for test assertions).
    #[must_use]
    pub fn payload(&self, table: &str, pk: &str) -> Option<&[u8]> {
        self.rows
            .get(&(table.to_owned(), pk.to_owned()))
            .map(Vec::as_slice)
    }

    /// How many rows the store holds (for test assertions).
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// How many writes are queued in the outbox (for test assertions).
    #[must_use]
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }
}

impl Storage for InMemoryStorage {
    fn checkpoint(&self) -> crate::Result<Lsn> {
        Ok(self.checkpoint)
    }

    fn apply_batch(&mut self, ops: &[RowOp], checkpoint: Lsn) -> crate::Result<()> {
        // Atomicity: mutate a shadow copy, swap in only if every op succeeded.
        // (For the in-memory impl no op can fail, but the structure documents
        // the contract that SqliteStorage enforces with a real transaction.)
        let mut shadow = self.rows.clone();
        for op in ops {
            match op {
                RowOp::Insert { table, pk, payload } | RowOp::Update { table, pk, payload } => {
                    shadow.insert((table.clone(), pk.clone()), payload.as_ref().to_vec());
                }
                RowOp::Delete { table, pk } => {
                    shadow.remove(&(table.clone(), pk.clone()));
                }
            }
        }
        self.rows = shadow;
        // Monotonic: never move the checkpoint backward.
        if checkpoint > self.checkpoint {
            self.checkpoint = checkpoint;
        }
        Ok(())
    }
}

impl Outbox for InMemoryStorage {
    fn enqueue(&mut self, write: PendingWrite) -> crate::Result<u64> {
        // Monotonic id (never reused — mirrors `AUTOINCREMENT` semantics).
        self.next_write_id = self
            .next_write_id
            .checked_add(1)
            .expect("write id space exhausted");
        let id = self.next_write_id;
        self.outbox.insert(id, write);
        Ok(id)
    }

    fn pending(&self) -> crate::Result<Vec<(u64, PendingWrite)>> {
        // BTreeMap iterates in ascending key order → oldest first, as the
        // contract requires.
        Ok(self
            .outbox
            .iter()
            .map(|(&id, pw)| (id, pw.clone()))
            .collect())
    }

    fn mark_done(&mut self, id: u64) -> crate::Result<()> {
        // Idempotent: removing an unknown id is a no-op (BTreeMap::remove
        // returns Option, not an error).
        self.outbox.remove(&id);
        Ok(())
    }
}

// `Storage` never returns an error from the in-memory impl; the error arm exists
// so tests can assert the engine's *behavior* on a backend failure. A test-only
// failing store is trivially constructable by wrapping this in something that
// returns `Err` unconditionally — no need to bloat the public surface here.
#[allow(dead_code)]
fn _storage_error_is_reachable() -> StorageError {
    StorageError::Backend("unreachable in InMemoryStorage".into())
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
        let s = InMemoryStorage::new();
        assert_eq!(s.checkpoint().unwrap(), Lsn::ZERO);
        assert_eq!(s.row_count(), 0);
    }

    #[test]
    fn apply_inserts_rows_and_advances_checkpoint() {
        let mut s = InMemoryStorage::new();
        let ops = [ins("tasks", "1", b"alice"), ins("tasks", "2", b"bob")];
        s.apply_batch(&ops, Lsn::new(100)).unwrap();

        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));
        assert_eq!(s.row_count(), 2);
        assert_eq!(s.payload("tasks", "1"), Some(b"alice" as &[u8]));
        assert_eq!(s.payload("tasks", "2"), Some(b"bob" as &[u8]));
    }

    #[test]
    fn apply_is_idempotent_reapply_is_noop_equivalent() {
        // The core exactly-once property at the apply layer: re-applying the
        // same RowOp (same table+pk) overwrites with the same bytes — no row
        // count bloat, no duplicate. Last-writer-wins by WAL order (ADR-0014 a).
        let mut s = InMemoryStorage::new();

        s.apply_batch(&[ins("tasks", "1", b"v1")], Lsn::new(10))
            .unwrap();
        // Re-apply the SAME op (same table+pk) — must UPSERT, not insert a copy.
        s.apply_batch(&[ins("tasks", "1", b"v1")], Lsn::new(10))
            .unwrap();

        assert_eq!(s.row_count(), 1, "no duplicate row");
        assert_eq!(s.payload("tasks", "1"), Some(b"v1" as &[u8]));
    }

    #[test]
    fn update_overwrites_payload_by_pk() {
        let mut s = InMemoryStorage::new();
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

        assert_eq!(s.row_count(), 1);
        assert_eq!(s.payload("tasks", "1"), Some(b"v2" as &[u8]));
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(20));
    }

    #[test]
    fn delete_removes_row() {
        let mut s = InMemoryStorage::new();
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

        assert_eq!(s.row_count(), 0);
        assert!(s.payload("tasks", "1").is_none());
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(20));
    }

    #[test]
    fn checkpoint_is_monotonic_lower_lsn_does_not_regress() {
        let mut s = InMemoryStorage::new();
        s.apply_batch(&[ins("tasks", "1", b"x")], Lsn::new(100))
            .unwrap();
        // A late-arriving batch with a stale LSN must NOT drag the checkpoint back.
        s.apply_batch(&[ins("tasks", "2", b"y")], Lsn::new(50))
            .unwrap();

        assert_eq!(
            s.checkpoint().unwrap(),
            Lsn::new(100),
            "checkpoint never regresses"
        );
        // …but the row still applies (monotonicity is about the checkpoint, not the data).
        assert_eq!(s.row_count(), 2);
    }

    #[test]
    fn empty_batch_advances_checkpoint_only() {
        // A transaction boundary (commit) with no row ops should still move the
        // checkpoint — the client acks the commit LSN even if it carried no rows.
        let mut s = InMemoryStorage::new();
        s.apply_batch(&[], Lsn::new(42)).unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(42));
        assert_eq!(s.row_count(), 0);
    }

    #[test]
    fn delete_of_missing_row_is_a_noop() {
        // Idempotency on the delete path: deleting a pk that isn't there must
        // not error and must not change row count.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[RowOp::Delete {
                table: "tasks".into(),
                pk: "never-existed".into(),
            }],
            Lsn::new(5),
        )
        .unwrap();
        assert_eq!(s.row_count(), 0);
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(5));
    }
}
