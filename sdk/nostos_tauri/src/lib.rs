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
//! The five `#[tauri::command]` handlers (`connect` / `subscribe` / `write` /
//! `query` / `checkpoint`) are thin wrappers over `impl NostosState` async methods, which
//! `.await` on the host runtime Tauri runs the command on (or, in tests, the
//! `#[tokio::test]` runtime). `NostosState` ALSO owns a
//! `tokio::runtime::Runtime` — the home of the long-lived `subscribe()` run
//! loop (`client.run_once()`), so live replication continues independent of
//! command-handler scheduling. Constructed in `new()` (NOT inside an async
//! context — `Runtime::new()` would be unsound there; `new()` is called from
//! Tauri's sync `setup` hook or a test's plain `NostosState::new()` call).
//!
//! # ponytail: `unsafe` policy
//! Hand-written `unsafe` is forbidden in this crate (`#![forbid(unsafe_code)]`).
//! `tauri`'s macro-generated FFI lives in the `tauri` dependency, not in this
//! crate's source, so the forbid does not interact with it.
//!
//! # ponytail: deferred surfaces (upgrade path)
//! - **`subscribe` row-tick callback**: the run loop IS wired
//!   (`NostosState::subscribe` spawns `client.run_once()` on the owned runtime),
//!   so live replication works end-to-end — but received rows land in the
//!   on-device SQLite store only; no `tauri::ipc::Channel` fans row-tick
//!   events to the JS layer yet. Ceiling: JS callers must `query()` to observe
//!   changes. Upgrade path: add a `Channel<NostosRowEvent>` sink threaded
//!   through `subscribe`, same shape as the Flutter `rows_sink`.
//! - **`subscribe` where_sql / resume_lsn**: the `table` arg is accepted for
//!   API parity; v1 asserts it matches the session's single table (one table
//!   per `NostosState`, matching the sibling SDKs). Per-call predicate filters
//!   + `resume_lsn` arrive with the multi-table lift.
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

/// Plugin state, managed by Tauri. Owns a `tokio::runtime::Runtime` (home of
/// the `subscribe()` run loop) plus at most one active session (v1: one table
/// per client, matching `nostos-client`'s Phase-0 predicate floor and the
/// sibling SDKs).
///
/// Construct via `NostosState::new()` (the plugin's `setup` hook does this and
/// registers it with `app.manage(...)`); drive with `connect` / `subscribe` /
/// `write` / `query` / `checkpoint`.
pub struct NostosState {
    // The run loop (`client.run_once()`) lives here, NOT on the host runtime,
    // so live replication continues independent of command-handler scheduling
    // (mirrors sdk/nostos_node's owned `rt`). Constructed synchronously in
    // `new()` — `Runtime::new()` panics inside an async context, and `new()`
    // is called from Tauri's sync `setup` hook (or a test's sync call site).
    //
    // `Option` so `Drop` can move the runtime out: dropping a multi-thread
    // `Runtime` from inside an async context panics ("Cannot drop a runtime in
    // a context where blocking is not allowed") — the exact footgun `#[tokio
    // ::test]` hits. `Drop` off-loads the blocking shutdown to a std thread
    // when a runtime context is ambient; production (Tauri's sync `setup`)
    // drops outside async and pays no such cost.
    rt: Option<tokio::runtime::Runtime>,
    session: AsyncMutex<Option<Session>>,
}

struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
    // `JoinHandle` for the background `run_once()` task spawned by
    // `subscribe()`. `None` until subscribe() is called; aborted on
    // `abort_subscribe()` or session drop. Tied to `NostosState.rt`'s runtime.
    run_handle: Option<tokio::task::JoinHandle<()>>,
}

impl NostosState {
    /// Construct the state — starts the owned multi-thread tokio runtime (home
    /// of the `subscribe()` run loop) with no active session; `connect()`
    /// opens one. Synchronous — MUST NOT be called from inside an async
    /// context (tokio forbids `Runtime::new()` there); Tauri's `setup` hook
    /// and tests' `NostosState::new()` both call it sync.
    ///
    /// # Panics
    /// Panics if the tokio runtime can't be constructed (OS resource
    /// exhaustion) or if invoked from inside an async context.
    pub fn new() -> Self {
        Self {
            rt: Some(tokio::runtime::Runtime::new().expect("construct nostos-tauri runtime")),
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
            run_handle: None,
        });
        Ok(())
    }

    /// Start the live-replication run loop on the owned runtime. The session's
    /// `SyncClient::run_once()` drives the real WS subscribe → apply pipeline:
    /// server-pushed rows land in the on-device SQLite store, and the server's
    /// echo `WriteBack` re-emits this client's own `write()`s back through the
    /// same path. Received rows are observed via `query()` (a JS-layer row-tick
    /// `Channel` is the ponytail upgrade). `table` MUST match the active
    /// session's table (v1: one table per `NostosState`). Idempotent in the
    /// sense that a second call replaces + aborts a prior run loop for the same
    /// session.
    ///
    /// # Errors
    /// `String` if no session is active or the table mismatches.
    pub async fn subscribe(&self, table: String) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let client = match guard.as_ref() {
            None => {
                return Err("subscribe() called before connect()".to_string());
            }
            Some(s) if s.table != table => {
                return Err(format!(
                    "subscribe() table {table:?} does not match active session table {:?} — v1 supports one table per NostosState",
                    s.table
                ));
            }
            Some(s) => Arc::clone(&s.client),
        };
        // Spawn on the owned runtime (NOT the caller's): the run loop must keep
        // driving replication independent of the command-handler runtime.
        // `run_once` returns when idle_timeout fires; the test/caller aborts
        // the handle long before then. `run_with_reconnect` is the production
        // choice once reconnection policy is wired through config.
        let rt = self
            .rt
            .as_ref()
            .expect("runtime present until NostosState drops");
        let handle = rt.spawn(async move {
            let _ = client.run_once().await;
        });
        // Re-borrow mutably (the `&` borrow above is dead at this point) and
        // store the handle, aborting any prior handle defensively.
        if let Some(session) = guard.as_mut() {
            if let Some(prev) = session.run_handle.take() {
                prev.abort();
            }
            session.run_handle = Some(handle);
        }
        Ok(())
    }

    /// Abort the background run loop spawned by `subscribe()`, if any. No-op if
    /// `subscribe()` was never called or has already been aborted. The session
    /// itself stays open — `query()` / `checkpoint()` keep working against the
    /// on-device store; only live replication pauses.
    pub async fn abort_subscribe(&self) {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            if let Some(handle) = session.run_handle.take() {
                handle.abort();
            }
        }
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

impl Drop for NostosState {
    fn drop(&mut self) {
        // Move the runtime out so its blocking shutdown doesn't run inline.
        // Dropping a multi-thread `tokio::runtime::Runtime` panics inside an
        // async context ("Cannot drop a runtime in a context where blocking is
        // not allowed") — the `#[tokio::test]` case. When a runtime context is
        // ambient, off-load the drop to a std thread (clean shutdown happens
        // off the async worker; the test process reclaims resources on exit).
        // In production (Tauri's sync `setup` hook) `NostosState` drops outside
        // any async context, so the inline branch runs — no extra thread hop.
        if let Some(rt) = self.rt.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                std::thread::spawn(move || drop(rt));
            } else {
                drop(rt);
            }
        }
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

/// Start the live-replication run loop for `table` on the active session.
///
/// Without this exposed, a JS frontend could `connect` (which opens SQLite and
/// builds the client but does NO network I/O) and then wait forever: nothing
/// drives `run_once`, so no server-pushed row ever lands. It was registered in
/// neither `generate_handler!` nor `build.rs`, so the whole download path was
/// unreachable from JS while `cargo test` stayed green — the Rust test calls
/// `NostosState::subscribe` directly and never crosses the command boundary.
#[tauri::command]
async fn subscribe(state: State<'_, NostosState>, table: String) -> Result<(), String> {
    state.subscribe(table).await
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
            subscribe,
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

    // -------------------------------------------------------------------------
    // Live-replication E2E — copies the Rust reference template
    // (`crates/nostos-client/tests/e2e_live_replication.rs`) against the same
    // shared spine binary (`nostos-infra/examples/e2e_server`). Proves a full
    // server→client→server round-trip through `NostosState`'s REAL public API
    // (connect / subscribe / write / query), with no Tauri `AppHandle`, no
    // Postgres, no docker.
    // -------------------------------------------------------------------------

    /// Body the spine injects on `POST /push` — matches the reference template
    /// shape (PK only differs so the assertion is unambiguous).
    const PUSH_BODY: &str = r#"{"pk":"tauri-push","payload":{"title":"from-server","status":"open","priority":"5"}}"#;

    /// Live-replication E2E against the shared spine server. Drives the SAME
    /// two-direction round-trip the Rust reference template proves, entirely
    /// through `NostosState`'s public API: connect → subscribe → server PUSH
    /// arrives on-device → `query()` sees it → `write()` → server echoes →
    /// `query()` sees the write.
    #[tokio::test(flavor = "multi_thread")]
    async fn nostos_state_live_round_trip_against_spine() {
        let (port, mut child) = spawn_spine().await;

        // PID-unique DB path so a stale file from a prior run can't yield a
        // false positive (mirrors the reference template).
        let db_path = std::env::temp_dir()
            .join(format!("nostos-tauri-e2e-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        let state = NostosState::new();
        let url = format!("ws://127.0.0.1:{port}/sync");
        state
            .connect(url, None, db_path.to_str().expect("utf8 db path").to_owned())
            .await
            .expect("connect");

        // Start the live replication run loop on the owned runtime.
        state.subscribe("tasks".into()).await.expect("subscribe");
        // Let the subscribe land + the session register with the fan-out
        // service (the spine only delivers to sessions registered at fan-out
        // time, per the reference template).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ---- direction 1: server PUSH → on-device query ----
        http_push(port, PUSH_BODY).await;
        poll_query_pk(&state, "tauri-push", Duration::from_secs(8))
            .await
            .expect("pushed row never became queryable");
        println!("[tauri-e2e] PUSH_OK");

        // ---- direction 2: client WRITE → server echo → on-device query ----
        state
            .write(
                "tasks".into(),
                "upsert".into(),
                "tauri-echo".into(),
                Some(
                    r#"{"title":"from-client","status":"open","priority":"5"}"#.into(),
                ),
            )
            .await
            .expect("write");
        poll_query_pk(&state, "tauri-echo", Duration::from_secs(8))
            .await
            .expect("echoed write never became queryable");
        println!("[tauri-e2e] ECHO_OK");

        // Cleanup: abort the run loop, kill the spine, remove the temp DB.
        state.abort_subscribe().await;
        let _ = child.kill().await;
        let _ = std::fs::remove_file(&db_path);
        println!("[tauri-e2e] DONE");
    }

    /// Poll the on-device store via `query()` until a row for `pk` appears in
    /// `cairn_data` (the apply engine's target table) or `deadline` elapses.
    /// Returns `Some(())` once the row is queryable.
    async fn poll_query_pk(state: &NostosState, pk: &str, deadline: Duration) -> Option<()> {
        let sql = format!(
            "SELECT pk FROM cairn_data WHERE table_name = 'tasks' AND pk = '{pk}'"
        );
        let end = tokio::time::Instant::now() + deadline;
        loop {
            let rows_json = state.query(sql.clone()).await.expect("query");
            // query() returns a JSON array-of-objects string; parse + check
            // non-empty (the apply engine writes the row once it arrives).
            let rows: serde_json::Value =
                serde_json::from_str(&rows_json).expect("parse rows json");
            if rows.as_array().is_some_and(|a| !a.is_empty()) {
                return Some(());
            }
            if tokio::time::Instant::now() >= end {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Minimal HTTP/1.1 POST /push over a raw TCP stream — no HTTP dep (the
    /// spine's control endpoint is localhost-only). Mirrors the reference
    /// template's `http_push`.
    async fn http_push(port: u16, body: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .expect("connect spine");
        let req = format!(
            "POST /push HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
        let head = String::from_utf8_lossy(&buf[..buf.len().min(40)]);
        assert!(head.contains("200"), "POST /push non-200: {head}");
    }

    /// Spawn the spine binary, discover its port via the `NOSTOS_E2E_PORT`
    /// stdout line. Mirrors the reference template's spawn + ancestor-walking
    /// path lookup.
    async fn spawn_spine() -> (u16, tokio::process::Child) {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::process::Command;

        let exe = spine_binary_path();
        if !exe.exists() {
            // This crate is a SEPARATE workspace from the root, so build the
            // spine against the ROOT workspace's Cargo.toml (where
            // `nostos-infra` lives). `cargo build -p nostos-infra` from
            // sdk/nostos_tauri would fail to resolve the package.
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let root = manifest
                .parent()
                .and_then(|p| p.parent())
                .expect("resolve root workspace from sdk/nostos_tauri");
            let status = std::process::Command::new("cargo")
                .args(["build", "-p", "nostos-infra", "--example", "e2e_server"])
                .arg("--manifest-path")
                .arg(root.join("Cargo.toml"))
                .status()
                .expect("cargo build spine");
            assert!(status.success(), "build spine failed");
        }
        let exe = spine_binary_path();
        assert!(exe.exists(), "spine binary not found at {}", exe.display());

        let mut child = Command::new(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn spine {}: {e}", exe.display()));

        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();
        let mut port: Option<u16> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            let line = match tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await {
                Ok(Ok(Some(l))) => l,
                Ok(Ok(None) | Err(_)) => break,
                Err(_) => continue,
            };
            if let Some(rest) = line.strip_prefix("NOSTOS_E2E_PORT=") {
                port = rest.trim().parse::<u16>().ok();
            }
            if line.trim() == "NOSTOS_E2E_READY" {
                break;
            }
        }
        // Keep the stdout pipe drained in the background so the child isn't
        // SIGPIPE'd once its pipe buffer fills.
        tokio::spawn(async move {
            while let Ok(Ok(Some(_))) =
                tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await
            {}
        });
        let port = port.expect("never saw NOSTOS_E2E_PORT");
        eprintln!("[tauri-e2e] spine on port {port}");
        (port, child)
    }

    /// Resolve the built spine binary. The root workspace's `target/` is
    /// shared across all root members, so walk up from `CARGO_MANIFEST_DIR`
    /// (sdk/nostos_tauri) to find it.
    fn spine_binary_path() -> std::path::PathBuf {
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let rel = std::path::Path::new("target")
            .join(profile)
            .join("examples")
            .join("e2e_server");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut dir: Option<&std::path::Path> = Some(manifest);
        while let Some(d) = dir {
            let candidate = d.join(&rel);
            if candidate.exists() {
                return candidate;
            }
            dir = d.parent();
        }
        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd.join(&rel);
            if candidate.exists() {
                return candidate;
            }
        }
        manifest.join(&rel)
    }
}
