//! `nostos_node` — napi-rs glue exposing `nostos_client::SyncClient<SqliteStorage>`
//! to Node.js. Mirrors `sdk/nostos_flutter/rust/src/api/nostos.rs`.
//!
//! # Why this exists
//! Feasibility scaffold for the "cheap-catch-up multi-platform" thesis: prove
//! the SAME `SyncClient<SqliteStorage>` the native + Flutter SDKs drive can be
//! loaded from Node.js over napi-rs, with no engine/wire changes. Scope is
//! "compiles + loads + one method runs without a native crash" — NOT a polished
//! SDK.
//!
//! # Runtime shape
//! This crate owns a `tokio::runtime::Runtime` (just like the Flutter glue's
//! `NostosHandle`), on which the background connect/apply/reconnect loop is
//! spawned. `#[napi] async fn` methods are polled on napi's `tokio_rt` worker
//! and return JS Promises; the long-lived session loop is spawned onto OUR
//! owned runtime via `self.rt.spawn(...)`, so it outlives any single Promise.
//!
//! # ponytail: `unsafe` policy
//! Hand-written `unsafe` is forbidden workspace-wide. This crate is a
//! non-member SDK (own `[workspace]`, like `sdk/nostos_flutter/rust`) and the
//! `napi-derive` `#[napi]` macro IS the machine-generated FFI glue exception
//! (ADR-0015 addendum — same standing as `flutter_rust_bridge` generated code).
//! Accordingly this crate does NOT set `#![forbid(unsafe_code)]`; doing so would
//! reject the macro-expanded FFI. Do NOT add hand-written `unsafe` here.
//!
//! # ponytail: deferred surfaces (upgrade path)
//! - **Row-tick callback**: the Flutter glue hands Dart a `StreamSink<String>`
//!   per applied batch. The napi equivalent is a `ThreadsafeFunction`. Not
//!   wired here — callers poll `query()` for now. Upgrade: add a
//!   `subscribe(table, whereSql, onRows)` overload taking a
//!   `ThreadsafeFunction`.
//! - **Connection-state stream**: Flutter emits `NostosConnectionState`
//!   transitions. Deferred (same ThreadsafeFunction reason).
//! - **`.d.ts` generation**: plain `cargo build` does not emit TS types; use
//!   `npm run build:napi` (`@napi-rs/cli build`) for `.d.ts` + cross-triple
//!   packaging when shipping.

use std::sync::Arc;
use std::time::Duration;

use napi_derive::napi;
use tokio::sync::Mutex as AsyncMutex;

use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig};

/// Session-level reconnect backstop — mirrors the Flutter glue's
/// `IDLE_RECONNECT_BACKSTOP`. Long relative to `flush_quiesce` (per-batch flush
/// bound): this is a rare defense-in-depth reconnect, not a per-write latency
/// mechanism.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// A live Nostos client handle. Owns the tokio runtime the background sync loop
/// runs on, plus at most one active session (v1: one table per client, matching
/// `nostos-client`'s Phase-0 predicate floor and the Flutter glue).
///
/// Construct with `new NostosClient(url, token?, dbPath)`, then `await connect()`
/// (opens the local SQLite store + builds the `SyncClient` — no network) and
/// optionally `await subscribe(table, whereSql?)` (starts the network loop on
/// the owned runtime).
#[napi]
pub struct NostosClient {
    rt: tokio::runtime::Runtime,
    url: String,
    token: Option<String>,
    db_path: String,
    session: AsyncMutex<Option<Session>>,
}

/// The active session. Dropping this — including via `subscribe()`/`connect()`
/// replacing it — aborts the background run loop so a superseded session's
/// WebSocket + reconnect loop actually stops instead of leaking.
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
    run_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = self.run_task.take() {
            task.abort();
        }
    }
}

#[napi]
impl NostosClient {
    /// Construct a handle. Does no network I/O and does not open the store yet
    /// — `connect()` does. `db_path` is the SQLite file path (rusqlite accepts
    /// `":memory:"` for an ephemeral store, useful for tests).
    #[napi(constructor)]
    pub fn new(url: String, token: Option<String>, db_path: String) -> napi::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| napi::Error::from_reason(format!("nostos_node: tokio init failed: {e}")))?;
        Ok(Self {
            rt,
            url,
            token,
            db_path,
            session: AsyncMutex::new(None),
        })
    }

    /// Sync getter — proof-of-load accessor returning the WS URL. Synchronous on
    /// purpose: a non-Promise call that exercises the FFI seam without the
    /// async runtime, so a smoke test can distinguish "addon loads" from
    /// "Promise path works" if the latter ever regresses.
    #[napi(getter)]
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Open the local SQLite store and construct the `SyncClient`. No network
    /// I/O. After this resolves, `query()` works offline against the durable
    /// store (immediate snapshot — same offline-first property as the Flutter
    /// glue). The default table is `tasks` (overridden by `subscribe()`).
    #[napi]
    pub async fn connect(&self) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let storage =
            SqliteStorage::open(&self.db_path).map_err(|e| napi::Error::from_reason(e.to_string()))?;
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
            run_task: None,
        });
        Ok(())
    }

    /// Start the background connect/apply/reconnect loop against `table`
    /// (optionally filtered by `where_sql`, the safe-SQL subset of ADR-0012).
    /// Replaces any prior session on this handle. Resolves once the loop is
    /// spawned — network/session errors surface only internally (silent
    /// auto-reconnect, matching `SyncClient::run_with_reconnect`'s contract);
    /// `write()` is what surfaces a durable-outbox failure to the caller.
    ///
    /// ponytail: no row-tick callback is delivered yet (ThreadsafeFunction
    /// deferred — see module docs). Poll `query()` to observe applied rows.
    #[napi]
    pub async fn subscribe(
        &self,
        table: String,
        where_sql: Option<String>,
    ) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        // Drop any prior session first — its Drop aborts the prior run_task.
        *guard = None;

        let storage =
            SqliteStorage::open(&self.db_path).map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let config = SyncClientConfig {
            table: table.clone(),
            token: self.token.clone(),
            where_sql,
            idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
            ..SyncClientConfig::default()
        };
        let client = Arc::new(SyncClient::new(self.url.clone(), storage, config));

        // Spawn the long-lived reconnect loop on OUR runtime (not napi's), so it
        // outlives this Promise. `max_retries: None` (default) -> retries
        // forever; the task only completes on a terminal error, which we swallow
        // (auto-reconnect is the contract — a real write surfaces its own error).
        let run_client = Arc::clone(&client);
        let run_task = self.rt.spawn(async move {
            let _ = run_client.run_with_reconnect().await;
        });

        *guard = Some(Session {
            client,
            table,
            run_task: Some(run_task),
        });
        Ok(())
    }

    /// Enqueue a durable write against the active session's table. Resolves
    /// once the write is captured in the local outbox (NOT once the server
    /// acks it — that happens asynchronously; ADR-0013 outbox contract).
    ///
    /// `op` is `"upsert"` (insert-or-update), `"delete"`, or `"patch"`
    /// (column-level UPDATE — `payload_json` carries only the changed columns).
    ///
    /// `table` MUST match the active session's table (v1: one table per client).
    ///
    /// Returns the durable write's sequence id as a JS Number (f64).
    /// ponytail: precision is lost for ids >= 2^53 (napi-rs does not auto-convert
    /// `u64`; JS Numbers are f64). Fine for a feasibility scaffold — a shipped
    /// SDK should return `napi::bindgen_prelude::BigInt` to preserve full u64
    /// range, the same way power-sync-style clients surface large cursor ids.
    #[napi]
    pub async fn write(
        &self,
        table: String,
        op: String,
        pk: String,
        payload_json: Option<String>,
    ) -> napi::Result<f64> {
        use nostos_core::{PendingWrite, WriteOp};

        let write_op = match op.as_str() {
            "upsert" => WriteOp::Upsert,
            "delete" => WriteOp::Delete,
            "patch" => WriteOp::Patch,
            other => {
                return Err(napi::Error::from_reason(format!(
                    "unknown write op {other:?}: expected \"upsert\", \"delete\", or \"patch\""
                )))
            }
        };
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("write() called before connect()/subscribe()"))?;
        if session.table != table {
            return Err(napi::Error::from_reason(format!(
                "write() table {table:?} does not match active session table {:?} — v1 supports one table per NostosClient",
                session.table
            )));
        }
        let seq = session
            .client
            .write(PendingWrite {
                table,
                op: write_op,
                pk,
                payload_json,
            })
            .await
            .map_err(|e: ClientError| napi::Error::from_reason(e.to_string()))?;
        Ok(seq as f64)
    }

    /// Run an arbitrary `SELECT` against the on-device SQLite store and return
    /// a JSON-array-of-objects STRING (one object per row, keyed by column
    /// name) — the same shape the Flutter glue's `rows_sink` emits, so a JS
    /// caller `JSON.parse`s it directly. Requires `connect()` (or
    /// `subscribe()`) to have run.
    #[napi]
    pub async fn query(&self, sql: String) -> napi::Result<String> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("query() called before connect()/subscribe()"))?;
        // `with_storage` runs the closure on the client's storage task; `query`
        // is a read on the same `Mutex<Connection>` as the write path (no
        // shared mutation surface — see nostos-core's `Storage::query` doc).
        let rows = session
            .client
            .with_storage(move |s| s.query(&sql))
            .await
            .map_err(|e: ClientError| napi::Error::from_reason(e.to_string()))?
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        serde_json::to_string(&rows).map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    /// Tear down the active session's background work (the connect/apply loop).
    /// Safe to call with no active session (no-op) and idempotent. Does NOT
    /// shut down this handle's tokio runtime — a subsequent `connect()` or
    /// `subscribe()` on the SAME `NostosClient` reopens a fresh session against
    /// the same durable store. The runtime is torn down when the handle is
    /// GC'd by JS.
    #[napi]
    pub async fn close(&self) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        *guard = None; // Drop aborts run_task.
        Ok(())
    }
}
