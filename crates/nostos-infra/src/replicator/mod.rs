//! Replicator adapters — implement [`ReplicatorStream`].
//!
//! - [`FakeReplicator`] generates synthetic WAL events on demand. Used by the
//!   Week-1 benchmark to drive the *real* fan-out pipeline without a Postgres.
//! - `PgReplicator` (behind the `pg` feature, Week 2) consumes real logical
//!   replication via `pgoutput`.
//!
//! The benchmark's whole point: because both implement the same port, the
//! throughput we measure with `FakeReplicator` is the *router's* ceiling, not
//! Postgres's. Swap in `PgReplicator` later and the number changes only by the
//! cost of pgoutput parsing.

pub mod fake;

pub use fake::{FakeReplicator, FakeReplicatorConfig};
