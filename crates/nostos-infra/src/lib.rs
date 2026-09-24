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
//! | [`push`] | — (inherent; ADR-0037) | FCM HTTP v1 / APNs / Web Push rails + `PushRouter` coalescer (the `PushNotifier` port impl, plan 2.4) |
//!
//! The benchmark drives a `FakeReplicator` through the *real* `FanOutService`
//! and `TokioEventSink`, so what we measure is the production pipeline.

#![forbid(unsafe_code)]

pub mod auth;
/// Config file/dir names with the pre-rename fallback (ADR-0046).
pub mod config_path;
/// `NOSTOS_*` env reads with the pre-rename fallback (ADR-0046) — the only
/// sanctioned `std::env` reader (`clippy.toml`).
pub mod env;
/// Shared strict identifier validation (ADR-0013 discipline) — lifted here
/// when the mirror ingest adapter became the third caller; `pub` because the
/// server's `/ingest` route validates the same client-controlled table names.
pub mod ident;
/// `#[cfg(feature = "iroh")]` — the ADR-0041 spike transport: iroh accept
/// loop bridging each bidirectional stream to the loopback HTTP listener,
/// plus the QR-native iroh:// dial URL.
#[cfg(feature = "iroh")]
pub mod iroh_sync;
mod jwks;
/// Persisted operation-log writers (ADR-0025 slice 2). `RecordingOpLogWriter`
/// (always available, in-memory — bench/test) + `PgOpLogWriter` (feature "pg").
pub mod oplog;
/// Bounded lazy PG connect shared by the pool-of-one adapters (audit
/// 2026-08-17 M8). pg-only: every caller is a `pg`-gated adapter, and the
/// helper's signature names `tokio_postgres::Client` (optional dep) — an
/// ungated `mod` here broke the standalone no-feature `cargo check -p
/// nostos-infra` (workspace CI stayed green only via feature unification).
#[cfg(feature = "pg")]
mod pg_connect;
/// The push provider rails (ADR-0037 §1, plan tasks 2.1–2.4): FCM HTTP v1 /
/// APNs / Web Push senders with one shared `RailOutcome`, plus `router`'s
/// `PushRouter` — the coalescer that implements the application
/// `PushNotifier` port over them.
pub mod push;
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
/// Absent without the `pg` feature. Implements the `PushTokenRegistry` seam
/// (push/router) — the second implementation next to `InMemoryTokenRegistry`.
#[cfg(feature = "pg")]
pub mod token_store;
pub mod transport;
pub mod wire;
pub mod write_back;

pub use auth::{AllowAnonymous, StaticBearerAuth, SupabaseJwtAuth};
pub use oplog::RecordingOpLogWriter;
pub use push::router::{
    InMemoryTokenRegistry, PushRouter, PushSink, PushTokenRegistry, RailSet, RegisteredToken,
};
pub use push::{
    apns::ApnsRail,
    fcm::{FcmMessage, FcmRail, FcmTarget},
    PushPayload, PushRailError, RailOutcome,
};
// OpenSSL-backed rail — feature-gated (see push/mod.rs); default-on for servers.
#[cfg(feature = "webpush")]
pub use push::webpush::WebPushRail;
pub use replicator::{FakeReplicator, FakeReplicatorConfig, MirrorHandle, MirrorReplicator};
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
