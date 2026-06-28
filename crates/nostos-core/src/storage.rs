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
    fn apply_batch(&mut self, ops: &[RowOp], checkpoint: Lsn) -> crate::Result<()>;
}
