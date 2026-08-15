//! # nostos-infra
//!
//! Infrastructure adapters — the **only** place `tokio`, `axum`, and `postgres`
//! appear. Each module implements one [`nostos_application`] port.
//!
//! | Module | Implements | Notes |
//! |---|---|---|
//! | [`store`] | `SessionStore` | `DashMap`-indexed-by-table in-memory store |
//! | [`router`] | `EventSink` | Bounded per-session tokio channel with drop-on-full backpressure |
//! | [`replicator`] | `ReplicatorStream` | `PgReplicator` — real pgoutput logical replication (feature "pg"); `FakeReplicator` — synthetic WAL generator for benches/tests. |
//! | [`wire`] | — | `ReplicationEvent` ↔ JSON/binary frame codec |
//! | [`transport`] | — | axum WebSocket server adapter |
//! | [`token_store`] | — (inherent; ADR-0037) | `PgTokenStore` — push-token registry (feature "pg") |
//!
//! The benchmark drives a `FakeReplicator` through the *real* `FanOutService`
//! and `TokioEventSink`, so what we measure is the production pipeline.

#![forbid(unsafe_code)]

pub mod auth;
mod jwks;
/// Persisted operation-log writers (ADR-0025 slice 2). `RecordingOpLogWriter`
/// (always available, in-memory — bench/test) + `PgOpLogWriter` (feature "pg").
pub mod oplog;
pub mod replicator;
pub mod router;
/// `nostos_rules.toml` load/save (ADR-0031, Task 7). No `pg` feature gate —
/// the TOML shape and `SyncRules` domain type are usable without Postgres.
pub mod rules_file;
/// `#[cfg(feature = "pg")]` — the typed-schema endpoint adapter (WS1). Absent
/// without the `pg` feature (the server leaves `schema_source = None` then).
#[cfg(feature = "pg")]
pub mod schema_source;
/// `#[cfg(feature = "pg")]` — the snapshot-on-subscribe adapter. Absent
/// without the `pg` feature (the transport leaves `snapshotter = None` then).
#[cfg(feature = "pg")]
pub mod snapshot_source;
pub mod store;
/// `#[cfg(feature = "pg")]` — the push-token registry adapter (ADR-0037 §3).
/// Absent without the `pg` feature. Inherent methods only — no port trait
/// until a second implementation or a test seam demands one.
#[cfg(feature = "pg")]
pub mod token_store;
pub mod transport;
pub mod wire;
pub mod write_back;

pub use auth::{AllowAnonymous, SupabaseJwtAuth};
pub use oplog::RecordingOpLogWriter;
pub use replicator::{FakeReplicator, FakeReplicatorConfig};
pub use router::TokioEventSink;
pub use store::InMemorySessionStore;
pub use transport::SyncRouterState;
pub use wire::{
    decode_client_message, encode_event, encode_write_result, ClientMessage, FilterClause,
    WireFrame,
};
pub use write_back::{parse_allowlist, parse_counter_columns, parse_or_set_columns, NoWriteBack};

#[cfg(feature = "pg")]
pub use replicator::{PgReplicator, PgReplicatorConfig, PgReplicatorError};

#[cfg(feature = "pg")]
pub use snapshot_source::PgSnapshotter;

#[cfg(feature = "pg")]
pub use schema_source::{PgSchemaSource, PgTableStats};

#[cfg(feature = "pg")]
pub use write_back::PgWriteBack;

#[cfg(feature = "pg")]
pub use oplog::PgOpLogReader;

#[cfg(feature = "pg")]
pub use oplog::PgOpLogWriter;

#[cfg(feature = "pg")]
pub use oplog::PgOpLogCompactor;

#[cfg(feature = "pg")]
pub use token_store::{PgTokenStore, PushToken, TokenStoreError};
