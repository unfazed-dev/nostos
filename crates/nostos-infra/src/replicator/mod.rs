//! Replicator adapters — implement [`ReplicatorStream`].
//!
//! - [`FakeReplicator`] generates synthetic WAL events on demand. Used by the
//!   Week-1 benchmark to drive the *real* fan-out pipeline without a Postgres.
//! - [`PgReplicator`] (behind the `pg` feature) consumes real logical
//!   replication via `pgwire-replication` + `pgoutput` — the Phase-1 moat.
//!
//! The benchmark's whole point: because both implement the same port, the
//! throughput we measure with `FakeReplicator` is the *router's* ceiling, not
//! Postgres's. Swap in `PgReplicator` and the number changes only by the cost
//! of pgoutput parsing.

pub mod fake;

#[cfg(feature = "pg")]
pub mod pg;

pub use fake::{FakeReplicator, FakeReplicatorConfig};

#[cfg(feature = "pg")]
pub use pg::{PgReplicator, PgReplicatorConfig, PgReplicatorError};
