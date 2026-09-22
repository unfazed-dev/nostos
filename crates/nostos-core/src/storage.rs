//! The client-side storage seam.
//!
//! A [`Storage`] is what the client applies replication frames to. The trait is
//! deliberately tiny — **two methods** — because everything that matters for
//! correctness collapses into one property: *the LSN checkpoint and the row
//! writes land in the same atomic transaction*. If those can race, a crash
//! between them either loses data (checkpoint advanced past un-applied rows) or
//! replays it (checkpoint behind applied rows, forcing redo). Putting both in
//! `apply_batch` makes that guarantee structural, not conventional.
//!
//! This trait lives in `nostos-core` (not `nostos-client`) because it must be
//! implementable on WASM (`-sqlite-wasm` / OPFS), on Flutter's
//! `sqlite3_flutter_libs`, and on native `rusqlite` — none of which see tokio.
//! The async [`crate::ApplyEngine`] drives a synchronous `Storage` via
//! `spawn_blocking` on platforms that have it; on WASM the apply runs inline.
//!
//! `apply_batch` stores the opaque logical-replication tuple image (the
//! `RowOp` payload bytes) per `(table, pk)`. A column-level decoder + schema
//! registry arrives with the dynamic predicate engine (ADR-0012); until then
//! the stored bytes are durable and resumable but not SQL-queryable. That is an
//! honest scoping: the wire delivers opaque bytes today, so storage mirrors it.

use std::collections::HashSet;

use nostos_domain::{Lsn, RowOp};

/// An error applying a batch. Backend-specific failure modes (SQLite busy, disk
/// full, OPFS quota) are collapsed into [`Self::Backend`]; the engine treats all
/// of them as "this batch did not commit, do not advance the checkpoint."
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The backend rejected the batch (busy, quota, corruption, …). No row was
    /// committed — the checkpoint MUST NOT advance.
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Result alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// What a client persists replicated rows + its resume checkpoint to.
///
/// Implementations:
/// - [`crate::InMemoryStorage`] — the test double (and the contract reference).
/// - `SqliteStorage` in `nostos-client` — real `rusqlite` persistence.
/// - future: `-sqlite-wasm` (OPFS), RN `op-sqlite`, Flutter
///   `sqlite3_flutter_libs` adapters (ADR-0015).
///
/// All methods are synchronous and infallible on `&self` reads. A `Storage`
/// owns its connection; concurrent apply from multiple threads is the caller's
/// responsibility (the client apply loop is single-threaded by construction).
pub trait Storage {
    /// Read the durable last-applied LSN. On a fresh database this is
    /// [`Lsn::ZERO`] — the client will subscribe with no `resume_lsn` and
    /// receive the full snapshot.
    ///
    /// Called once per (re)connect to seed the `Subscribe` frame.
    fn checkpoint(&self) -> crate::Result<Lsn>;

    /// Read the durable last-seen server slot epoch (ADR-0025 reconnect-resume
    /// gate). On a fresh database this is `0` — the client sends `epoch: None`
    /// and the server treats it as a mismatch (full snapshot). Updated from the
    /// server's `resume_info` frame.
    ///
    /// Default `Ok(0)` — backends that don't persist epoch behave as a fresh
    /// client (snapshot on every reconnect) until overridden.
    fn epoch(&self) -> crate::Result<u64> {
        Ok(0)
    }

    /// Persist the server's current slot epoch. Called whenever the server
    /// advertises a new epoch via `resume_info`. Default no-op — backends that
    /// don't persist epoch simply won't resume-by-replay (snapshot fallback).
    fn save_epoch(&self, _epoch: u64) -> crate::Result<()> {
        Ok(())
    }

    /// The rules checksum this client last synced under (ADR-0031 D2).
    /// `0` = unknown (fresh DB, or a storage that does not persist it) → the
    /// Subscribe omits the field → server uses the composed-epoch fallback.
    ///
    /// Default `Ok(0)` — exactly like [`Self::epoch`]: a backend that doesn't
    /// override this stays on the composed-epoch fallback forever (never
    /// wrong, just never gets the log-attribution benefit of the explicit
    /// path).
    fn rules_checksum(&self) -> crate::Result<u64> {
        Ok(0)
    }

    /// Persist the rules checksum advertised in `resume_info`. Non-fatal on
    /// failure — mirrors [`Self::save_epoch`]: a persist failure costs one
    /// extra snapshot next reconnect, it must never kill the session. Default
    /// no-op — backends that don't persist the checksum simply never leave
    /// the composed-epoch fallback.
    fn save_rules_checksum(&self, _checksum: u64) -> crate::Result<()> {
        Ok(())
    }

    /// The `xid8` snapshot horizon this client last pulled to in direct mode
    /// (`crate::pull`), or `None` on a fresh database — the caller then starts
    /// from [`crate::Horizon::fresh`].
    ///
    /// Opaque text, never a number: the client does no arithmetic on it, and
    /// routing an `xid8` through a JS `number` is silently wrong past 2^53.
    /// Sits beside the LSN checkpoint rather than replacing it — server mode
    /// resumes from the LSN, direct mode from this, and a database that has
    /// synced in both modes holds both.
    ///
    /// Default `Ok(None)` — a backend that doesn't override it re-pulls the
    /// whole retained log on every launch. Same "defaults degrade, never lie"
    /// stance as [`Self::epoch`].
    fn horizon(&self) -> crate::Result<Option<String>> {
        Ok(None)
    }

    /// Persist the horizon. Called *after* the batch it covers has committed,
    /// so a crash in the window leaves the horizon behind the rows and the next
    /// pull re-applies them idempotently. Advancing it first would skip rows
    /// that never landed.
    ///
    /// Non-fatal on failure, exactly like [`Self::save_epoch`]: the cost is a
    /// re-read, and it must never kill a commit that already succeeded. Default
    /// no-op.
    fn save_horizon(&mut self, _horizon: &str) -> crate::Result<()> {
        Ok(())
    }

    /// Atomically apply a batch of row operations and advance the checkpoint.
    ///
    /// **Atomicity contract:** every `op` in `ops` AND the checkpoint advance to
    /// `checkpoint` commit together, or none of them do. A successful return
    /// means the rows are durable *and* the checkpoint reflects them; a
    /// failure means neither happened — the caller retries the whole batch.
    ///
    /// **Monotonicity:** the engine passes the highest LSN seen in the batch
    /// (or the batch's commit-LSN). A backend MAY no-op the checkpoint write if
    /// `checkpoint` is `<=` the stored value — the contract is "after this call,
    /// [`Self::checkpoint`] returns `>= checkpoint`," not "exactly checkpoint."
    ///
    /// **Idempotency:** re-applying the same `RowOp` (same table + pk) is a
    /// no-op-equivalent upsert — last-writer-wins by WAL order (ADR-0014 tier
    /// (a)). This is what makes reconnect replay safe.
    ///
    /// **Per-row LSN gating (ADR-0025 slice 4a):** each entry in `ops` carries
    /// its source LSN. An upsert applies only if `lsn >= row.applied_lsn` (a
    /// stale replay/live op must not overwrite a newer row); a delete applies
    /// only if `row.applied_lsn <= lsn` (a stale delete must not drop a newer
    /// row). This is what makes concurrent op-log replay + live fan-out safe
    /// when they interleave out of order on the same pk.
    ///
    /// **Snapshot windows (design D):** `snapshot_tables` names tables whose
    /// snapshot-reconcile window is open (a `snapshot_begin{T}` was seen without
    /// its matching `end`). Ops on such a table apply UNCONDITIONALLY — the
    /// snapshot is authoritative current-state whose synthetic-LSN rows must
    /// clobber stored rows regardless of the persisted `applied_lsn` — and still
    /// stamp `applied_lsn = lsn`. Synthetic vs real LSNs never mix: snapshot
    /// phase is unconditional; live/replay phase is always `>=`-gated.
    fn apply_batch(
        &mut self,
        ops: &[(RowOp, u64)],
        checkpoint: Lsn,
        snapshot_tables: &HashSet<String>,
    ) -> crate::Result<()>;

    /// Enumerate every primary key the client currently holds for `table`.
    ///
    /// The snapshot-reconcile path (ADR-0014 offline-delete fix) uses this at
    /// `snapshot_begin` to seed the orphan-candidate set: every local PK that
    /// the server's snapshot does NOT re-confirm is a row that was hard-deleted
    /// server-side while the client was offline, and MUST be reaped at
    /// `snapshot_end`. Read-only and infallible at the trait level — a backend
    /// read failure surfaces as [`StorageError::Backend`].
    fn pks_for_table(&self, table: &str) -> crate::Result<Vec<String>>;

    /// Read the opaque payload bytes for one row `(table, pk)`, or `None` if the
    /// row is absent. Used by the PN-Counter CRDT's client-side read-modify-write
    /// (ADR-0030 addendum): `counter_increment` reads the current counter
    /// payload, applies the delta to this replica's entry, and enqueues the
    /// result so the per-replica max merge converges (a bare delta would be
    /// clobbered by a concurrent delta from the same replica).
    ///
    /// Default `Ok(None)` — a backend that doesn't override it has no counter
    /// read-modify-write (the client's counter methods will treat every
    /// increment as a first-write, which is correct for a single-replica counter
    /// but loses same-replica concurrency). `SqliteStorage` and
    /// `InMemoryStorage` override with a real read.
    fn read_payload(&self, _table: &str, _pk: &str) -> crate::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Bulk-delete the rows identified by `pks` from `table`. Used by the
    /// snapshot-reconcile `end` step to reap orphans — PKs that were local at
    /// `begin` but absent from the snapshot. Idempotent: deleting a pk that's
    /// already gone is a no-op (mirrors `apply_batch`'s Delete semantics).
    ///
    /// Implementations SHOULD apply the deletes atomically (one transaction for
    /// the whole batch) so a partial failure leaves the local image in a
    /// known-consistent state. The reconcile path calls this outside
    /// `apply_batch` — it is a separate atomic op, not part of the row-apply
    /// transaction — so a backend that auto-commits per call (e.g. SQLite
    /// outside an explicit tx) is acceptable.
    fn delete_pks(&mut self, table: &str, pks: &[String]) -> crate::Result<()>;

    /// Wipe ALL local state for sign-out / principal switch (ADR-0029).
    ///
    /// Resets the store to a fresh-database state: no rows, checkpoint
    /// [`Lsn::ZERO`], epoch `0`, and (on a backend that bundles the outbox in
    /// the same physical store, like `SqliteStorage`) a drained outbox +
    /// dead-letter queue. After `clear()`, the next (re)connect MUST behave as
    /// a brand-new client: subscribe with no `resume_lsn` and take the full
    /// snapshot.
    ///
    /// **REQUIRED, not defaulted.** A no-op default would be a silent
    /// cross-user data leak — the same "defaults degrade" trap as the other
    /// defaulted methods (ADR-0025). Every impl MUST wipe its own surface; the
    /// compiler enforces parity.
    ///
    /// **The load-bearing detail:** the checkpoint MUST reset to `0`, not
    /// merely have its rows deleted. A stale checkpoint makes the next
    /// principal resume from the old LSN, never receive a snapshot, and see an
    /// empty database permanently — the resume-without-snapshot unsoundness
    /// class.
    fn clear(&mut self) -> crate::Result<()>;
}
