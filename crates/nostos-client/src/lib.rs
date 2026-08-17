//! # nostos-client — the native Nostos sync client.
//!
//! The receive half of the loop: connect to `/sync`, apply frames to a durable
//! SQLite store, checkpoint the LSN, and reconnect with `resume_lsn` on drop.
//!
//! This crate holds everything that is NOT WASM-portable: the tokio transport
//! and the native `rusqlite` backend. The platform-agnostic apply engine
//! ([`nostos_core::ApplyEngine`]) and the [`nostos_core::Storage`] seam live in
//! `nostos-core`; this crate supplies two real implementations of them.
//!
//! ## What's here
//!
//! - [`sqlite::SqliteStorage`] — real `rusqlite` persistence: opaque row bytes
//!   per `(table, pk)` + a `cairn_meta` checkpoint, applied atomically.
//! - [`client::SyncClient`] — the tokio orchestrator: subscribe with the durable
//!   `resume_lsn`, drive the apply engine, `Ack` each commit, reconnect with
//!   backoff.
//!
//! ## What's NOT here (ponytail — deferred)
//!
//! - FFI bridges (ADR-0015 — they bind `nostos-core`, not this).
//! - Column-level decoding (opaque bytes until ADR-0012).
//! - Direct write-back (ADR-0013, Phase 4).

#![forbid(unsafe_code)]

pub mod client;
pub mod sqlite;

pub use client::{
    ClientError, SessionOutcome, StreamDecl, StreamHandle, StreamSubscription, SyncClient,
    SyncClientConfig, TableSub, WriteQueueStatus,
};
pub use sqlite::SqliteStorage;
