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

        // Materialize the RowOps and apply atomically. On error the storage
        // contract guarantees nothing committed — leave pending intact so a
        // retry re-attempts the same batch.
        let ops: Vec<RowOp> = self.pending.drain(..).map(Frame::into_row_op).collect();
        match self.storage.apply_batch(&ops, checkpoint) {
            Ok(()) => {
                self.open_txn = None;
                Ok(Some(ApplyOutcome {
                    checkpoint,
                    rows_applied: count,
                }))
            }
            // Surface the backend error verbatim; pending is preserved for retry.
            Err(StorageError::Backend(msg)) => {
                // Re-buffer the ops we drained so a retry sees the same batch.
                // (We lost the original Frame metadata converting to RowOp, but
                // the ops are what matter for re-apply — idempotent by pk.)
                self.pending = ops
                    .into_iter()
                    .map(|op| Frame {
                        lsn: checkpoint.raw(),
                        op: op.operation(),
                        table: op.table().to_owned(),
                        pk: op.pk().to_owned(),
                        payload: if op.has_payload() {
                            Some(op.payload_bytes().to_vec())
                        } else {
                            None
                        },
                        txn_id: None,
                    })
                    .collect();
                Err(StorageError::Backend(msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryStorage;
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
                &[RowOp::Insert {
                    table: "tasks".into(),
                    pk: "1".into(),
                    payload: Bytes::from_static(b"x"),
                }],
                Lsn::new(500),
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
                &[RowOp::Insert {
                    table: "tasks".into(),
                    pk: "1".into(),
                    payload: b"payload-1".to_vec().into(),
                }],
                Lsn::new(50),
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
    fn into_op_helper_is_wired() {
        // Sanity on the test-local helper (kept so the RowOp path is exercised).
        let op = into_op(frame(1, "x", None));
        assert!(matches!(op, RowOp::Insert { .. }));
    }
}
