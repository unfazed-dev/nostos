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

use crate::{Outbox, PendingWrite, Storage, StorageError, WriteOp};

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

    /// Enumerate the `(pk, payload_bytes)` pairs the store holds for `table`,
    /// sorted by pk (BTreeMap iteration is already sorted; this preserves it).
    ///
    /// A diagnostic accessor — NOT part of the [`Storage`] trait (the trait
    /// stays minimal: `checkpoint` + `apply_batch`). Exists so the WASM FFI
    /// (`nostos-ffi-wasm::NostosEngine`) and the browser demo can render the
    /// engine's *current state* without re-implementing the apply path. Deletes
    /// are naturally excluded (they `remove` the row, so the pk is absent).
    #[must_use]
    pub fn rows_for(&self, table: &str) -> Vec<(String, Vec<u8>)> {
        // `range((table, "")..)` would be cleaner, but it can't express "all
        // keys whose first element == table" without a sentinel upper bound
        // (and `(table, \u{10FFFF})` is ugly). The `BTreeMap` is small (it's a
        // single client's view), so a filtered scan is fine and obvious.
        self.rows
            .iter()
            .filter(|((t, _), _)| t == table)
            .map(|((_, pk), bytes)| (pk.clone(), bytes.clone()))
            .collect()
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

    fn pks_for_table(&self, table: &str) -> crate::Result<Vec<String>> {
        // Same filtered-scan approach as `rows_for` — the BTreeMap is small
        // (single client view), so a linear scan is fine and obvious. Returns
        // the PKs in BTreeMap iteration order (sorted), which keeps
        // snapshot-reconcile deterministic in tests.
        Ok(self
            .rows
            .iter()
            .filter(|((t, _), _)| t == table)
            .map(|((_, pk), _)| pk.clone())
            .collect())
    }

    fn delete_pks(&mut self, table: &str, pks: &[String]) -> crate::Result<()> {
        // Bulk-remove: each pk is a direct BTreeMap key. Idempotent — removing
        // an absent key is a no-op. No shadow copy here (unlike `apply_batch`)
        // because there's no atomicity-with-checkpoint contract on this path;
        // the reconcile is a standalone op.
        for pk in pks {
            self.rows.remove(&(table.to_owned(), pk.clone()));
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

    fn apply_local(&mut self, write: &PendingWrite) -> crate::Result<()> {
        // WS2 slice-2: render the row into the data map now (optimistic), with
        // NO checkpoint advance — the row is the user's intent, not yet a
        // server-confirmed replication event. The echo's apply_batch reconciles.
        match write.op {
            WriteOp::Upsert => {
                let payload = write
                    .payload_json
                    .as_deref()
                    .unwrap_or("null")
                    .as_bytes()
                    .to_vec();
                self.rows
                    .insert((write.table.clone(), write.pk.clone()), payload);
            }
            WriteOp::Delete => {
                self.rows.remove(&(write.table.clone(), write.pk.clone()));
            }
            WriteOp::Patch => {
                // Partial-column; the server PATCH path (P3) is source of truth.
                // ponytail: instant-local patch needs a read-merge-write; defer
                // until a client issues one (demo + Supabase use upsert/delete).
            }
        }
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

    #[test]
    fn rows_for_returns_inserted_rows_in_pk_order() {
        // The readback accessor the WASM FFI + demo render from. It must return
        // the (pk, payload) pairs for the table, sorted by pk (BTreeMap order),
        // and exclude other tables.
        let mut s = InMemoryStorage::new();
        // Insert out of pk order — the accessor must still hand back sorted.
        let ops = [
            ins("tasks", "2", b"bob"),
            ins("tasks", "1", b"alice"),
            ins("users", "9", b"carol"), // different table — must be excluded
        ];
        s.apply_batch(&ops, Lsn::new(10)).unwrap();

        let rows = s.rows_for("tasks");
        assert_eq!(
            rows,
            vec![
                ("1".to_string(), b"alice".to_vec()),
                ("2".to_string(), b"bob".to_vec()),
            ],
            "sorted by pk, excludes other tables"
        );

        // A table with no rows yields an empty Vec (not an error).
        assert!(s.rows_for("absent").is_empty());
    }

    #[test]
    fn rows_for_excludes_deleted_rows() {
        // A delete `remove`s the row from the BTreeMap, so the enumeration must
        // no longer surface it — the readback reflects the engine's *current*
        // state, not its history.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[ins("tasks", "1", b"keep"), ins("tasks", "2", b"drop")],
            Lsn::new(10),
        )
        .unwrap();
        s.apply_batch(
            &[RowOp::Delete {
                table: "tasks".into(),
                pk: "2".into(),
            }],
            Lsn::new(20),
        )
        .unwrap();

        let rows = s.rows_for("tasks");
        assert_eq!(rows, vec![("1".to_string(), b"keep".to_vec())]);
    }

    #[test]
    fn rows_for_reflects_update_in_place() {
        // An update overwrites the payload by pk; the enumeration must show the
        // latest bytes, not the original insert.
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

        let rows = s.rows_for("tasks");
        assert_eq!(rows, vec![("1".to_string(), b"v2".to_vec())]);
    }
}
