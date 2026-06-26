//! Replication events — the units that flow through the entire pipeline.
//!
//! These types are deliberately plain (no async, no I/O). They're cloned along
//! a 1-to-N fan-out, so the payload is [`bytes::Bytes`] — a cheap-clone
//! reference-counted buffer (Arc-backed internally), so fanning an event out to
//! 10,000 sessions bumps a refcount, not 10,000 byte copies.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::lsn::Lsn;

/// One row-level mutation extracted from the WAL.
///
/// `payload` is the logical-replication tuple image, opaque to the domain —
/// the wire codec (infra) is responsible for translating to/from the
/// on-the-wire frame. [`Bytes`] is reference-counted, so a fan-out to 10,000
/// sessions clones a refcount, not 10,000 copies of the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowOp {
    Insert {
        table: String,
        pk: String,
        payload: Bytes,
    },
    Update {
        table: String,
        pk: String,
        payload: Bytes,
    },
    Delete {
        table: String,
        pk: String,
    },
}

impl RowOp {
    /// The table this op targets — used by the [`Predicate`] index for O(1)
    /// candidate-session pruning.
    ///
    /// [`Predicate`]: crate::predicate::Predicate
    #[inline]
    #[must_use]
    pub fn table(&self) -> &str {
        match self {
            RowOp::Insert { table, .. }
            | RowOp::Update { table, .. }
            | RowOp::Delete { table, .. } => table,
        }
    }

    /// The primary-key value as a string (logical replication delivers PKs as
    /// text in the tuple image; we model it as opaque string here).
    #[inline]
    #[must_use]
    pub fn pk(&self) -> &str {
        match self {
            RowOp::Insert { pk, .. } | RowOp::Update { pk, .. } | RowOp::Delete { pk, .. } => pk,
        }
    }

    /// Approximate byte weight of this op's payload — used by the router's
    /// accounting (MB/sec throughput) and by backpressure budgeting.
    #[inline]
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            RowOp::Insert { payload, .. } | RowOp::Update { payload, .. } => payload.len(),
            RowOp::Delete { .. } => 0,
        }
    }

    /// True if this op carries a payload (Insert/Update). Deletes don't.
    #[inline]
    #[must_use]
    pub const fn has_payload(&self) -> bool {
        !matches!(self, RowOp::Delete { .. })
    }

    /// High-level operation classification — used in metrics & benchmark
    /// reporting (PowerSync publishes separate small-row / large-row / txn
    /// ceilings, so we classify the same way).
    #[inline]
    #[must_use]
    pub fn operation(&self) -> Operation {
        match self {
            RowOp::Insert { .. } => Operation::Insert,
            RowOp::Update { .. } => Operation::Update,
            RowOp::Delete { .. } => Operation::Delete,
        }
    }
}

/// The kind of row mutation. Reported in metrics & benchmark output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Insert,
    Update,
    Delete,
}

/// A row op tagged with its source LSN — the unit that flows through the
/// `FanOutService` and into per-session sinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationEvent {
    /// The WAL position of this change. The client checkpoints this once it
    /// has applied the event, enabling incremental resume.
    pub lsn: Lsn,
    /// The row mutation.
    pub op: RowOp,
    /// Optional transaction id — events sharing a txn id belong to one
    /// atomic Postgres transaction and should be applied as a batch.
    pub txn_id: Option<u64>,
}

impl ReplicationEvent {
    #[inline]
    #[must_use]
    pub fn new(lsn: Lsn, op: RowOp) -> Self {
        Self {
            lsn,
            op,
            txn_id: None,
        }
    }

    #[inline]
    #[must_use]
    pub fn with_txn(mut self, txn_id: u64) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    /// The table this event's op targets.
    #[inline]
    #[must_use]
    pub fn table(&self) -> &str {
        self.op.table()
    }

    /// Approximate payload weight.
    #[inline]
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.op.payload_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn table_pk_extraction() {
        let ins = RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: payload(8),
        };
        assert_eq!(ins.table(), "tasks");
        assert_eq!(ins.pk(), "1");
        assert_eq!(ins.operation(), Operation::Insert);
        assert!(ins.has_payload());
        assert_eq!(ins.payload_len(), 8);

        let del = RowOp::Delete {
            table: "tasks".into(),
            pk: "1".into(),
        };
        assert!(!del.has_payload());
        assert_eq!(del.payload_len(), 0);
    }

    #[test]
    fn event_carries_lsn_and_txn() {
        let e = ReplicationEvent::new(
            Lsn::new(42),
            RowOp::Delete {
                table: "t".into(),
                pk: "x".into(),
            },
        )
        .with_txn(99);
        assert_eq!(e.lsn, Lsn::new(42));
        assert_eq!(e.table(), "t");
        assert_eq!(e.txn_id, Some(99));
    }

    #[test]
    fn bytes_payload_is_shared_not_copied() {
        // Demonstrates the cheap-clone property: cloning a RowOp with a 1MB
        // payload should not allocate 1MB again — Bytes is Arc-backed.
        let big = RowOp::Insert {
            table: "t".into(),
            pk: "p".into(),
            payload: payload(1_000_000),
        };
        let cloned = big.clone();
        // Both reference the same allocation — payload bytes are shared.
        match (&big, &cloned) {
            (RowOp::Insert { payload: a, .. }, RowOp::Insert { payload: b, .. }) => {
                // `Bytes::as_ptr` points at the same backing buffer when shared.
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            _ => unreachable!(),
        }
    }
}
