//! `nostos-tauri` — Tauri 2 plugin exposing `nostos_client::SyncClient<SqliteStorage>`
//! to desktop web apps. Mirrors `sdk/nostos_node/src/lib.rs` and
//! `sdk/nostos_flutter/rust/src/api/nostos.rs`.
//!
//! # Why this exists
//! Feasibility scaffold for the "cheap-catch-up multi-platform" thesis: prove
//! the SAME `SyncClient<SqliteStorage>` the native + Flutter + Node SDKs drive
//! can be loaded from a Tauri (desktop web) app, with no engine/wire changes.
//! Scope is "compiles + a rust unit test of the integration logic passes" —
//! NOT a polished SDK.
//!
//! # Runtime shape
//! The four `#[tauri::command]` handlers (`connect` / `write` / `query` /
//! `checkpoint`) are thin wrappers over `impl NostosState` async methods, which
//! `.await` on the host runtime Tauri runs the command on (or, in tests, the
//! `#[tokio::test]` runtime) — so `NostosState` does **not** own a
//! `tokio::runtime::Runtime` today. An owned runtime returns when `subscribe()`
//! lands (see the struct `ponytail:`), to spawn the long-lived run loop; owning
//! one now would be dead weight AND would panic on drop inside an async
//! context.
//!
//! # ponytail: `unsafe` policy
//! Hand-written `unsafe` is forbidden in this crate (`#![forbid(unsafe_code)]`).
//! `tauri`'s macro-generated FFI lives in the `tauri` dependency, not in this
//! crate's source, so the forbid does not interact with it.
//!
//! # ponytail: deferred surfaces (upgrade path)
//! - **`subscribe(table, where_sql)` + the run loop**: not wired. The owned
//!   `rt` is retained precisely so a future `subscribe` command can
//!   `rt.spawn(client.run_with_reconnect())` (mirroring `nostos_node`'s
//!   `subscribe`). Ceiling: no row-tick callback / live sync yet — callers are
//!   offline-only. Upgrade path: add `subscribe` + a `tauri::ipc::Channel` for
//!   row-ticks, same shape as the Flutter `rows_sink`.
//! - **Permissions**: only the four default command permissions are listed in
//!   `permissions/default.toml`. A shipped plugin would also publish scoped
//!   permission sets per table.
//! - **JS bindings / `.d.ts`**: a shipped plugin runs `tauri-plugin`'s JS
//!   scaffolder to emit a `guest-js/` package; not in scope here.

#![forbid(unsafe_code)]
// The Tauri plugin shape uses a generic `R: Runtime` init + `State<'_, T>`
// commands. Clippy pedantic noise about "missing errors_doc" / "missing_panics_doc"
// on the `#[tauri::command]` async fns and the generic init is not load-bearing
// for a scaffold; keep the surface readable instead.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;
use std::time::Duration;

use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::{PendingWrite, WriteOp};
use nostos_domain::Lsn;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, State};
use tokio::sync::Mutex as AsyncMutex;

/// Session-level reconnect backstop — mirrors `sdk/nostos_node`'s
/// `IDLE_RECONNECT_BACKSTOP` and the Flutter glue's constant of the same name.
/// Long relative to per-batch flush bounds: this is a rare defense-in-depth
/// reconnect, not a per-write latency mechanism.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// Plugin state, managed by Tauri. Owns the tokio runtime (for a future
/// `subscribe` run loop — see the module ponytail) plus at most one active
/// session (v1: one table per client, matching `nostos-client`'s Phase-0
/// predicate floor and the sibling SDKs).
///
/// Construct via `NostosState::new()` (the plugin's `setup` hook does this and
/// registers it with `app.manage(...)`); drive with `connect` / `write` /
/// `query` / `checkpoint`.
pub struct NostosState {
    // ponytail: no owned `tokio::runtime::Runtime` today. connect/write/query/
    // checkpoint `.await` on the host runtime (Tauri's, or #[tokio::test]'s);
    // an owned runtime would be dead weight AND panics on drop inside an async
    // context. It returns when `subscribe()` lands to spawn the long-lived
    // `run_with_reconnect` loop (mirrors sdk/nostos_node's `rt`).
    session: AsyncMutex<Option<Session>>,
}

struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
}

impl NostosState {
    /// Construct the state — starts with no active session; `connect()` opens
    /// one. Infallible: nothing is allocated up front (no owned runtime yet —
    /// see the struct `ponytail:` for when one returns).
    pub fn new() -> Self {
        Self {
            session: AsyncMutex::new(None),
        }
    }

    /// Open the local SQLite store at `db_path` and build a `SyncClient`
    /// against `url`. No network I/O — the subscribe/run loop is a separate
    /// (not-yet-wired) command. Idempotent: a second call while a session is
    /// live is a no-op. The default table is `tasks` (matches `nostos_node`).
    ///
    /// # Errors
    /// `String` if the SQLite store can't be opened/migrated.
    pub async fn connect(
        &self,
        url: String,
        token: Option<String>,
        db_path: String,
    ) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let storage =
            SqliteStorage::open(&db_path).map_err(|e| e.to_string())?;
        let config = SyncClientConfig {
            table: "tasks".to_owned(),
            token,
            idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
            ..SyncClientConfig::default()
        };
        let client = Arc::new(SyncClient::new(url, storage, config));
        *guard = Some(Session {
            client,
            table: "tasks".to_owned(),
        });
        Ok(())
    }

    /// Enqueue a durable write against the active session's table. Resolves
    /// once the write is captured in the local outbox (NOT once the server
    /// acks it — ADR-0013 outbox contract). `op` is `"upsert"` / `"delete"` /
    /// `"patch"` (column-level UPDATE — `payload_json` carries only the
    /// changed columns). `table` MUST match the active session's table.
    ///
    /// # Errors
    /// `String` if no session is active, the table mismatches, the op string
    /// is unknown, or the durable enqueue itself failed (disk full / busy).
    pub async fn write(
        &self,
        table: String,
        op: String,
        pk: String,
        payload_json: Option<String>,
    ) -> Result<u64, String> {
        let write_op = match op.as_str() {
            "upsert" => WriteOp::Upsert,
            "delete" => WriteOp::Delete,
            "patch" => WriteOp::Patch,
            other => {
                return Err(format!(
                    "unknown write op {other:?}: expected \"upsert\", \"delete\", or \"patch\""
                ))
            }
        };
        let client = {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| "write() called before connect()".to_string())?;
            if session.table != table {
                return Err(format!(
                    "write() table {table:?} does not match active session table {:?} — v1 supports one table per NostosState",
                    session.table
                ));
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
            .map_err(|e: ClientError| e.to_string())?;
        Ok(seq)
    }

    /// Run an arbitrary `SELECT` against the on-device SQLite store and return
    /// a JSON-array-of-objects STRING (one object per row, keyed by column
    /// name) — the same shape `nostos_node`'s `query()` emits, so a JS caller
    /// `JSON.parse`s it directly. Requires `connect()` to have run.
    ///
    /// # Errors
    /// `String` if no session is active or the SQL fails to prepare.
    pub async fn query(&self, sql: String) -> Result<String, String> {
        let client = {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| "query() called before connect()".to_string())?;
            Arc::clone(&session.client)
        };
        // `with_storage` runs the closure on the client's storage task; `query`
        // is the read-side accessor on the same Mutex<Connection> as the write
        // path (see crates/nostos-client/src/sqlite.rs).
        let rows = client
            .with_storage(move |s| s.query(&sql))
            .await
            .map_err(|e: ClientError| e.to_string())? // outer: ClientError
            .map_err(|e| e.to_string())?; // inner: StorageError (nested Result)
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    /// Read the current durable LSN checkpoint (u64). Requires `connect()` to
    /// have run. A fresh store reports `0`.
    ///
    /// # Errors
    /// `String` if no session is active or the checkpoint read fails.
    pub async fn checkpoint(&self) -> Result<u64, String> {
        let client = {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| "checkpoint() called before connect()".to_string())?;
            Arc::clone(&session.client)
        };
        let lsn: Lsn = client.checkpoint().await.map_err(|e| e.to_string())?;
        Ok(lsn.0)
    }
}

impl Default for NostosState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tauri command handlers — thin wrappers over `impl NostosState` so the logic
// is unit-testable without a live `tauri::AppHandle`.
// ---------------------------------------------------------------------------

/// Open the local SQLite store + build the `SyncClient`. No network I/O.
#[tauri::command]
async fn connect(
    state: State<'_, NostosState>,
    url: String,
    token: Option<String>,
    db_path: String,
) -> Result<(), String> {
    state.connect(url, token, db_path).await
}

/// Enqueue a durable write against the active session's table.
#[tauri::command]
async fn write(
    state: State<'_, NostosState>,
    table: String,
    op: String,
    pk: String,
    payload_json: Option<String>,
) -> Result<u64, String> {
    state.write(table, op, pk, payload_json).await
}

/// Run a `SELECT` against the on-device SQLite store; returns a JSON string.
#[tauri::command]
async fn query(state: State<'_, NostosState>, sql: String) -> Result<String, String> {
    state.query(sql).await
}

/// Read the durable LSN checkpoint as a u64.
#[tauri::command]
async fn checkpoint(state: State<'_, NostosState>) -> Result<u64, String> {
    state.checkpoint().await
}

/// Build the `nostos` Tauri plugin. Generic over `R: Runtime` so a Tauri app
/// using any runtime (the default `Wry`, or a custom one) can register it via
/// `tauri::Builder::default().plugin(nostos_tauri::init())`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::<R, ()>::new("cairn")
        .setup(|app, _api| {
            app.manage(NostosState::new());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            write,
            query,
            checkpoint,
        ])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proof-of-integration: the SAME `SyncClient<SqliteStorage>` the native +
    /// Flutter + Node SDKs drive constructs + serves an offline query through
    /// the Tauri state shape, with no live `tauri::AppHandle` required. This
    /// mirrors `nostos_node`'s offline smoke path (construct + query round-trip)
    /// — the only difference is the state shape (`NostosState` vs the `#[napi]`
    /// `NostosClient`).
    #[tokio::test]
    async fn nostos_state_offline_connect_query_round_trip() {
        let state = NostosState::new();

        // `connect()` opens SqliteStorage at `:memory:` and builds the
        // SyncClient. No network I/O — the url is unused without a run loop.
        state
            .connect(
                "ws://localhost:0".into(),
                None,
                ":memory:".into(),
            )
            .await
            .expect("connect");

        // Round-trip an aliased `SELECT 1 AS one` through
        // `SyncClient::with_storage` → `SqliteStorage::query` — the same path
        // `nostos_node`'s `query()` takes. (Aliased so the column key is stable;
        // `nostos_node`'s smoke uses the same `AS one` shape.)
        let rows_json = state
            .query("SELECT 1 AS one".into())
            .await
            .expect("query");
        assert!(
            rows_json.contains("\"one\":1") || rows_json.contains("\"one\": 1"),
            "expected an one=1 row in the JSON, got: {rows_json}"
        );

        // `checkpoint()` reads the durable LSN from `cairn_meta` — proves the
        // schema initialized + the engine storage accessor is wired. A fresh
        // store reports 0.
        let lsn = state.checkpoint().await.expect("checkpoint");
        assert_eq!(lsn, 0, "fresh store should report Lsn(0)");
    }

    /// `write()` before `connect()` surfaces a clear error rather than
    /// panicking — the same contract `nostos_node` enforces.
    #[tokio::test]
    async fn write_before_connect_is_an_error() {
        let state = NostosState::new();
        let err = state
            .write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                None,
            )
            .await
            .expect_err("write before connect should error");
        assert!(
            err.contains("before connect"),
            "expected a before-connect error, got: {err}"
        );
    }
}
