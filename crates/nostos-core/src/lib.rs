//! # nostos-core — the platform-agnostic client sync engine.
//!
//! The apply half of Nostos: consume replication frames from `/sync, apply them
//! to a [`Storage`], and advance the durable LSN checkpoint so a reconnect
//! resumes exactly where the client left off.
//!
//! ## Why a separate crate
//!
//! This crate is **pure Rust: no tokio, no SQLite, no I/O.** It compiles to WASM
//! unchanged, which is what the FFI bridges (ADR-0015) bind — `flutter_rust_bridge`
//! (Flutter), UniFFI (iOS/Android/RN), and `wasm-bindgen` (Web) all want the same
//! apply state machine with no runtime baggage. The async transport + the native
//! SQLite backend live in `nostos-client`; here we define only the engine and the
//! [`Storage`] seam they share.
//!
//! ## The two-method contract
//!
//! [`Storage`] is deliberately two methods. The correctness property — *the row
//! writes and the LSN checkpoint land in one atomic transaction* — collapses
//! into [`Storage::apply_batch`], so it's structural rather than conventional.
//! The async [`ApplyEngine`] drives that synchronous method on `spawn_blocking`
//! (or inline on WASM), batches frames to transaction boundaries, and yields the
//! LSN the caller should `Ack`.
//!
//! ## What's NOT here (ponytail — deferred, documented in ADRs)
//!
//! - The network/transport (`SyncClient` lives in `nostos-client`).
//! - The native SQLite backend (`SqliteStorage` lives in `nostos-client`).
//! - Column-level decoding (opaque payload bytes only until ADR-0012).
//! - CRDT / custom merge (ADR-0014, Phase 4).
//!
//! The client outbox *queue contract* IS here ([`Outbox`], [`PendingWrite`]) —
//! the durable surface for offline writes (ADR-0013). The native `rusqlite` impl
//! of it lives in `nostos-client`, same as `Storage`.

#![forbid(unsafe_code)]

pub mod apply;
pub mod attachments;
pub mod in_memory;
pub mod outbox;
pub mod storage;

pub use apply::{ApplyEngine, ApplyOutcome, Frame};
pub use attachments::{retry_after_ms, AttachmentOp, AttachmentState, DEFAULT_MAX_ATTEMPTS};
pub use in_memory::InMemoryStorage;
pub use outbox::{Outbox, PendingWrite, WriteOp};
pub use storage::{Result, Storage, StorageError};

// Re-export the domain types the client surface needs so downstream (nostos-client,
// the FFI shims) can depend on `nostos-core` alone.
pub use nostos_domain::{Lsn, Operation, RowOp};
