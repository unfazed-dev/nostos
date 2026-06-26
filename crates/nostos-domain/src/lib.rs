//! # nostos-domain
//!
//! The pure business core of Nostos. **Zero I/O, zero async, zero framework
//! dependencies.** Types + invariants only.
//!
//! This is the innermost ring of the hexagonal architecture (see
//! [`docs/ARCHITECTURE.md`]). It must compile and test without a runtime. The
//! application layer defines ports over these types; the infra layer implements
//! them. Nothing here may depend on anything above.
//!
//! [`docs/ARCHITECTURE.md`]: https://github.com/nostos-sync/nostos/blob/main/docs/ARCHITECTURE.md

#![forbid(unsafe_code)]

pub mod events;
pub mod lsn;
pub mod predicate;
pub mod session;

pub use events::{Operation, ReplicationEvent, RowOp};
pub use lsn::Lsn;
pub use predicate::{ColumnValue, Predicate, PredicateFilter};
pub use session::{SessionId, SyncSession};

/// Convenience: the canonical "tasks" table name used by the benchmark workload.
pub const TASKS_TABLE: &str = "tasks";
