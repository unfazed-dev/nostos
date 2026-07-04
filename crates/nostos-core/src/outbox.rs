//! The client outbox — a durable queue of local writes awaiting server
//! acknowledgment (ADR-0013 v1).
//!
//! This is the *write* half of the client's two durability surfaces, sibling to
//! [`crate::Storage`] (the read/apply half). The same property that makes
//! `Storage` load-bearing — *the row writes and the checkpoint land in one
//! atomic transaction* — applies here: an enqueued write MUST be durable before
//! `enqueue` returns, so a crash between a user action and server ack can't
//! strand the intent. [`Outbox`] captures that contract in three methods.
//!
//! ## Why a separate trait
//!
//! The outbox is conceptually distinct from `Storage` even when one physical
//! database implements both (the recommended shape — see [`Outbox`]'s docs): the
//! apply loop *reads* from the wire and writes to `Storage`; the flush loop
//! *reads* from the outbox and writes to the wire. Keeping them as separate
//! traits lets a future backend split them (e.g. an in-memory outbox for tests,
//! or a different table family) without entangling the two loops. Today
//! `SqliteStorage` implements BOTH against the same SQLite file so a crash can't
//! strand one without the other.
//!
//! ## WASM-clean
//!
//! Like the rest of `nostos-core`, this module is pure Rust: no tokio, no
//! SQLite. The trait is synchronous (the flush loop is single-threaded by
//! construction, mirroring the apply loop); the native `rusqlite` impl lives in
//! `nostos-client`.

/// A durable queue of local writes awaiting server acknowledgment (ADR-0013 v1).
/// Same-crate sibling of [`crate::Storage`]; implementations SHOULD persist both
/// in the same database so a crash can't strand one without the other.
///
/// The three methods form the queue's lifecycle:
/// - [`Outbox::enqueue`] captures a local write durably (it returns before any
///   network round-trip — the caller never blocks on the server to record user
///   intent).
/// - [`Outbox::pending`] snapshots every unacknowledged write, oldest first, so
///   the flush loop can drain them in order after a (re)connect.
/// - [`Outbox::mark_done`] removes an acknowledged write (the server returned
///   `WriteResult{ok:true}`).
///
/// A write that comes back `ok:false` is NOT removed — it stays at the queue
/// head and is retried on the next flush. This is deliberate: a transient
/// rejection (constraint violation under a race, a momentarily-unwritable
/// table) should not silently drop user intent.
///
/// ponytail: failed writes retry forever and block the queue head; add a
/// dead-letter policy when a design partner hits a permanent rejection.
pub trait Outbox {
    /// Enqueue a local write. Returns its monotonically increasing id.
    ///
    /// **Durability contract:** the write is durable (survives a crash) before
    /// this returns. An `Err` means it did NOT land — the caller MUST surface
    /// that to the user (the write was not captured).
    fn enqueue(&mut self, write: PendingWrite) -> crate::Result<u64>;

    /// All writes not yet acknowledged, oldest first. Each entry is `(id, write)`
    /// so the flush loop can correlate the server's `WriteResult.client_write_id`
    /// back to the queued row (the id is the correlation key on the wire).
    fn pending(&self) -> crate::Result<Vec<(u64, PendingWrite)>>;

    /// Remove an acknowledged write. Called after `WriteResult{ok:true}`.
    /// Removing an unknown id is a no-op (idempotent — a redelivery after a
    /// partial flush must not error).
    fn mark_done(&mut self, id: u64) -> crate::Result<()>;
}

/// One local write awaiting server ack. The client's *write intent* — distinct
/// from the replication `Operation` (which is a server-originated event). This
/// is what the user did; the server's `WriteResult` + the round-tripped
/// replication frame are the response.
///
/// `payload_json` is the JSON object tuple-image (the same shape the read path
/// delivers and `PgWriteBack` consumes). It's `None` for deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    /// Target table — MUST be in the server's `NOSTOS_WRITE_TABLES` allowlist.
    pub table: String,
    /// Upsert or delete.
    pub op: WriteOp,
    /// Primary-key value (v1 convention: pk column is `id`).
    pub pk: String,
    /// The row image for an upsert (a JSON object of column → value), or `None`
    /// for a delete.
    pub payload_json: Option<String>,
}

/// What the client wants to do to a row. The wire string (`"upsert"` /
/// `"delete"`) is derived from this via [`WriteOp::as_wire_str`].
///
/// This is NOT `nostos_domain::Operation` (insert/update/delete) — those are
/// server-originated replication events. The outbox carries the client's
/// coarser intent: an upsert is "make this row look like this," which the
/// server maps to an INSERT … ON CONFLICT DO UPDATE regardless of whether the
/// row pre-existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    /// Insert-or-update (the server applies an upsert; last-writer-wins by WAL
    /// order — ADR-0014).
    Upsert,
    /// Delete by primary key (idempotent — a missing row is success).
    Delete,
}

impl WriteOp {
    /// The wire string the `Write` frame's `op` field carries. Matches the
    /// `dispatch_write` op match in the server transport (`"upsert" | "delete"`).
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            WriteOp::Upsert => "upsert",
            WriteOp::Delete => "delete",
        }
    }
}

impl WriteOp {
    /// Parse a wire `op` string back into the enum. Returns `None` for anything
    /// other than `"upsert"` / `"delete"` (the server's `dispatch_write` rejects
    /// the same set as `InvalidPayload`).
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "upsert" => Some(WriteOp::Upsert),
            "delete" => Some(WriteOp::Delete),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_op_wire_roundtrip() {
        for op in [WriteOp::Upsert, WriteOp::Delete] {
            let s = op.as_wire_str();
            assert_eq!(WriteOp::from_wire_str(s), Some(op));
        }
    }

    #[test]
    fn write_op_rejects_unknown_wire_string() {
        assert_eq!(WriteOp::from_wire_str("insert"), None);
        assert_eq!(WriteOp::from_wire_str("update"), None);
        assert_eq!(WriteOp::from_wire_str(""), None);
    }

    #[test]
    fn pending_write_eq() {
        let a = PendingWrite {
            table: "tasks".into(),
            op: WriteOp::Upsert,
            pk: "1".into(),
            payload_json: Some(r#"{"x":1}"#.into()),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
