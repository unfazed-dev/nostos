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

/// Initial table snapshot (COPY under the slot's exported snapshot). Lives
/// behind the `pg` feature with the rest of the replication adapter.
#[cfg(feature = "pg")]
mod snapshot;

/// OID-keyed JSON value mapping (ADR-0019), shared by `pg`, `snapshot`, and
/// the subscribe-time `PgSnapshotter` so a row renders byte-identically
/// regardless of which path produced it. `pub(crate)` so the snapshot-on-
/// subscribe adapter (`snapshot_source.rs`) can reuse it without re-implementing
/// the type table (divergence would make a snapshot row subtly different from a
/// streamed row — the exact bug ADR-0019 exists to prevent).
#[cfg(feature = "pg")]
pub(crate) mod typed;

/// Column extraction for the predicate `matches` seam (ADR-0012 slice 2).
/// Always available — pure JSON parsing, no `pg` feature required.
pub mod extract;

pub use extract::extract_json_column;
pub use fake::{FakeReplicator, FakeReplicatorConfig};

#[cfg(feature = "pg")]
pub use pg::{PgReplicator, PgReplicatorConfig, PgReplicatorError};
