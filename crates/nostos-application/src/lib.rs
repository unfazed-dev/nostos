//! # nostos-application
//!
//! The application layer: **use-cases** (what the system *does*) and **ports**
//! (driven-side interfaces the infra layer implements). Depends only on
//! [`nostos-domain`]. This is where the fan-out hot loop lives.
//!
//! ## Ports
//!
//! - [`ReplicatorStream`] — the source of replication events (real Postgres
//!   logical replication in prod; a `FakeReplicator` in the benchmark).
//! - [`EventSink`] — a delivery target for one session (a tokio channel in prod;
//!   a recording sink in unit tests).
//! - [`SessionStore`] — the live set of sessions, indexed by predicate table so
//!   [`FanOutService`] can prune candidates in O(1).
//!
//! ## Use-cases
//!
//! - [`FanOutService`] — *the throughput moat*: pull events from the
//!   replicator, evaluate predicates via the store, deliver to matching sinks.
//! - [`SessionManager`] — connect/disconnect sessions as clients open/close.
//!
//! The hot loop never references `tokio` directly — it goes through the
//! [`EventSink`] trait, so the same code runs in the server (tokio sink) and in
//! unit tests (recording sink).

#![forbid(unsafe_code)]

pub mod eviction;
pub mod fanout;
pub mod ports;
pub mod session;

pub use eviction::EvictionPolicy;
pub use fanout::{FanOutOutcome, FanOutService};
pub use ports::{
    DeliveryDecision, EventSink, Metrics, MetricsSnapshot, ReplicatorStream, SessionCandidate,
    SessionStore, StoreRejection, SyncAuth, WriteBack, WriteBackError,
};
pub use session::SessionManager;

// Re-export the domain types the application surface needs so downstream crates
// can depend on `nostos-application` alone for the public API.
pub use nostos_domain::{Lsn, Predicate, Principal, ReplicationEvent, SessionId, SyncSession};
