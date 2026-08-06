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
//! [`docs/ARCHITECTURE.md`]: https://github.com/unfazed-dev/nostos/blob/main/docs/ARCHITECTURE.md

#![forbid(unsafe_code)]

pub mod crdt;
pub mod events;
mod fnv;
pub mod lsn;
pub mod predicate;
pub mod predicate_compile;
pub mod principal;
pub mod rules;
pub mod scope;
pub mod session;
pub mod sync_epoch;
pub mod tier;

pub use crdt::{
    merge_or_set_or_lww, merge_or_set_payloads, present_elements, Hlc, OrSetElement, OrSetError,
    OrSetPayload,
};
pub use events::{Operation, ReplicationEvent, RowOp};
pub use lsn::Lsn;
pub use predicate::{ColumnValue, Predicate, PredicateExpr, PredicateFilter};
pub use predicate_compile::{parse_predicate_expr, ParseError};
pub use principal::{Principal, TenantScope};
pub use rules::{HandRule, RulesError, SyncMode, SyncRules, TableRule, RULES_VERSION};
pub use scope::{ScopeError, ScopeExpr, ScopeOp, ScopeTerm, ScopeValue};
pub use session::{SessionId, SyncSession};
pub use sync_epoch::compose_sync_epoch;
pub use tier::Tier;

/// Convenience: the canonical "tasks" table name used by the benchmark workload.
pub const TASKS_TABLE: &str = "tasks";
