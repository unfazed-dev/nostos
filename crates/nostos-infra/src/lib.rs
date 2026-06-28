//! # nostos-infra
//!
//! Infrastructure adapters — the **only** place `tokio`, `axum`, and `postgres`
//! appear. Each module implements one [`nostos_application`] port.
//!
//! | Module | Implements | Notes |
//! |---|---|---|
//! | [`store`] | `SessionStore` | `DashMap`-indexed-by-table in-memory store |
//! | [`router`] | `EventSink` | Bounded per-session tokio channel with drop-on-full backpressure |
//! | [`replicator`] | `ReplicatorStream` | `FakeReplicator` (Week 1) + `PgReplicator` stub (Week 2) |
//! | [`wire`] | — | `ReplicationEvent` ↔ JSON/binary frame codec |
//! | [`transport`] | — | axum WebSocket server adapter |
//!
//! The benchmark drives a `FakeReplicator` through the *real* `FanOutService`
//! and `TokioEventSink`, so what we measure is the production pipeline.

#![forbid(unsafe_code)]

pub mod auth;
pub mod replicator;
pub mod router;
pub mod store;
pub mod transport;
pub mod wire;

pub use auth::{AllowAnonymous, SupabaseJwtAuth};
pub use replicator::{FakeReplicator, FakeReplicatorConfig};
pub use router::TokioEventSink;
pub use store::InMemorySessionStore;
pub use transport::SyncRouterState;
pub use wire::{decode_client_message, encode_event, ClientMessage, FilterClause, WireFrame};

#[cfg(feature = "pg")]
pub use replicator::{PgReplicator, PgReplicatorConfig, PgReplicatorError};
