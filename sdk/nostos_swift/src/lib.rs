//! `nostos-swift` — UniFFI bridge exposing `nostos_client::SyncClient<SqliteStorage>`
//! to Swift (iOS / macOS). Mirrors `sdk/nostos_tauri/src/lib.rs` and
//! `sdk/nostos_node/src/lib.rs` — the SAME `SyncClient<SqliteStorage>` the native,
//! Tauri, Flutter, and Node SDKs drive, loaded into Swift via UniFFI's
//! proc-macro FFI, with no engine/wire changes.
//!
//! # Why this exists
//! Feasibility scaffold for the "cheap-catch-up multi-platform" thesis: prove
//! the SAME client the four sibling SDKs drive can be loaded from Swift, with
//! no engine/wire changes. Scope is "Rust compiles for the host Apple target +
//! UniFFI generates Swift + the generated Swift typechecks" — NOT a polished
//! SDK.
//!
//! # Runtime shape
//! `NostosClient` owns a `tokio::runtime::Runtime` (the same shape
//! `sdk/nostos_node`'s `NostosClient` uses) so that `connect`/`write`/`query`/
//! `checkpoint` — all of which `.await` on `SyncClient`'s async API — can be
//! surfaced to Swift as **synchronous** methods. UniFFI async (the `tokio`
//! feature + `#[uniffi::method(async_runtime = "tokio")]`) is the alternative
//! path but adds ForeignFuture plumbing friction that is not load-bearing for a
//! scaffold; `block_on` from the foreign (Swift) thread is simpler and matches
//! `nostos_node`'s "own runtime, block internally" precedent. Swift's calling
//! thread blocks briefly on each call; a future `subscribe()` run loop will be
//! spawned onto the owned runtime (mirrors `nostos_node::NostosClient::subscribe`).
//!
//! # ponytail: `unsafe` policy
//! Hand-written `unsafe` is forbidden in this crate (`#![forbid(unsafe_code)]`).
//! UniFFI's macro-generated FFI scaffolding lives in the `uniffi` dependency's
//! proc-macro output, not in this crate's hand-written source, so the forbid
//! does not interact with it — same precedent as `nostos_tauri` (tauri's
//! macro FFI) and `nostos_node` (napi-derive macro FFI). ADR-0015 addendum:
//! machine-generated FFI glue is the one workspace-wide exception.
//!
//! # ponytail: deferred surfaces (upgrade path)
//! - **`subscribe(table, where_sql)` + the run loop**: not wired. The owned
//!   `rt` is retained precisely so a future `subscribe` method can
//!   `rt.spawn(client.run_with_reconnect())` (mirroring `nostos_node`'s
//!   `subscribe`). Ceiling: no row-tick callback / live sync yet; callers are
//!   offline-only. Upgrade path: add `subscribe` + a UniFFI callback interface
//!   for row-ticks, same shape as the Flutter `rows_sink`.
//! - **iOS cross-compile**: host (`aarch64-apple-darwin`) is the proof target
//!   here. `cargo build --target aarch64-apple-ios` needs `rusqlite`'s
//!   `bundled` feature (SQLite isn't shipped in the iOS SDK the way it is on
//!   macOS) — deferred. Upgrade path: add a feature gate in nostos-client that
//!   flips `rusqlite/bundled` for the iOS target.
//! - **SPM `.binaryTarget` linking the `.xcframework`**: the staticlib is
//!   produced by `cargo build --release`; wrapping it as an `.xcframework`
//!   + `Package.swift` binary target is the next increment past the
//!   `swiftc -typecheck` gate. (Update: as of this commit, host AND
//!   `aarch64-apple-ios` cross-compile both finish with the workspace's
//!   existing rusqlite config — see VERIFICATION below. The `bundled` gate is
//!   still the right belt-and-suspenders fix for non-Apple iOS toolchains.)

#![forbid(unsafe_code)]
// UniFFI proc-macro surface: clippy pedantic noise about "missing_errors_doc"
// on the FFI methods is not load-bearing for a scaffold; keep the surface
// readable instead (mirrors nostos_tauri's allow list).
#![allow(clippy::missing_errors_doc)]
// Module-level prose bullets span multiple lines; clippy's
// `doc_lazy_continuation` lint demands per-line indent alignment that would
// make the prose unreadable for no API-doc payoff (no rustdoc is rendered
// from this scaffold's module header). Targeted allow, scoped to this crate.
#![allow(clippy::doc_lazy_continuation)]

use std::sync::Arc;
use std::time::Duration;

use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{PendingWrite, WriteOp};
use nostos_domain::Lsn;
use tokio::sync::Mutex as AsyncMutex;

// UniFFI scaffolding — emits the FFI entrypoints (`uniffi_*` symbols) that
// `uniffi-bindgen generate --library` reads to produce Swift bindings. The
// argument is the UniFFI namespace (becomes the generated `.swift` filename
// and the Swift module name).
uniffi::setup_scaffolding!("nostos_swift");

/// Session-level reconnect backstop — mirrors `sdk/nostos_node`'s
/// `IDLE_RECONNECT_BACKSTOP` and the Flutter glue's constant of the same name.
/// Long relative to per-batch flush bounds: this is a rare defense-in-depth
/// reconnect, not a per-write latency mechanism.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// UniFFI-visible error type. UniFFI 0.28 refuses to bindgen `Result<_, String>`
/// ("unknown throw type: Some(String)"); every FFI method therefore returns
/// `Result<_, NostosError>`, with the message preserved verbatim from the
/// underlying `StorageError` / `ClientError` / `serde_json::Error`. The single
/// `Message` variant keeps the Swift side a simple `throw NostosError.Message`
/// — matching `nostos_node`'s single-reason `napi::Error::from_reason` shape.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NostosError {
    #[error("{message}")]
    Message {
        message: String,
    },
}

impl NostosError {
    /// Wrap any error Display-able into the single-variant `NostosError`.
    /// Used as the `.map_err(NostosError::wrap)` shorthand throughout.
    fn wrap<E: std::fmt::Display>(e: E) -> Self {
        NostosError::Message {
            message: e.to_string(),
        }
    }
}

/// A live Nostos client handle for Swift. Owns the tokio runtime the
/// `SyncClient`'s async API runs on, plus at most one active session (v1: one
/// table per client, matching `nostos-client`'s Phase-0 predicate floor and the
/// sibling SDKs).
///
/// Construct via `NostosClient(url, token: nil, dbPath:)` then call `connect()`
/// (opens the local SQLite store + builds the `SyncClient` — no network) and
/// drive `write` / `query` / `checkpoint`. All four are synchronous from
/// Swift's view — see the module `ponytail:` for why we chose sync-over-block
/// over UniFFI async.
#[derive(uniffi::Object)]
pub struct NostosClient {
    rt: tokio::runtime::Runtime,
    url: String,
    token: Option<String>,
    db_path: String,
    session: AsyncMutex<Option<Session>>,
}

/// The active session. Dropping this — including via a second `connect()`
/// replacing it — releases the `Arc<SyncClient<SqliteStorage>>`. (No
/// `run_task` to abort today — `subscribe()` is the ponytail.)
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
}

#[uniffi::export]
impl NostosClient {
    /// Construct a handle. Does no network I/O and does not open the store yet
    /// — `connect()` does. `db_path` is the SQLite file path (rusqlite accepts
    /// `":memory:"` for an ephemeral store, useful for tests).
    ///
    /// # Errors
    /// `String` if the owned tokio runtime fails to initialize (resource
    /// exhaustion).
    #[uniffi::constructor]
    pub fn new(
        url: String,
        token: Option<String>,
        db_path: String,
    ) -> Result<Arc<Self>, NostosError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(NostosError::wrap)?;
        Ok(Arc::new(Self {
            rt,
            url,
            token,
            db_path,
            session: AsyncMutex::new(None),
        }))
    }

    /// Open the local SQLite store at `db_path` and build a `SyncClient`
    /// against `url`. No network I/O — the subscribe/run loop is a separate
    /// (not-yet-wired) method. Idempotent: a second call while a session is
    /// live is a no-op. The default table is `tasks` (matches `nostos_node`
    /// and `nostos_tauri`).
    ///
    /// # Errors
    /// `NostosError` if the SQLite store can't be opened/migrated.
    pub fn connect(&self) -> Result<(), NostosError> {
        self.rt.block_on(async {
            let mut guard = self.session.lock().await;
            if guard.is_some() {
                return Ok(());
            }
            let storage = SqliteStorage::open(&self.db_path).map_err(NostosError::wrap)?;
            let config = SyncClientConfig {
                table: "tasks".to_owned(),
                token: self.token.clone(),
                idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
                ..SyncClientConfig::default()
            };
            let client = Arc::new(SyncClient::new(self.url.clone(), storage, config));
            *guard = Some(Session {
                client,
                table: "tasks".to_owned(),
            });
            Ok(())
        })
    }

    /// Enqueue a durable write against the active session's table. Resolves
    /// once the write is captured in the local outbox (NOT once the server
    /// acks it — ADR-0013 outbox contract). `op` is `"upsert"` / `"delete"` /
    /// `"patch"` (column-level UPDATE — `payload_json` carries only the
    /// changed columns). `table` MUST match the active session's table.
    ///
    /// # Errors
    /// `NostosError` if no session is active, the table mismatches, the op
    /// string is unknown, or the durable enqueue itself failed (disk full /
    /// busy).
    pub fn write(
        &self,
        table: String,
        op: String,
        pk: String,
        payload_json: Option<String>,
    ) -> Result<u64, NostosError> {
        self.rt.block_on(async {
            let write_op = match op.as_str() {
                "upsert" => WriteOp::Upsert,
                "delete" => WriteOp::Delete,
                "patch" => WriteOp::Patch,
                other => {
                    return Err(NostosError::Message {
                        message: format!(
                            "unknown write op {other:?}: expected \"upsert\", \"delete\", or \"patch\""
                        ),
                    })
                }
            };
            let client = {
                let guard = self.session.lock().await;
                let session = guard
                    .as_ref()
                    .ok_or_else(|| NostosError::Message {
                        message: "write() called before connect()".to_string(),
                    })?;
                if session.table != table {
                    return Err(NostosError::Message {
                        message: format!(
                            "write() table {table:?} does not match active session table {:?} — v1 supports one table per NostosClient",
                            session.table
                        ),
                    });
                }
                Arc::clone(&session.client)
            };
            let seq = client
                .write(PendingWrite {
                    table,
                    op: write_op,
                    pk,
                    payload_json,
                })
                .await
                .map_err(|e: ClientError| NostosError::wrap(e))?;
            Ok(seq)
        })
    }

    /// Run an arbitrary `SELECT` against the on-device SQLite store and return
    /// a JSON-array-of-objects STRING (one object per row, keyed by column
    /// name) — the same shape `nostos_node`'s and `nostos_tauri`'s `query()`
    /// emit. Requires `connect()` to have run.
    ///
    /// # Errors
    /// `NostosError` if no session is active or the SQL fails to prepare.
    pub fn query(&self, sql: String) -> Result<String, NostosError> {
        self.rt.block_on(async {
            let client = {
                let guard = self.session.lock().await;
                let session = guard
                    .as_ref()
                    .ok_or_else(|| NostosError::Message {
                        message: "query() called before connect()".to_string(),
                    })?;
                Arc::clone(&session.client)
            };
            // `with_storage` runs the closure on the client's storage task;
            // `query` is the read-side accessor on the same Mutex<Connection>
            // as the write path (see crates/nostos-client/src/sqlite.rs).
            let rows = client
                .with_storage(move |s| s.query(&sql))
                .await
                .map_err(|e: ClientError| NostosError::wrap(e))? // outer: ClientError
                .map_err(NostosError::wrap)?; // inner: StorageError (nested Result)
            serde_json::to_string(&rows).map_err(NostosError::wrap)
        })
    }

    /// Read the current durable LSN checkpoint (u64). Requires `connect()` to
    /// have run. A fresh store reports `0`.
    ///
    /// # Errors
    /// `NostosError` if no session is active or the checkpoint read fails.
    pub fn checkpoint(&self) -> Result<u64, NostosError> {
        self.rt.block_on(async {
            let client = {
                let guard = self.session.lock().await;
                let session = guard
                    .as_ref()
                    .ok_or_else(|| NostosError::Message {
                        message: "checkpoint() called before connect()".to_string(),
                    })?;
                Arc::clone(&session.client)
            };
            let lsn: Lsn = client.checkpoint().await.map_err(NostosError::wrap)?;
            Ok(lsn.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proof-of-integration: the SAME `SyncClient<SqliteStorage>` the sibling
    /// SDKs drive constructs + serves an offline query through the UniFFI
    /// `NostosClient` shape, with no live Swift runtime required. Mirrors
    /// `nostos_tauri`'s offline smoke path (construct + query round-trip).
    #[test]
    fn nostos_client_offline_connect_query_round_trip() {
        let client = NostosClient::new(
            "ws://localhost:0".into(),
            None,
            ":memory:".into(),
        )
        .expect("construct");

        client.connect().expect("connect");

        let rows_json = client.query("SELECT 1 AS one".into()).expect("query");
        assert!(
            rows_json.contains("\"one\":1") || rows_json.contains("\"one\": 1"),
            "expected an one=1 row in the JSON, got: {rows_json}"
        );

        let lsn = client.checkpoint().expect("checkpoint");
        assert_eq!(lsn, 0, "fresh store should report Lsn(0)");
    }

    /// `write()` before `connect()` surfaces a clear error rather than
    /// panicking — the same contract `nostos_tauri` and `nostos_node` enforce.
    #[test]
    fn write_before_connect_is_an_error() {
        let client = NostosClient::new(
            "ws://localhost:0".into(),
            None,
            ":memory:".into(),
        )
        .expect("construct");

        let err = client
            .write("tasks".into(), "upsert".into(), "pk1".into(), None)
            .expect_err("write before connect should error");
        let msg = err.to_string();
        assert!(
            msg.contains("before connect"),
            "expected a before-connect error, got: {msg}"
        );
    }
}
