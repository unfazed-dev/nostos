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
//!   per `(table, pk)` + a `nostos_meta` checkpoint, applied atomically.
//! - [`client::SyncClient`] — the tokio orchestrator: subscribe with the durable
//!   `resume_lsn`, drive the apply engine, `Ack` each commit, reconnect with
//!   backoff.
//! - [`doorbell::listen`] — direct mode's wake-up: a Supabase Realtime private
//!   channel whose every message means "call `rpc/pull`". Contentless on
//!   purpose (ADR-0037).
//! - [`postgrest::PostgrestSource`] — direct mode's change source: `rpc/pull`
//!   against the client's own Postgres, no Nostos server in the path
//!   (`docs/plans/direct-mode-sync-protocol.md`). The decode/group/advance
//!   logic it drives is in `nostos-core` so the browser Worker reuses it.
//!
//! ## What's NOT here (ponytail — deferred)
//!
//! - FFI bridges (ADR-0015 — they bind `nostos-core`, not this).
//! - Column-level decoding (opaque bytes until ADR-0012).
//! - Direct write-back (ADR-0013, Phase 4).

#![forbid(unsafe_code)]

pub mod client;
pub mod direct;
pub mod doorbell;
/// `#[cfg(feature = "iroh")]` — ADR-0041 spike: dial-by-scheme for
/// `iroh://` sync URLs (WebSocket handshake over an iroh bidirectional
/// stream; the session loop is unchanged).
#[cfg(feature = "iroh")]
pub mod iroh_dial;
pub mod postgrest;
pub mod sqlite;

pub use client::{
    ClientError, SessionOutcome, StreamDecl, StreamHandle, StreamSubscription, SyncClient,
    SyncClientConfig, TableSub, WriteQueueStatus,
};
pub use direct::{DirectClient, SyncOutcome};
pub use doorbell::{listen, DoorbellConfig, DoorbellError, Inbound};
pub use postgrest::{DrainOutcome, PostgrestError, PostgrestSource, MAX_PAGES_PER_DRAIN};
pub use sqlite::SqliteStorage;
