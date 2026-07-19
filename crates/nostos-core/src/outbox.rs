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
/// ## Dead-letter policy (ADR-0013 v2)
///
/// A *permanently* failing write (server bug, schema drift, a row the principal
/// will never be allowed to write) would otherwise retry forever and block the
/// queue head — every subsequent write piles up behind it and never reaches the
/// wire. The dead-letter policy caps this: the flush loop calls
/// [`Outbox::bump_attempts`] on every `ok:false`; once the count reaches the
/// configured `dead_letter_max_attempts`, it calls [`Outbox::mark_dead_letter`]
/// to quarantine the write (it stays in the backing store for inspection/replay
/// but is excluded from [`Outbox::pending`]), so the queue head advances past
/// it on the next flush. The write is never silently deleted — a dead-letter is
/// a visible, operator-inspectable state, not data loss.
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
    ///
    /// **Dead-letter exclusion:** implementations that support the dead-letter
    /// policy MUST exclude dead-lettered rows here (a dead-lettered write is no
    /// longer "pending" — it has been quarantined so the queue head can advance).
    fn pending(&self) -> crate::Result<Vec<(u64, PendingWrite)>>;

    /// Remove an acknowledged write. Called after `WriteResult{ok:true}`.
    /// Removing an unknown id is a no-op (idempotent — a redelivery after a
    /// partial flush must not error).
    fn mark_done(&mut self, id: u64) -> crate::Result<()>;

    /// Bump the retry counter for a rejected write and return the NEW attempt
    /// count (post-increment). Called from the flush loop on every
    /// `WriteResult{ok:false}` so a permanently-failing write can be
    /// quarantined after a bounded number of retries rather than blocking the
    /// queue head forever (ADR-0013 v2 dead-letter policy).
    ///
    /// **Default:** no-op tracking — returns `Ok(0)`, so the write stays
    /// retryable indefinitely (the pre-DLQ behavior). The flush loop dead-
    /// letters when `count >= dead_letter_max_attempts`; a no-op default
    /// therefore never triggers dead-lettering (0 is below any positive max),
    /// which is the correct behavior for a backend (e.g. `InMemoryStorage`)
    /// that doesn't persist retry state. Backends that DO persist retry state
    /// (`SqliteStorage`) override this to increment-and-return a real counter.
    ///
    /// Takes `&self` (not `&mut self`) because the durable backend uses a
    /// `Mutex<Connection>` for interior mutability — matching [`Self::pending`]
    /// (`&self`) rather than [`Self::mark_done`] (`&mut self`). Either shape
    /// works on `SqliteStorage`; `&self` was chosen so a backend that keeps the
    /// counter in a separate, concurrently-accessed store can implement it
    /// without exclusive access.
    fn bump_attempts(&self, _id: u64) -> crate::Result<u32> {
        Ok(0)
    }

    /// Mark a write as permanently failed (dead-lettered). A dead-lettered
    /// write MUST be excluded from subsequent [`Self::pending`] calls (the
    /// queue head advances past it); it is NOT deleted — it remains in the
    /// backing store for inspection/replay (e.g. `SqliteStorage::
    /// dead_letter_entries`). Idempotent: marking an already-dead-lettered id
    /// is a no-op (a redelivery after a partial flush must not error).
    ///
    /// **Default:** no-op. A backend that overrides this MUST also make
    /// `pending()` exclude dead-lettered rows (the two are a contract pair).
    /// Returns `Ok(())` so the flush loop never fails on the quarantine path —
    /// a failure to dead-letter leaves the write retryable, which is safe (the
    /// queue head just doesn't advance past it yet).
    fn mark_dead_letter(&self, _id: u64) -> crate::Result<()> {
        Ok(())
    }

    /// Apply `write` to the local data store IMMEDIATELY (optimistic), WITHOUT
    /// advancing the replication checkpoint. This is the "instant-local write"
    /// that gives the offline-first feel: the user's write renders locally
    /// before any server round-trip. The server's echo later UPSERTs the
    /// authoritative image on top (last-writer-wins reconcile).
    ///
    /// Distinct from [`crate::Storage::apply_batch`], which advances the
    /// checkpoint and is reserved for server-confirmed replication events.
    /// `apply_local` writes the same row but leaves the cursor untouched — the
    /// row is the user's intent, not yet a confirmed fact.
    ///
    /// **Default:** no-op. Instant-local is an optimization, not a correctness
    /// requirement: a backend that doesn't override it simply shows the row on
    /// the server's echo instead of instantly (the write is still queued by
    /// [`Self::enqueue`], so nothing is lost). Backends with a data store
    /// (`SqliteStorage`, `InMemoryStorage`) override it to write the row now.
    ///
    /// **Best-effort by contract:** the client `write` path enqueues FIRST and
    /// treats an `Err` here as "visibility delayed to echo," not as a write
    /// failure — the outbox entry is already durable before this runs.
    fn apply_local(&mut self, _write: &PendingWrite) -> crate::Result<()> {
        Ok(())
    }

    /// The PKs of this table's unacknowledged local writes (derived from
    /// [`Self::pending`]). The snapshot-reconcile orphan-reap (ADR-0025 hole #1)
    /// uses this to EXEMPT the user's own optimistic writes: a pending-local
    /// row sits in the data store before the server echoes it, so it is absent
    /// from a server snapshot but is NOT an orphan — reaping it would delete
    /// the user's unsynced work on every (re-)subscribe with a pending outbox.
    ///
    /// Default derives from [`Self::pending`] (correct for every backend;
    /// override only if a direct query is materially cheaper than materializing
    /// the full pending list).
    fn pending_pks_for_table(&self, table: &str) -> crate::Result<Vec<String>> {
        let mut pks: Vec<String> = self
            .pending()?
            .into_iter()
            .filter(|(_, w)| w.table == table)
            .map(|(_, w)| w.pk)
            .collect();
        pks.sort();
        pks.dedup();
        Ok(pks)
    }
}

/// One local write awaiting server ack. The client's *write intent* — distinct
/// from the replication `Operation` (which is a server-originated event). This
/// is what the user did; the server's `WriteResult` + the round-tripped
/// replication frame are the response.
///
/// `payload_json` is the JSON object tuple-image (the same shape the read path
/// delivers and `PgWriteBack` consumes). It's `None` for deletes; for an upsert
/// it's the full row, for a patch it's the partial column set to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    /// Target table — MUST be in the server's `NOSTOS_WRITE_TABLES` allowlist.
    pub table: String,
    /// Upsert, delete, or patch.
    pub op: WriteOp,
    /// Primary-key value (v1 convention: pk column is `id`).
    pub pk: String,
    /// The row image for an upsert (a JSON object of column → value), or `None`
    /// for a delete.
    pub payload_json: Option<String>,
}

/// What the client wants to do to a row. The wire string (`"upsert"` /
/// `"delete"` / `"patch"`) is derived from this via [`WriteOp::as_wire_str`].
///
/// This is NOT `nostos_domain::Operation` (insert/update/delete) — those are
/// server-originated replication events. The outbox carries the client's
/// coarser intent: an upsert is "make this row look like this," which the
/// server maps to an INSERT … ON CONFLICT DO UPDATE regardless of whether the
/// row pre-existed; a patch is "change only these columns of an existing row."
///
/// Patch matches PowerSync's PATCH op-type (P3 parity,
/// `docs/plans/powersync-sdk-parity-plan.md`): column-level LWW, idempotent,
/// never inserts. "Deletes always win" — a patch and a delete racing on the
/// same pk resolve to the row being gone (delete is terminal); a patch of an
/// absent row is a no-op success (mirrors delete-of-missing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    /// Insert-or-update (the server applies an upsert; last-writer-wins by WAL
    /// order — ADR-0014).
    Upsert,
    /// Delete by primary key (idempotent — a missing row is success).
    Delete,
    /// Column-level UPDATE of an existing row (no insert). The payload carries
    /// only the columns to change; columns absent from the payload are
    /// untouched. A patch of a row that does not exist is success (idempotent);
    /// under tenant scoping (ADR-0018) a patch of a row that exists under a
    /// different tenant is a `Forbidden` rejection, never a silent no-op.
    Patch,
}

impl WriteOp {
    /// The wire string the `Write` frame's `op` field carries. Matches the
    /// `dispatch_write` op match in the server transport
    /// (`"upsert" | "delete" | "patch"`).
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            WriteOp::Upsert => "upsert",
            WriteOp::Delete => "delete",
            WriteOp::Patch => "patch",
        }
    }
}

impl WriteOp {
    /// Parse a wire `op` string back into the enum. Returns `None` for anything
    /// other than `"upsert"` / `"delete"` / `"patch"` (the server's
    /// `dispatch_write` rejects the same set as `InvalidPayload`).
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "upsert" => Some(WriteOp::Upsert),
            "delete" => Some(WriteOp::Delete),
            "patch" => Some(WriteOp::Patch),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_op_wire_roundtrip() {
        for op in [WriteOp::Upsert, WriteOp::Delete, WriteOp::Patch] {
            let s = op.as_wire_str();
            assert_eq!(WriteOp::from_wire_str(s), Some(op));
        }
    }

    #[test]
    fn write_op_rejects_unknown_wire_string() {
        assert_eq!(WriteOp::from_wire_str("insert"), None);
        assert_eq!(WriteOp::from_wire_str("update"), None);
        assert_eq!(WriteOp::from_wire_str("put"), None);
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
