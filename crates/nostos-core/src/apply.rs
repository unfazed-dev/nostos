//! The apply state machine — turn a stream of replication frames into atomic
//! storage commits, and tell the caller what LSN to `Ack`.
//!
//! ## Bounding model
//!
//! A Postgres transaction is atomic: events sharing a `txn_id` MUST apply
//! together (all-or-nothing) so the client never observes a partially-applied
//! transaction. Between transaction boundaries the engine ALSO flushes when a
//! soft cap ([`ApplyEngine::max_batch`]) is reached — this keeps a long, single
//! transaction-less stream from buffering unbounded rows in memory and from
//! producing one giant SQLite transaction. The cap is a *soft* correctness
//! invariant: it never splits a known transaction, only batches across
//! txn-less / independent frames.
//!
//! ## The `Frame` input
//!
//! The engine consumes [`Frame`]s — a pure, runtime-free view of a replication
//! row. This type lives in `nostos-core` (not `nostos-infra::wire::WireFrame`)
//! because `nostos-core` must not depend on the infra ring; `nostos-client`'s
//! `SyncClient` does the trivial one-line conversion from the wire frame. The
//! payload is the *decoded* opaque bytes (the wire carries them hex-encoded;
//! the client hex-decodes once, at the boundary).
//!
//! ## Output
//!
//! Each commit yields an [`ApplyOutcome`] — the new checkpoint and the count of
//! rows applied. The caller (`SyncClient`) sends `Ack { lsn }` with that
//! checkpoint, driving the ack-driven slot advance on the server (ADR-0009).

use nostos_domain::{Lsn, RowOp};

use crate::{Storage, StorageError};
use std::collections::{HashMap, HashSet};

/// One decoded replication row, ready to apply. The pure-runtime twin of
/// `nostos_infra::wire::WireFrame` minus the hex encoding.
///
/// `payload` is `Some` for inserts/updates (the opaque tuple image, hex-decoded
/// to bytes at the wire boundary) and `None` for deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lsn: u64,
    pub op: nostos_domain::Operation,
    pub table: String,
    pub pk: String,
    pub payload: Option<Vec<u8>>,
    /// Events sharing a txn id belong to one atomic Postgres transaction and
    /// MUST apply together. `None` means "standalone" (flush by the soft cap).
    pub txn_id: Option<u64>,
}

impl Frame {
    /// Convert this frame into the [`RowOp`] the storage layer applies.
    ///
    /// Inserts/updates carry their payload; deletes carry only table + pk.
    #[must_use]
    pub fn into_row_op(self) -> RowOp {
        match self.op {
            nostos_domain::Operation::Insert => RowOp::Insert {
                table: self.table,
                pk: self.pk,
                payload: self.payload.unwrap_or_default().into(),
            },
            nostos_domain::Operation::Update => RowOp::Update {
                table: self.table,
                pk: self.pk,
                payload: self.payload.unwrap_or_default().into(),
            },
            nostos_domain::Operation::Delete => RowOp::Delete {
                table: self.table,
                pk: self.pk,
                old_payload: None,
            },
        }
    }

    /// The LSN of this frame as a typed [`Lsn`].
    #[must_use]
    pub const fn lsn(&self) -> Lsn {
        Lsn::new(self.lsn)
    }
}

/// The result of a single atomic commit: how far we got, and how many rows landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// The new durable checkpoint — the value to `Ack` to the server.
    pub checkpoint: Lsn,
    /// Rows applied in this commit (0 for an empty-txn boundary ack).
    pub rows_applied: usize,
}

/// The apply state machine. Owns a pending batch and flushes it to a [`Storage`]
/// at transaction boundaries or when the soft cap is reached.
///
/// Generic over `S: Storage` so it runs against the in-memory double in unit
/// tests and `SqliteStorage` in `nostos-client` with identical logic.
#[derive(Debug)]
pub struct ApplyEngine<S> {
    storage: S,
    /// Buffered frames awaiting the next commit boundary.
    pending: Vec<Frame>,
    /// The txn_id of the currently-open transaction, if any. Frames accumulate
    /// until the txn closes (a frame with a different/None txn_id arrives).
    open_txn: Option<u64>,
    /// The highest LSN in the pending batch — the checkpoint we'll ack on flush.
    high_water: Lsn,
    /// Soft cap on buffered frames across non-transactional runs.
    max_batch: usize,
    /// Snapshot-reconcile orphan candidates (ADR-0014 offline-delete fix).
    /// `begin` seeds this with the local PKs of the snapshotted table; each
    /// received snapshot row removes its pk; `end` reaps whatever remains.
    /// Empty except during an open snapshot window.
    snapshot_orphans: HashMap<String, HashSet<String>>,
}

/// Default soft cap before a non-transactional flush. Large enough to amortize
/// per-transaction overhead on a stream of independent frames, small enough that
/// a single SQLite transaction (and the in-memory buffer) stays bounded.
pub const DEFAULT_MAX_BATCH: usize = 256;

impl<S: Storage> ApplyEngine<S> {
    /// Build an engine over `storage`, with the default soft cap.
    #[must_use]
    pub fn new(storage: S) -> Self {
        Self::with_max_batch(storage, DEFAULT_MAX_BATCH)
    }

    /// Build an engine with an explicit soft cap. `max_batch == 0` is treated
    /// as "flush every frame" (useful for tests + the chaos e2e).
    #[must_use]
    pub fn with_max_batch(storage: S, max_batch: usize) -> Self {
        // Seed the high-water from the durable checkpoint BEFORE moving storage
        // into self — reconnecting over a store that already has progress must
        // not start the high-water below where we last committed.
        let high_water = storage.checkpoint().unwrap_or(Lsn::ZERO);
        Self {
            storage,
            pending: Vec::new(),
            open_txn: None,
            high_water,
            max_batch,
            snapshot_orphans: HashMap::new(),
        }
    }

    /// Hand the storage back (e.g. for assertions / to take a checkpoint read
    /// after the engine stops).
    #[must_use]
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Borrow the backing storage mutably. Lets a caller reach a backend-specific
    /// accessor (e.g. `InMemoryStorage::row_count`) without consuming the engine.
    /// The engine's own state (pending batch, high-water) is untouched.
    #[must_use]
    pub fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    /// Borrow the backing storage read-only.
    #[must_use]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The current durable checkpoint (delegates to storage).
    pub fn checkpoint(&self) -> crate::Result<Lsn> {
        self.storage.checkpoint()
    }

    /// The durable last-seen server slot epoch (ADR-0025 reconnect-resume gate).
    pub fn epoch(&self) -> crate::Result<u64> {
        self.storage.epoch()
    }

    /// Persist the server's current slot epoch (delegates to storage).
    pub fn save_epoch(&self, epoch: u64) -> crate::Result<()> {
        self.storage.save_epoch(epoch)
    }

    /// Is there a buffered-but-unflushed batch right now? `true` between a
    /// frame that got admitted (buffered) and the next commit boundary /
    /// explicit [`Self::flush`].
    ///
    /// This is the seam a caller uses to arm a time-bounded flush: `feed`'s
    /// return value alone is NOT the right signal (it returns `Some` on a
    /// txn-boundary flush that *also* buffers the new frame — see `feed`'s
    /// doc — so checking the return value would miss that a new batch is now
    /// pending). No I/O; safe to call from an async context without
    /// `spawn_blocking`.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Feed a frame. Returns `Some(outcome)` if this frame triggered a commit
    /// (a transaction closed, or the soft cap was reached), or `None` if the
    /// frame was buffered pending a future boundary.
    ///
    /// Transaction semantics:
    /// - A frame with `txn_id = Some(t)` extends the open transaction if one is
    ///   open with the same `t`; it *never* triggers a mid-transaction flush.
    /// - A frame whose `txn_id` differs from the open transaction (including
    ///   `None` vs `Some`) closes the open one → flush, then buffer the new frame.
    pub fn feed(&mut self, frame: Frame) -> crate::Result<Option<ApplyOutcome>> {
        // Does this frame close the currently-open transaction?
        let txn_changed = match (self.open_txn, frame.txn_id) {
            (Some(open), Some(t)) => open != t,
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        };

        // Flush the open transaction before admitting a frame from a different one.
        let mut flushed: Option<ApplyOutcome> = None;
        if txn_changed && !self.pending.is_empty() {
            // flush() returns Option<ApplyOutcome> (None only if nothing pending,
            // which the `!self.pending.is_empty()` guard rules out) — flatten, not wrap.
            flushed = self.flush()?;
        }

        // Admit the frame.
        self.high_water = self.high_water.max(frame.lsn());
        self.open_txn = frame.txn_id;

        // Snapshot-reconcile: a row the snapshot re-confirms is NOT an orphan,
        // so remove its pk from the open snapshot's candidate set (ADR-0014).
        // Applies to upserts (the row is present) AND deletes (the row is
        // already gone, can't be an orphan). A no-op when no snapshot is open.
        if let Some(orphans) = self.snapshot_orphans.get_mut(&frame.table) {
            orphans.remove(&frame.pk);
        }

        self.pending.push(frame);

        // Soft cap: flush a batch of independent (non-txn) frames so we don't
        // buffer forever. Never splits a known transaction.
        if self.open_txn.is_none() && self.pending.len() >= self.max_batch.max(1) {
            // Already flushed above? Then pending held < cap; the cap flush wins.
            // Not yet flushed? Flush now.
            if flushed.is_none() {
                flushed = self.flush()?;
            }
        }

        Ok(flushed)
    }

    /// A snapshot boundary control frame (ADR-0014 offline-delete fix).
    ///
    /// The server emits `snapshot_begin{T}` before a snapshot's rows and
    /// `snapshot_end{T}` after. Between them, every upsert the engine applies
    /// for `T` is a row the snapshot confirmed present; any local PK that does
    /// NOT appear in the snapshot is a row hard-deleted server-side while the
    /// client was offline (the snapshot is present-rows-only — `PgSnapshotter`
    /// delivers no tombstones). At `end` we reap that orphan set.
    ///
    /// - `begin`: snapshot the current local PKs for `table` into the orphan
    ///   candidate set, MINUS `exempt_pks` (the outbox's pending-local PKs —
    ///   ADR-0025 hole #1: the user's own unacked writes must never be reaped).
    ///   If a snapshot was already open for `table`, the new begin replaces it
    ///   (defensive — the wire is begin/end-paired).
    /// - `end`: remove the recorded set and bulk-delete every pk still in it
    ///   (those that no snapshot row re-confirmed). No-op if no snapshot was
    ///   open for `table` (a stray `end` without a `begin`).
    ///
    /// The delete commits immediately (the storage contract for `delete_pks`
    /// permits auto-commit-per-call) so it lands before the pump acks. The
    /// orphan-set removal on the per-row apply path is in `feed` — received
    /// rows are subtracted from the set so they survive the `end` reap.
    pub fn snapshot_boundary(
        &mut self,
        table: &str,
        begin: bool,
        exempt_pks: &[String],
    ) -> crate::Result<()> {
        if begin {
            let mut orphans: HashSet<String> =
                self.storage.pks_for_table(table)?.into_iter().collect();
            // ADR-0025 hole #1: never reap the user's own pending-local writes.
            for pk in exempt_pks {
                orphans.remove(pk);
            }
            self.snapshot_orphans.insert(table.to_string(), orphans);
            Ok(())
        } else {
            // `end`: reap whatever remains in the orphan set.
            if let Some(orphans) = self.snapshot_orphans.remove(table) {
                if !orphans.is_empty() {
                    let to_delete: Vec<String> = orphans.into_iter().collect();
                    self.storage.delete_pks(table, &to_delete)?;
                }
            }
            // Stray `end` with no open snapshot for `table` → no-op.
            Ok(())
        }
    }

    /// Flush any buffered frames as one atomic commit. Returns `None` if there
    /// was nothing pending (the caller may still want to ack the high-water LSN).
    ///
    /// Call this when the stream goes idle / the connection closes to ensure the
    /// last partial batch is durable before reconnect.
    pub fn flush(&mut self) -> crate::Result<Option<ApplyOutcome>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let count = self.pending.len();
        let checkpoint = self.high_water;

        // ADR-0025 slice 4a: sort by LSN so the storage's per-row `>=` gate sees
        // monotonic input across a mixed live+replay batch, then pair each op
        // with its source LSN for per-row gating. `snapshot_tables` (design D)
        // = tables with an open snapshot-reconcile window → apply unconditionally
        // (synthetic-LSN snapshot rows must clobber stored rows).
        self.pending.sort_by_key(|f| f.lsn);
        let ops: Vec<(RowOp, u64)> = self
            .pending
            .iter()
            .map(|f| (f.clone().into_row_op(), f.lsn))
            .collect();
        let snapshot_tables: HashSet<String> = self.snapshot_orphans.keys().cloned().collect();
        // Built `ops` from iter() (not drain) → on error `pending` is intact for
        // retry; no Frame-rebuild needed. The sort is idempotent on re-flush.
        match self.storage.apply_batch(&ops, checkpoint, &snapshot_tables) {
            Ok(()) => {
                self.pending.clear();
                self.open_txn = None;
                Ok(Some(ApplyOutcome {
                    checkpoint,
                    rows_applied: count,
                }))
            }
            // Surface the backend error verbatim; pending is preserved for retry.
            Err(StorageError::Backend(msg)) => Err(StorageError::Backend(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryStorage;
    use crate::Outbox;
    use bytes::Bytes;

    fn frame(lsn: u64, pk: &str, txn: Option<u64>) -> Frame {
        Frame {
            lsn,
            op: nostos_domain::Operation::Insert,
            table: "tasks".into(),
            pk: pk.into(),
            payload: Some(format!("payload-{pk}").into_bytes()),
            txn_id: txn,
        }
    }

    fn into_op(f: Frame) -> RowOp {
        f.into_row_op()
    }

    #[test]
    fn frame_into_insert_carries_payload() {
        let f = Frame {
            lsn: 1,
            op: nostos_domain::Operation::Insert,
            table: "t".into(),
            pk: "1".into(),
            payload: Some(b"hello".to_vec()),
            txn_id: None,
        };
        let RowOp::Insert { table, pk, payload } = f.into_row_op() else {
            panic!("expected Insert");
        };
        assert_eq!(table, "t");
        assert_eq!(pk, "1");
        assert_eq!(payload.as_ref(), b"hello");
    }

    #[test]
    fn frame_into_delete_drops_payload() {
        let f = Frame {
            lsn: 1,
            op: nostos_domain::Operation::Delete,
            table: "t".into(),
            pk: "9".into(),
            payload: Some(b"ignored".to_vec()),
            txn_id: None,
        };
        assert!(matches!(f.into_row_op(), RowOp::Delete { .. }));
    }

    #[test]
    fn frames_in_one_txn_buffer_until_txn_closes() {
        // Three frames of txn 7, then a txn-less frame: the txn-7 batch should
        // flush on the boundary, then the standalone frame stays buffered.
        let mut engine = ApplyEngine::new(InMemoryStorage::new());

        // None returned while accumulating within the transaction.
        assert!(engine.feed(frame(10, "1", Some(7))).unwrap().is_none());
        assert!(engine.feed(frame(11, "2", Some(7))).unwrap().is_none());
        assert!(engine.feed(frame(12, "3", Some(7))).unwrap().is_none());

        // A frame outside txn 7 closes it → flush.
        let outcome = engine
            .feed(frame(13, "4", None))
            .unwrap()
            .expect("txn boundary flush");
        assert_eq!(outcome.rows_applied, 3);
        assert_eq!(outcome.checkpoint, Lsn::new(12)); // high-water of the txn batch

        // The standalone frame is now buffered (not yet flushed).
        let storage = engine.into_storage();
        assert_eq!(storage.row_count(), 3, "the 4th frame is still pending");
    }

    #[test]
    fn soft_cap_flushes_non_transactional_stream() {
        // max_batch = 3: three independent frames should flush on the third.
        let mut engine = ApplyEngine::with_max_batch(InMemoryStorage::new(), 3);

        assert!(engine.feed(frame(1, "a", None)).unwrap().is_none());
        assert!(engine.feed(frame(2, "b", None)).unwrap().is_none());
        let outcome = engine
            .feed(frame(3, "c", None))
            .unwrap()
            .expect("cap flush");
        assert_eq!(outcome.rows_applied, 3);
        assert_eq!(outcome.checkpoint, Lsn::new(3));

        let storage = engine.into_storage();
        assert_eq!(storage.row_count(), 3);
        assert_eq!(storage.checkpoint().unwrap(), Lsn::new(3));
    }

    #[test]
    fn flush_drains_pending_and_advances_checkpoint() {
        let mut engine = ApplyEngine::new(InMemoryStorage::new());
        engine.feed(frame(10, "1", None)).unwrap();
        engine.feed(frame(20, "2", None)).unwrap();

        let outcome = engine.flush().unwrap().expect("had pending");
        assert_eq!(outcome.rows_applied, 2);
        assert_eq!(outcome.checkpoint, Lsn::new(20));

        // A second flush with nothing pending returns None.
        assert!(engine.flush().unwrap().is_none());
    }

    #[test]
    fn flush_empty_returns_none_without_error() {
        let mut engine = ApplyEngine::new(InMemoryStorage::new());
        assert!(engine.flush().unwrap().is_none());
    }

    #[test]
    fn resume_after_flush_only_fetches_new_frames() {
        // The contract the chaos e2e proves end-to-end: after a flush at LSN 20,
        // the durable checkpoint is 20, so a reconnect subscribes with
        // resume_lsn=20 and the server skips everything ≤ 20.
        let mut engine = ApplyEngine::with_max_batch(InMemoryStorage::new(), 2);
        engine.feed(frame(10, "a", None)).unwrap();
        engine.feed(frame(20, "b", None)).unwrap(); // flush at cap

        let checkpoint = engine.checkpoint().unwrap();
        assert_eq!(
            checkpoint,
            Lsn::new(20),
            "durable checkpoint reflects applied frames"
        );

        // Feed a frame the server would only send AFTER resume_lsn=20.
        engine.feed(frame(30, "c", None)).unwrap();
        engine.flush().unwrap();
        let storage = engine.into_storage();
        assert_eq!(storage.row_count(), 3);
        assert_eq!(storage.checkpoint().unwrap(), Lsn::new(30));
    }

    #[test]
    fn engine_seeds_high_water_from_storage_checkpoint() {
        // An engine built over a storage that already has progress should not
        // regress the high-water mark below it.
        let mut storage = InMemoryStorage::new();
        storage
            .apply_batch(
                &[(
                    RowOp::Insert {
                        table: "tasks".into(),
                        pk: "1".into(),
                        payload: Bytes::from_static(b"x"),
                    },
                    500,
                )],
                Lsn::new(500),
                &HashSet::new(),
            )
            .unwrap();

        let engine = ApplyEngine::new(storage);
        assert_eq!(engine.high_water, Lsn::new(500));
    }

    #[test]
    fn idempotent_reapply_via_reconnect_replay_is_safe() {
        // Simulate reconnect replay: the server re-sends frames 10..20 even
        // though we already applied them. Idempotent upsert-by-pk means the
        // store doesn't double-count.
        let mut engine = ApplyEngine::with_max_batch(InMemoryStorage::new(), 100);

        for i in 1..=5 {
            engine.feed(frame(i * 10, &i.to_string(), None)).unwrap();
        }
        engine.flush().unwrap();
        let storage = engine.into_storage();
        assert_eq!(storage.row_count(), 5);
        assert_eq!(storage.checkpoint().unwrap(), Lsn::new(50));

        // Re-open over the SAME logical store and "replay" the same frames.
        // (InMemoryStorage doesn't persist; we re-create with the checkpoint
        // advanced to simulate the durable state, then re-feed — the row count
        // must stay 5, not 10.)
        let mut replay = InMemoryStorage::new();
        replay
            .apply_batch(
                &[(
                    RowOp::Insert {
                        table: "tasks".into(),
                        pk: "1".into(),
                        payload: b"payload-1".to_vec().into(),
                    },
                    50,
                )],
                Lsn::new(50),
                &HashSet::new(),
            )
            .unwrap();
        let mut engine2 = ApplyEngine::with_max_batch(replay, 100);
        for i in 1..=5 {
            engine2.feed(frame(i * 10, &i.to_string(), None)).unwrap();
        }
        engine2.flush().unwrap();
        let replayed = engine2.into_storage();
        assert_eq!(
            replayed.row_count(),
            5,
            "idempotent re-apply did not duplicate rows"
        );
    }

    #[test]
    fn has_pending_reflects_buffered_state() {
        let mut engine = ApplyEngine::new(InMemoryStorage::new());
        assert!(!engine.has_pending(), "nothing fed yet");

        engine.feed(frame(10, "1", Some(7))).unwrap();
        assert!(engine.has_pending(), "a txn frame is buffered");

        engine.feed(frame(11, "2", Some(7))).unwrap();
        assert!(engine.has_pending(), "still accumulating the same txn");

        engine.flush().unwrap();
        assert!(!engine.has_pending(), "explicit flush drains the buffer");
    }

    #[test]
    fn has_pending_true_after_a_boundary_flush_that_buffers_the_next_frame() {
        // The exact trap the doc comment warns about: `feed` returns `Some`
        // (a txn boundary flushed) on the SAME call that buffers the new
        // frame — checking the return value alone would miss that a fresh
        // batch is now pending. `has_pending` must catch it.
        let mut engine = ApplyEngine::new(InMemoryStorage::new());
        engine.feed(frame(10, "1", Some(7))).unwrap();
        let outcome = engine.feed(frame(20, "2", Some(8))).unwrap();
        assert!(outcome.is_some(), "txn 7 closed on this call");
        assert!(
            engine.has_pending(),
            "frame 2 (txn 8) is buffered by the same call"
        );
    }

    #[test]
    fn into_op_helper_is_wired() {
        // Sanity on the test-local helper (kept so the RowOp path is exercised).
        let op = into_op(frame(1, "x", None));
        assert!(matches!(op, RowOp::Insert { .. }));
    }

    // ---- snapshot-reconcile (ADR-0014 offline-delete fix) ----

    /// The P0 fix: a row hard-deleted server-side while the client was offline
    /// is absent from the snapshot → the client keeps a stale ORPHAN. The
    /// snapshot_begin/end boundary pair must reap local PKs the snapshot did
    /// NOT re-confirm.
    #[test]
    fn snapshot_reconcile_removes_orphans_absent_from_snapshot() {
        // Seed: local has pk=A in table T (e.g. carried over from a prior
        // session). The server has since hard-deleted A, so the snapshot only
        // contains B.
        let mut storage = InMemoryStorage::new();
        storage
            .apply_batch(
                &[(
                    RowOp::Insert {
                        table: "T".into(),
                        pk: "A".into(),
                        payload: Bytes::from_static(b"old"),
                    },
                    1,
                )],
                Lsn::new(1),
                &HashSet::new(),
            )
            .unwrap();

        let mut engine = ApplyEngine::new(storage);

        // snapshot_begin{T}: seed the orphan candidate set with the local PKs.
        engine.snapshot_boundary("T", true, &[]).unwrap();
        // The snapshot delivers only B (A is absent — server hard-deleted it).
        engine
            .feed(Frame {
                lsn: 2,
                op: nostos_domain::Operation::Insert,
                table: "T".into(),
                pk: "B".into(),
                payload: Some(b"new".to_vec()),
                txn_id: None,
            })
            .unwrap();
        // snapshot_end{T}: reap orphans → A is gone, B stays.
        engine.snapshot_boundary("T", false, &[]).unwrap();
        engine.flush().unwrap();

        let storage = engine.into_storage();
        assert!(
            storage.payload("T", "A").is_none(),
            "orphan pk A (absent from snapshot) must be reaped"
        );
        assert_eq!(
            storage.payload("T", "B"),
            Some(b"new" as &[u8]),
            "snapshot row B must be present"
        );
    }

    /// A live upsert OUTSIDE any snapshot window must NOT trigger reconcile —
    /// no false deletes. The orphan set is empty except between begin/end, so
    /// a stray frame can never reap a row.
    #[test]
    fn live_upsert_outside_snapshot_does_not_reconcile() {
        let mut storage = InMemoryStorage::new();
        storage
            .apply_batch(
                &[
                    (
                        RowOp::Insert {
                            table: "T".into(),
                            pk: "A".into(),
                            payload: Bytes::from_static(b"a"),
                        },
                        1,
                    ),
                    (
                        RowOp::Insert {
                            table: "T".into(),
                            pk: "B".into(),
                            payload: Bytes::from_static(b"b"),
                        },
                        1,
                    ),
                ],
                Lsn::new(1),
                &HashSet::new(),
            )
            .unwrap();

        let mut engine = ApplyEngine::new(storage);
        // A live upsert for pk=C with NO boundary calls.
        engine
            .feed(Frame {
                lsn: 2,
                op: nostos_domain::Operation::Insert,
                table: "T".into(),
                pk: "C".into(),
                payload: Some(b"c".to_vec()),
                txn_id: None,
            })
            .unwrap();
        engine.flush().unwrap();

        let storage = engine.into_storage();
        assert_eq!(storage.payload("T", "A"), Some(b"a" as &[u8]));
        assert_eq!(storage.payload("T", "B"), Some(b"b" as &[u8]));
        assert_eq!(storage.payload("T", "C"), Some(b"c" as &[u8]));
        assert_eq!(
            storage.row_count(),
            3,
            "no false deletes outside a snapshot window"
        );
    }

    /// A row that arrives during the snapshot window must NOT be reaped: the
    /// apply path removes its pk from the orphan candidate set. This is the
    /// per-row subtraction that protects received rows from the `end` reap.
    #[test]
    fn snapshot_window_received_row_is_protected_from_reap() {
        let mut storage = InMemoryStorage::new();
        // Local has A and B; the snapshot will re-deliver A only (B was
        // hard-deleted server-side).
        storage
            .apply_batch(
                &[
                    (
                        RowOp::Insert {
                            table: "T".into(),
                            pk: "A".into(),
                            payload: Bytes::from_static(b"a-old"),
                        },
                        1,
                    ),
                    (
                        RowOp::Insert {
                            table: "T".into(),
                            pk: "B".into(),
                            payload: Bytes::from_static(b"b"),
                        },
                        1,
                    ),
                ],
                Lsn::new(1),
                &HashSet::new(),
            )
            .unwrap();

        let mut engine = ApplyEngine::new(storage);
        engine.snapshot_boundary("T", true, &[]).unwrap();
        // A re-arrives (present in snapshot) → removed from orphan set.
        engine
            .feed(Frame {
                lsn: 2,
                op: nostos_domain::Operation::Insert,
                table: "T".into(),
                pk: "A".into(),
                payload: Some(b"a-new".to_vec()),
                txn_id: None,
            })
            .unwrap();
        engine.snapshot_boundary("T", false, &[]).unwrap();
        engine.flush().unwrap();

        let storage = engine.into_storage();
        assert_eq!(
            storage.payload("T", "A"),
            Some(b"a-new" as &[u8]),
            "re-confirmed row A survived (and was updated)"
        );
        assert!(
            storage.payload("T", "B").is_none(),
            "absent row B reaped as orphan"
        );
    }

    /// A snapshot_end with no matching begin (stray control frame, e.g. a
    /// redelivery) is a no-op — no rows touched.
    #[test]
    fn snapshot_end_without_begin_is_noop() {
        let mut storage = InMemoryStorage::new();
        storage
            .apply_batch(
                &[(
                    RowOp::Insert {
                        table: "T".into(),
                        pk: "A".into(),
                        payload: Bytes::from_static(b"a"),
                    },
                    1,
                )],
                Lsn::new(1),
                &HashSet::new(),
            )
            .unwrap();
        let mut engine = ApplyEngine::new(storage);
        engine.snapshot_boundary("T", false, &[]).unwrap();
        let storage = engine.into_storage();
        assert_eq!(storage.payload("T", "A"), Some(b"a" as &[u8]));
    }

    /// ADR-0025 hole #1: a pending-local write (in the outbox, not yet echoed
    /// by the server) sits in the data store, so it is absent from a server
    /// snapshot. It MUST NOT be reaped — it is the user's own unacked work, not
    /// an orphan. The `exempt_pks` set (the outbox's pending pks for the table)
    /// removes it from the orphan seed at `begin`.
    #[test]
    fn snapshot_reconcile_exempts_pending_local_writes() {
        let mut storage = InMemoryStorage::new();
        // A is in the store (instant-local optimistic render)...
        storage
            .apply_batch(
                &[(
                    RowOp::Insert {
                        table: "T".into(),
                        pk: "A".into(),
                        payload: Bytes::from_static(b"a-local"),
                    },
                    1,
                )],
                Lsn::new(1),
                &HashSet::new(),
            )
            .unwrap();
        // ...AND A is pending in the outbox (server has not echoed it yet).
        storage
            .enqueue(crate::PendingWrite {
                table: "T".into(),
                op: crate::outbox::WriteOp::Upsert,
                pk: "A".into(),
                payload_json: Some(r#"{"id":"A"}"#.into()),
            })
            .unwrap();
        // The caller derives the exempt set from the outbox (ADR-0025 hole #1).
        let exempt = storage.pending_pks_for_table("T").unwrap();
        assert_eq!(exempt, vec!["A".to_string()]);

        let mut engine = ApplyEngine::new(storage);
        // snapshot_begin{T}: seed orphans MINUS the exempt pending-local pk A.
        engine.snapshot_boundary("T", true, &exempt).unwrap();
        // The snapshot delivers only B (A is absent — the server doesn't know
        // A yet; A is still in the client's outbox).
        engine
            .feed(Frame {
                lsn: 2,
                op: nostos_domain::Operation::Insert,
                table: "T".into(),
                pk: "B".into(),
                payload: Some(b"b-snap".to_vec()),
                txn_id: None,
            })
            .unwrap();
        // snapshot_end{T}: reap — but A is exempt, so it survives.
        engine.snapshot_boundary("T", false, &[]).unwrap();
        engine.flush().unwrap();

        let storage = engine.into_storage();
        assert_eq!(
            storage.payload("T", "A"),
            Some(b"a-local" as &[u8]),
            "pending-local write A must NOT be reaped (hole #1)"
        );
        assert_eq!(
            storage.payload("T", "B"),
            Some(b"b-snap" as &[u8]),
            "snapshot row B present"
        );
    }
}
