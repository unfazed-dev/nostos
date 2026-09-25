//! In-memory `Storage` + `Outbox` — the test double and the executable contract
//! reference.
//!
//! This exists so the apply engine (and every unit test) can be exercised
//! without a SQLite build. The [`InMemoryStorage`] implements [`crate::Storage`]
//! AND [`crate::Outbox`] with the exact semantics the traits document: atomic
//! batch apply (all rows + the checkpoint move together), idempotent upsert-by-pk,
//! monotonic LSN; and a monotonic-id write queue that mirrors `nostos_outbox`.
//!
//! The data model mirrors what `SqliteStorage` will persist: a row keyed by
//! `(table, pk)` holding the opaque payload bytes, plus a single checkpoint LSN.

use std::collections::{BTreeMap, HashSet};

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
    /// `(table, pk)` → `(payload, applied_lsn)`. The applied_lsn drives per-row
    /// gating (ADR-0025 slice 4a): a stale op (lsn < applied_lsn) is skipped.
    rows: BTreeMap<(String, String), (Vec<u8>, u64)>,
    checkpoint: Lsn,
    /// The write outbox: `(id, PendingWrite)` pairs, oldest first. The next id
    /// to assign is `next_write_id` (monotonic, mirrors AUTOINCREMENT).
    outbox: BTreeMap<u64, PendingWrite>,
    next_write_id: u64,
    /// Tables whose payload is an add-wins OR-set (ADR-0030): applies MERGE
    /// element-wise by HLC instead of clobbering. Empty by default — the apply
    /// path is unchanged for ordinary tables (the bench's `tasks` included), so
    /// the measured fan-out path is unaffected.
    or_set_tables: HashSet<String>,
    /// Tables whose payload is a PN-Counter CRDT (ADR-0030 addendum): applies
    /// MERGE per-replica elementwise max. Empty by default.
    counter_tables: HashSet<String>,
    /// Direct mode's `xid8` snapshot horizon, opaque text (`crate::pull`).
    /// `None` = fresh. Overridden rather than left on the trait default so a
    /// direct-mode test exercises a horizon that actually persists.
    horizon: Option<String>,
}

impl InMemoryStorage {
    /// A fresh store at LSN zero (the client will take a full snapshot).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare which tables hold add-wins OR-sets (ADR-0030). For those tables
    /// `apply_batch` / `apply_local` merge element-wise by HLC; all others stay
    /// last-writer-wins. Builder-style.
    #[must_use]
    pub fn with_or_set_tables(mut self, tables: HashSet<String>) -> Self {
        self.or_set_tables = tables;
        self
    }

    /// Declare which tables hold PN-Counter CRDTs (ADR-0030 addendum). For those
    /// tables `apply_batch` / `apply_local` merge per-replica elementwise max;
    /// all others stay last-writer-wins (or OR-set if tagged). Builder-style.
    #[must_use]
    pub fn with_counter_tables(mut self, tables: HashSet<String>) -> Self {
        self.counter_tables = tables;
        self
    }

    /// Set the OR-set tables post-construction (Wave 4a: the wasm engine's
    /// `set_crdt_tables` needs a setter, not a builder, because the engine is
    /// already constructed when the JS caller configures CRDT tables).
    pub fn set_or_set_tables(&mut self, tables: HashSet<String>) {
        self.or_set_tables = tables;
    }

    /// Set the counter tables post-construction (Wave 4a: same rationale as
    /// [`Self::set_or_set_tables`]).
    pub fn set_counter_tables(&mut self, tables: HashSet<String>) {
        self.counter_tables = tables;
    }

    /// Read back a row's opaque payload (for test assertions).
    #[must_use]
    pub fn payload(&self, table: &str, pk: &str) -> Option<&[u8]> {
        self.rows
            .get(&(table.to_owned(), pk.to_owned()))
            .map(|(bytes, _)| bytes.as_slice())
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
            .map(|((_, pk), (bytes, _))| (pk.clone(), bytes.clone()))
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

    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &std::collections::HashSet<String>,
    ) -> crate::Result<()> {
        // Atomicity: mutate a shadow copy, swap in only if every op succeeded.
        // (For the in-memory impl no op can fail, but the structure documents
        // the contract that SqliteStorage enforces with a real transaction.)
        let mut shadow = self.rows.clone();
        for (op, lsn) in ops {
            match op {
                RowOp::Insert { table, pk, payload } | RowOp::Update { table, pk, payload } => {
                    let uncond = snapshot_tables.contains(table);
                    let admit = uncond
                        || shadow
                            .get(&(table.clone(), pk.clone()))
                            .is_none_or(|(_, prev)| *lsn >= *prev);
                    if admit {
                        // OR-set tables merge element-wise by HLC (ADR-0030) so
                        // concurrent adds to the same row converge instead of
                        // clobbering; ordinary tables keep blind LWW upsert. A
                        // malformed OR-set payload degrades to the incoming
                        // bytes (LWW) via `merge_or_set_or_lww`.
                        let bytes = if self.or_set_tables.contains(table.as_str()) {
                            let existing: &[u8] = shadow
                                .get(&(table.clone(), pk.clone()))
                                .map_or(&[], |(p, _)| p.as_slice());
                            nostos_domain::merge_or_set_or_lww(existing, payload.as_ref())
                        } else if self.counter_tables.contains(table.as_str()) {
                            // PN-Counter (ADR-0030 addendum): merge per-replica
                            // elementwise max; a malformed payload degrades to LWW.
                            let existing: &[u8] = shadow
                                .get(&(table.clone(), pk.clone()))
                                .map_or(&[], |(p, _)| p.as_slice());
                            nostos_domain::merge_counter_or_lww(existing, payload.as_ref())
                        } else {
                            payload.as_ref().to_vec()
                        };
                        shadow.insert((table.clone(), pk.clone()), (bytes, *lsn));
                    }
                }
                RowOp::Delete { table, pk, .. } => {
                    let uncond = snapshot_tables.contains(table);
                    let admit = uncond
                        || shadow
                            .get(&(table.clone(), pk.clone()))
                            .is_none_or(|(_, prev)| *prev <= *lsn);
                    if admit {
                        shadow.remove(&(table.clone(), pk.clone()));
                    }
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

    fn read_payload(&self, table: &str, pk: &str) -> crate::Result<Option<Vec<u8>>> {
        Ok(self
            .rows
            .get(&(table.to_owned(), pk.to_owned()))
            .map(|(bytes, _)| bytes.clone()))
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

    fn horizon(&self) -> crate::Result<Option<String>> {
        Ok(self.horizon.clone())
    }

    fn save_horizon(&mut self, horizon: &str) -> crate::Result<()> {
        self.horizon = Some(horizon.to_string());
        Ok(())
    }

    fn clear(&mut self) -> crate::Result<()> {
        // ADR-0029: reset to fresh-client state for sign-out / principal switch.
        // `rows.clear()` empties the data store; the checkpoint reset to ZERO is
        // load-bearing — a stale checkpoint makes the next principal resume from
        // the old LSN, skip the snapshot, and see an empty DB permanently
        // (resume-without-snapshot unsoundness). InMemoryStorage does not persist
        // epoch (the trait default is always 0), so there is no epoch field to
        // reset. The outbox is cleared here too so a single call wipes the whole
        // principal's footprint; Outbox::clear covers the outbox-only path.
        self.rows.clear();
        // ADR-0029: checkpoint → 0 is load-bearing (resume-without-snapshot guard).
        self.checkpoint = Lsn::ZERO;
        // Same reason, direct mode's half: a surviving horizon would make the
        // next principal resume mid-log and never see the rows below it.
        self.horizon = None;
        self.outbox.clear();
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

    /// Trivially atomic: all inserts happen synchronously in-process, so the
    /// group is all-or-nothing by construction (ADR-0032 T3).
    fn enqueue_batch(&mut self, writes: Vec<PendingWrite>) -> crate::Result<Vec<u64>> {
        let mut ids = Vec::with_capacity(writes.len());
        for w in writes {
            self.next_write_id = self
                .next_write_id
                .checked_add(1)
                .expect("write id space exhausted");
            let id = self.next_write_id;
            self.outbox.insert(id, w);
            ids.push(id);
        }
        Ok(ids)
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
                let incoming = write.payload_json.as_deref().unwrap_or("null").as_bytes();
                // OR-set tables merge the optimistic edit element-wise by HLC
                // (ADR-0030); ordinary tables clobber (blind upsert). Optimistic:
                // stamp MAX so the local edit survives any in-flight server op on
                // this pk until the echo reconciles.
                let bytes = if self.or_set_tables.contains(write.table.as_str()) {
                    let existing: &[u8] = self
                        .rows
                        .get(&(write.table.clone(), write.pk.clone()))
                        .map_or(&[], |(p, _)| p.as_slice());
                    nostos_domain::merge_or_set_or_lww(existing, incoming)
                } else if self.counter_tables.contains(write.table.as_str()) {
                    // PN-Counter (ADR-0030 addendum): merge the optimistic delta
                    // per-replica max instead of clobbering.
                    let existing: &[u8] = self
                        .rows
                        .get(&(write.table.clone(), write.pk.clone()))
                        .map_or(&[], |(p, _)| p.as_slice());
                    nostos_domain::merge_counter_or_lww(existing, incoming)
                } else {
                    incoming.to_vec()
                };
                self.rows
                    .insert((write.table.clone(), write.pk.clone()), (bytes, u64::MAX));
            }
            WriteOp::Delete => {
                self.rows.remove(&(write.table.clone(), write.pk.clone()));
            }
            // Both no-ops (clippy-fused: identical empty bodies). Patch needs a
            // read-merge-write this opaque store can't do — unlike SqliteStorage,
            // which implements optimistic Patch because real clients DO issue
            // patches (the "patch edits don't render offline" regression).
            // ponytail: the two Storage impls diverge here (audit 2026-08-17
            // L4) — an offline Patch on this backend stays invisible until the
            // server's replicated echo reconciles (apply_batch upserts the
            // authoritative image). Upgrade: store typed JSON and merge
            // columns. Increment is server-authoritative (ADR-0030 Decision 1
            // — can't compute col+delta locally).
            WriteOp::Patch | WriteOp::Increment => {}
        }
        Ok(())
    }

    fn clear(&mut self) -> crate::Result<()> {
        // ponytail: 4b per-principal retention layers above this (ADR-0029
        // §Decision-2, pending ratification) — today sign-out discards ALL
        // pending writes. InMemoryStorage has no dead-letter state (the
        // bump_attempts/mark_dead_letter defaults are no-ops here), so draining
        // the BTreeMap is the complete wipe.
        self.outbox.clear();
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
    use std::collections::HashSet;

    fn ins(table: &str, pk: &str, payload: &[u8]) -> RowOp {
        RowOp::Insert {
            table: table.into(),
            pk: pk.into(),
            payload: Bytes::copy_from_slice(payload),
        }
    }

    fn empty_snap() -> HashSet<String> {
        HashSet::new()
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
        let ops = [
            (ins("tasks", "1", b"alice"), 100),
            (ins("tasks", "2", b"bob"), 100),
        ];
        s.apply_batch(&ops, Lsn::new(100), &empty_snap()).unwrap();

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

        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &empty_snap(),
        )
        .unwrap();
        // Re-apply the SAME op (same table+pk) — must UPSERT, not insert a copy.
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &empty_snap(),
        )
        .unwrap();

        assert_eq!(s.row_count(), 1, "no duplicate row");
        assert_eq!(s.payload("tasks", "1"), Some(b"v1" as &[u8]));
    }

    #[test]
    fn update_overwrites_payload_by_pk() {
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &empty_snap(),
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
            &empty_snap(),
        )
        .unwrap();

        assert_eq!(s.row_count(), 1);
        assert_eq!(s.payload("tasks", "1"), Some(b"v2" as &[u8]));
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(20));
    }

    #[test]
    fn delete_removes_row() {
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"x"), 10)],
            Lsn::new(10),
            &empty_snap(),
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
            &empty_snap(),
        )
        .unwrap();

        assert_eq!(s.row_count(), 0);
        assert!(s.payload("tasks", "1").is_none());
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(20));
    }

    #[test]
    fn checkpoint_is_monotonic_lower_lsn_does_not_regress() {
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"x"), 100)],
            Lsn::new(100),
            &empty_snap(),
        )
        .unwrap();
        // A late-arriving batch with a stale LSN must NOT drag the checkpoint back.
        s.apply_batch(
            &[(ins("tasks", "2", b"y"), 50)],
            Lsn::new(50),
            &empty_snap(),
        )
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
        s.apply_batch(&[], Lsn::new(42), &empty_snap()).unwrap();
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(42));
        assert_eq!(s.row_count(), 0);
    }

    #[test]
    fn delete_of_missing_row_is_a_noop() {
        // Idempotency on the delete path: deleting a pk that isn't there must
        // not error and must not change row count.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "never-existed".into(),
                    old_payload: None,
                },
                5,
            )],
            Lsn::new(5),
            &empty_snap(),
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
            (ins("tasks", "2", b"bob"), 10),
            (ins("tasks", "1", b"alice"), 10),
            (ins("users", "9", b"carol"), 10), // different table — must be excluded
        ];
        s.apply_batch(&ops, Lsn::new(10), &empty_snap()).unwrap();

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
            &[
                (ins("tasks", "1", b"keep"), 10),
                (ins("tasks", "2", b"drop"), 10),
            ],
            Lsn::new(10),
            &empty_snap(),
        )
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "2".into(),
                    old_payload: None,
                },
                20,
            )],
            Lsn::new(20),
            &empty_snap(),
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
        s.apply_batch(
            &[(ins("tasks", "1", b"v1"), 10)],
            Lsn::new(10),
            &empty_snap(),
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
            &empty_snap(),
        )
        .unwrap();

        let rows = s.rows_for("tasks");
        assert_eq!(rows, vec![("1".to_string(), b"v2".to_vec())]);
    }

    #[test]
    fn stale_delete_is_gated_out_and_row_survives() {
        // ADR-0025 slice 4a core correctness: out-of-order delivery must not
        // corrupt state. Apply a live INSERT@160 then a replayed DELETE@140 on
        // the same pk — the delete is stale (lsn < applied_lsn) and MUST be
        // skipped, leaving the row at its newer value.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"new"), 160)],
            Lsn::new(160),
            &empty_snap(),
        )
        .unwrap();
        s.apply_batch(
            &[(
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "1".into(),
                    old_payload: None,
                },
                140,
            )],
            Lsn::new(160),
            &empty_snap(),
        )
        .unwrap();
        assert_eq!(
            s.payload("tasks", "1"),
            Some(b"new" as &[u8]),
            "stale delete gated out — newer row survives"
        );
        assert_eq!(s.row_count(), 1);
    }

    #[test]
    fn snapshot_table_overwrites_despite_lower_lsn() {
        // ADR-0025 slice 4a design D: a table in snapshot_tables applies
        // UNCONDITIONALLY, so a synthetic-LSN snapshot row (lsn below the
        // persisted real lsn) still clobbers the stored row.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"real"), 9_000)],
            Lsn::new(9_000),
            &empty_snap(),
        )
        .unwrap();
        let snap = HashSet::from(["tasks".to_string()]);
        // Synthetic snapshot row at lsn=5 (<< 9_000) — unconditional under D.
        s.apply_batch(&[(ins("tasks", "1", b"snap"), 5)], Lsn::new(9_000), &snap)
            .unwrap();
        assert_eq!(
            s.payload("tasks", "1"),
            Some(b"snap" as &[u8]),
            "snapshot row applies unconditionally despite lower lsn"
        );
    }

    #[test]
    fn clear_resets_to_fresh_client_state() {
        // ADR-0029: sign-out wipe resets to a fresh-client image — no rows,
        // checkpoint ZERO (load-bearing — a stale checkpoint makes the next
        // principal resume past the snapshot and see an empty DB permanently),
        // and a drained outbox. InMemoryStorage carries no epoch field (the
        // trait default is always 0), so there is no epoch to reset here.
        let mut s = InMemoryStorage::new();
        s.apply_batch(
            &[(ins("tasks", "1", b"alice"), 100)],
            Lsn::new(100),
            &empty_snap(),
        )
        .unwrap();
        s.enqueue(PendingWrite {
            table: "tasks".into(),
            op: WriteOp::Upsert,
            pk: "2".into(),
            payload_json: Some(r#"{"title":"b"}"#.into()),
        })
        .unwrap();
        assert_eq!(s.row_count(), 1);
        assert_eq!(s.checkpoint().unwrap(), Lsn::new(100));
        assert_eq!(s.outbox_len(), 1);

        Storage::clear(&mut s).unwrap();

        assert_eq!(s.row_count(), 0, "rows cleared");
        assert_eq!(
            s.checkpoint().unwrap(),
            Lsn::ZERO,
            "checkpoint reset to 0 — the resume-without-snapshot guard",
        );
        assert_eq!(s.outbox_len(), 0, "outbox cleared");
    }

    /// A store with one OR-set table ("tags"); all others ordinary.
    fn or_set_store() -> InMemoryStorage {
        let tables: HashSet<String> = ["tags".to_string()].into_iter().collect();
        InMemoryStorage::new().with_or_set_tables(tables)
    }

    fn counter_store() -> InMemoryStorage {
        let tables: HashSet<String> = ["counts".to_string()].into_iter().collect();
        InMemoryStorage::new().with_counter_tables(tables)
    }

    #[test]
    fn or_set_table_merges_concurrent_adds() {
        // Two server frames adding DIFFERENT elements to the same OR-set row
        // must MERGE (both present), not clobber (ADR-0030). The second frame's
        // LSN exceeds the first so it admits.
        let mut s = or_set_store();
        let x = br#"{"elements":[{"v":"x","h":{"wall_ms":10,"ctr":0}}]}"#;
        let y = br#"{"elements":[{"v":"y","h":{"wall_ms":10,"ctr":1}}]}"#;
        s.apply_batch(&[(ins("tags", "s1", x), 1)], Lsn::new(1), &empty_snap())
            .unwrap();
        s.apply_batch(&[(ins("tags", "s1", y), 2)], Lsn::new(2), &empty_snap())
            .unwrap();
        let mut present =
            nostos_domain::present_elements(s.payload("tags", "s1").unwrap()).unwrap();
        present.sort();
        assert_eq!(present, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn or_set_apply_local_merges_optimistic_add() {
        // A server frame sets {x}; an optimistic local Upsert of {y} MERGES →
        // {x,y} — the offline-add-meets-server-frame case that plain LWW would
        // clobber (the whole reason the CRDT tier exists, ADR-0030).
        let mut s = or_set_store();
        let x = br#"{"elements":[{"v":"x","h":{"wall_ms":10,"ctr":0}}]}"#;
        s.apply_batch(&[(ins("tags", "s1", x), 1)], Lsn::new(1), &empty_snap())
            .unwrap();
        s.apply_local(&PendingWrite {
            table: "tags".into(),
            op: WriteOp::Upsert,
            pk: "s1".into(),
            payload_json: Some(r#"{"elements":[{"v":"y","h":{"wall_ms":10,"ctr":1}}]}"#.into()),
        })
        .unwrap();
        let mut present =
            nostos_domain::present_elements(s.payload("tags", "s1").unwrap()).unwrap();
        present.sort();
        assert_eq!(present, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn ordinary_table_still_clobbers_last_wins() {
        // Sanity: a non-OR-set, non-counter table does NOT merge — the second
        // Upsert wins, i.e. the apply path is unchanged for ordinary tables.
        let mut s = InMemoryStorage::new();
        s.apply_batch(&[(ins("tasks", "1", b"v1"), 1)], Lsn::new(1), &empty_snap())
            .unwrap();
        s.apply_batch(&[(ins("tasks", "1", b"v2"), 2)], Lsn::new(2), &empty_snap())
            .unwrap();
        assert_eq!(s.payload("tasks", "1"), Some(b"v2" as &[u8]));
    }

    // ---- PN-Counter CRDT (ADR-0030 addendum) ----

    #[test]
    fn counter_table_merges_concurrent_increments_from_two_replicas() {
        // Two server frames from DIFFERENT replicas incrementing the same
        // counter row must MERGE per-replica max → total = Σp − Σn. Replica A
        // adds 3 (+1 later), replica B adds 5 and subtracts 2 → total = 3+1+5−2 = 7.
        let mut s = counter_store();
        let a = br#"{"entries":[{"r":"A","p":3,"n":0}]}"#;
        let b = br#"{"entries":[{"r":"B","p":5,"n":2}]}"#;
        let a2 = br#"{"entries":[{"r":"A","p":4,"n":0}]}"#;
        s.apply_batch(&[(ins("counts", "c1", a), 1)], Lsn::new(1), &empty_snap())
            .unwrap();
        s.apply_batch(&[(ins("counts", "c1", b), 2)], Lsn::new(2), &empty_snap())
            .unwrap();
        s.apply_batch(&[(ins("counts", "c1", a2), 3)], Lsn::new(3), &empty_snap())
            .unwrap();
        let merged = s.payload("counts", "c1").unwrap();
        let value = nostos_domain::counter_value(merged).unwrap();
        assert_eq!(value, 7, "3 (A) + 1 (A bump) + 5 (B) − 2 (B) = 7");
    }

    #[test]
    fn counter_apply_local_merges_optimistic_increment() {
        // A server frame sets replica A's counter to 3; an optimistic local
        // increment from replica B by 5 MERGES → total 8 (the offline-increment-
        // meets-server-frame case that plain LWW would clobber).
        let mut s = counter_store();
        let a = br#"{"entries":[{"r":"A","p":3,"n":0}]}"#;
        s.apply_batch(&[(ins("counts", "c1", a), 1)], Lsn::new(1), &empty_snap())
            .unwrap();
        s.apply_local(&PendingWrite {
            table: "counts".into(),
            op: WriteOp::Upsert,
            pk: "c1".into(),
            payload_json: Some(r#"{"entries":[{"r":"B","p":5,"n":0}]}"#.into()),
        })
        .unwrap();
        let merged = s.payload("counts", "c1").unwrap();
        let value = nostos_domain::counter_value(merged).unwrap();
        assert_eq!(value, 8, "3 (A server) + 5 (B optimistic) = 8");
    }
}
