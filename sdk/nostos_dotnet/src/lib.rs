//! `nostos-dotnet` — UniFFI bridge exposing `nostos_client::SyncClient<SqliteStorage>`
//! to .NET (iOS / Android / Windows / macOS). Mirrors `sdk/nostos_swift/src/lib.rs`
//! and `sdk/nostos_kotlin/src/lib.rs` — the SAME `SyncClient<SqliteStorage>` the
//! native, Tauri, Flutter, Swift, Kotlin, and Node SDKs drive, loaded into .NET
//! via UniFFI's proc-macro FFI, with no engine/wire changes.
//!
//! # Why this exists
//! Feasibility scaffold for the "cheap-catch-up multi-platform" thesis: prove
//! the SAME client the six sibling SDKs drive can be loaded from .NET, with no
//! engine/wire changes. The binding is **Nord UniFFI-CS** (`uniffi-bindgen-cs`
//! tag `v0.9.2+v0.28.3`) — the same proc-macro surface as nostos_swift/kotlin,
//! one Rust interface, four foreign bindings (Swift, Kotlin, C#, …). Scope is
//! "Rust compiles for host + iOS + iOS-sim + Android + Windows-msvc (link-fail
//! expected) + UniFFI generates C#" — NOT a polished SDK.
//!
//! # Runtime shape
//! `NostosClient` owns a `tokio::runtime::Runtime` (the same shape
//! `sdk/nostos_swift`'s and `sdk/nostos_kotlin`'s `NostosClient` use) so that
//! `connect`/`write`/`query`/`checkpoint` — all of which `.await` on
//! `SyncClient`'s async API — can be surfaced to .NET as **synchronous**
//! methods. UniFFI async (the `tokio` feature + `#[uniffi::method(async_runtime
//! = "tokio")]`) is the alternative path but adds ForeignFuture plumbing
//! friction that is not load-bearing for a scaffold; `block_on` from the
//! foreign (.NET P/Invoke) thread is simpler and matches `nostos_swift`'s and
//! `nostos_kotlin`'s "own runtime, block internally" precedent. .NET's calling
//! thread blocks briefly on each call; a future `subscribe()` run loop will be
//! spawned onto the owned runtime (mirrors `nostos_node::NostosClient::subscribe`).
//!
//! # ponytail: `unsafe` policy
//! Hand-written `unsafe` is forbidden in this crate (`#![forbid(unsafe_code)]`).
//! UniFFI's macro-generated FFI scaffolding lives in the `uniffi` dependency's
//! proc-macro output, not in this crate's hand-written source, so the forbid
//! does not interact with it — same precedent as `nostos_swift`, `nostos_kotlin`,
//! `nostos_tauri` (tauri's macro FFI), and `nostos_node` (napi-derive macro FFI).
//! ADR-0015 addendum: machine-generated FFI glue is the one workspace-wide
//! exception. The C# side (`dotnet/Nostos.DotNet.csproj`) sets
//! `<AllowUnsafeBlocks>true</AllowUnsafeBlocks>` because the Nord bindgen
//! emits P/Invoke pointers (`IntPtr` / `Unsafe.AsPointer<>`) — that flag is a
//! .NET-project property, not a Rust property; the Rust crate stays forbid-unsafe.
//!
//! # ponytail: deferred surfaces (upgrade path)
//! - **`subscribe(table)` run loop + poll**: WIRED. `subscribe()` spawns
//!   `client.run_with_reconnect()` on the owned runtime; the loop drives the
//!   WS session (subscribe-ack + drain + flush) and applies incoming rows to
//!   the on-device SQLite store via the engine. .NET polls `query()` until the
//!   expected row appears — the SAME shape the Rust E2E template
//!   (`crates/nostos-client/tests/e2e_live_replication.rs`) uses, and the exact
//!   shape `sdk/nostos_swift` + `sdk/nostos_kotlin` shipped. Ceiling: no row-tick
//!   callback / push notification to .NET yet — callers discover new rows by
//!   polling. Upgrade path: a UniFFI callback interface for row-ticks (same
//!   shape as the Flutter `rows_sink`), or a `poll_new_rows()` drain over
//!   `SyncClient::subscribe_changes()`'s broadcast channel.
//! - **Windows cross-compile**: `cargo build --target x86_64-pc-windows-msvc`
//!   compiles the Rust to `.rlib`/`.dll` objects but FAILS at link on macOS
//!   (no Windows SDK / MSVC linker on this host). This is a KNOWN limitation
//!   of cross-compiling to Windows from macOS — NOT a blocker. Upgrade path:
//!   build the Windows artifact in CI on a `windows-latest` runner, or install
//!   the Windows SDK + lld-linker on this host. The Rust source is
//!   Windows-clean (no platform-specific code); only the link step fails.
//! - **NuGet packaging**: the committed `dotnet/generated/NostosClient.cs` is
//!   the bindgen output; wrapping it as a NuGet `.nupkg` with multi-TFM
//!   `runtimes/<RID>/native/libnostos_dotnet.(dll|dylib|so)` is the next
//!   increment past the `cargo build + bindgen generate` gate.

#![forbid(unsafe_code)]
// UniFFI proc-macro surface: clippy pedantic noise about "missing_errors_doc"
// on the FFI methods is not load-bearing for a scaffold; keep the surface
// readable instead (mirrors nostos_swift's + nostos_kotlin's allow list).
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
// `uniffi-bindgen-cs --library` reads to produce C# bindings. The argument is
// the UniFFI namespace (becomes the generated C# namespace `Nostos` and the
// FFI symbol prefix). nostos_swift uses `nostos_swift`, nostos_kotlin uses
// `nostos_kotlin`; nostos_dotnet uses bare `nostos` so the C# namespace reads
// `Nostos.NostosClient` (cleaner for the .NET consumer — matches the namespace
// requirement in the scaffold brief).
uniffi::setup_scaffolding!("nostos");

/// Session-level reconnect backstop — mirrors `sdk/nostos_swift`'s and
/// `sdk/nostos_kotlin`'s `IDLE_RECONNECT_BACKSTOP` and the Flutter glue's
/// constant of the same name. Long relative to per-batch flush bounds: this is
/// a rare defense-in-depth reconnect, not a per-write latency mechanism.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// UniFFI-visible error type. UniFFI 0.28 refuses to bindgen `Result<_, String>`
/// ("unknown throw type: Some(String)"); every FFI method therefore returns
/// `Result<_, NostosError>`, with the message preserved verbatim from the
/// underlying `StorageError` / `ClientError` / `serde_json::Error`. The single
/// `Message` variant keeps the C# side a simple `throw new NostosError.Message`
/// — matching `nostos_swift`'s + `nostos_kotlin`'s enum and `nostos_node`'s
/// single-reason `napi::Error::from_reason` shape.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NostosError {
    #[error("{message}")]
    Message { message: String },
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

/// A live Nostos client handle for .NET. Owns the tokio runtime the
/// `SyncClient`'s async API runs on, plus at most one active session (v1: one
/// table per client, matching `nostos-client`'s Phase-0 predicate floor and the
/// sibling SDKs).
///
/// Construct via `NostosClient(url, token, dbPath)` then call `connect()`
/// (opens the local SQLite store + builds the `SyncClient` — no network) and
/// drive `write` / `query` / `checkpoint`. All four are synchronous from
/// .NET's view — see the module `ponytail:` for why we chose sync-over-block
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
/// replacing it — releases the `Arc<SyncClient<SqliteStorage>>` AND aborts the
/// background run loop (`run_task`) so a superseded session's WebSocket +
/// reconnect loop actually stops instead of leaking. Mirrors `nostos_swift`'s
/// and `nostos_kotlin`'s `Session` shape verbatim.
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

#[uniffi::export]
impl NostosClient {
    /// Construct a handle. Does no network I/O and does not open the store yet
    /// — `connect()` does. `db_path` is the SQLite file path (rusqlite accepts
    /// `":memory:"` for an ephemeral store, useful for tests).
    ///
    /// # Errors
    /// `NostosError` if the owned tokio runtime fails to initialize (resource
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
    /// against `url`. No network I/O — `subscribe()` is what starts the live
    /// replication loop. Idempotent: a second call while a session is live is
    /// a no-op. The default table is `tasks` (matches `nostos_swift`,
    /// `nostos_kotlin`, `nostos_node`, and `nostos_tauri`).
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
                run_task: None,
            });
            Ok(())
        })
    }

    /// Start the live replication loop on the owned runtime. Spawns
    /// `client.run_with_reconnect()` — the loop opens the WS session
    /// (subscribe-ack + drain + flush) and applies incoming rows to the
    /// on-device SQLite store via the engine, exactly as the Rust E2E
    /// template (`crates/nostos-client/tests/e2e_live_replication.rs`) drives
    /// it, and exactly as `sdk/nostos_swift`'s and `sdk/nostos_kotlin`'s
    /// `subscribe()` do. Returns immediately; the loop runs until the session
    /// is dropped (Drop aborts the task) or the process exits.
    ///
    /// `table` is accepted for API symmetry with `nostos_node::subscribe(table,
    /// _)` and the upcoming per-table session floor. Today the session's
    /// table is fixed at `connect()` time (default `"tasks"`); a mismatched
    /// `table` here is a programming error.
    ///
    /// # ponytail: poll-only
    /// UniFFI 0.28's async-callback path (the natural fit for a row-tick
    /// callback into .NET) is fiddly enough to defer; the run loop applies
    /// rows to storage as they arrive, and .NET polls `query()` until the
    /// expected row appears (same shape as the Rust E2E template and the
    /// Swift / Kotlin SDKs). A future `poll_new_rows()` draining
    /// `SyncClient::subscribe_changes()`'s broadcast channel is the upgrade
    /// path if `query()` polling proves too coarse.
    ///
    /// # Errors
    /// `NostosError` if no session is active (call `connect()` first) or the
    /// requested `table` does not match the session fixed at `connect()` time.
    pub fn subscribe(&self, table: String) -> Result<(), NostosError> {
        self.rt.block_on(async {
            let mut guard = self.session.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| NostosError::Message {
                    message: "subscribe() called before connect()".to_string(),
                })?;
            if session.table != table {
                return Err(NostosError::Message {
                    message: format!(
                        "subscribe() table {table:?} does not match active session table {:?} — v1 supports one table per NostosClient",
                        session.table
                    ),
                });
            }
            // Idempotent: a second subscribe() while a run loop is live is a
            // no-op (mirrors `connect()`'s idempotency).
            if session.run_task.is_some() {
                return Ok(());
            }
            let client = Arc::clone(&session.client);
            // Spawn on OUR runtime (not UniFFI's) so the loop outlives this
            // call. `run_with_reconnect` retries forever on transport errors;
            // the task only completes on a terminal error, which we swallow
            // (auto-reconnect is the contract — a real write surfaces its own
            // error). Session::Drop aborts this handle on replacement / client
            // drop.
            let run_task = self.rt.spawn(async move {
                let _ = client.run_with_reconnect().await;
            });
            session.run_task = Some(run_task);
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
    /// name) — the same shape `nostos_swift`'s, `nostos_kotlin`'s, `nostos_node`'s,
    /// and `nostos_tauri`'s `query()` emit. Requires `connect()` to have run.
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
    /// `NostosClient` shape, with no live .NET runtime required. Mirrors
    /// `nostos_swift`'s + `nostos_kotlin`'s offline smoke path (construct +
    /// query round-trip).
    #[test]
    fn nostos_client_offline_connect_query_round_trip() {
        let client =
            NostosClient::new("ws://localhost:0".into(), None, ":memory:".into()).expect("construct");

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
    /// panicking — the same contract `nostos_swift`, `nostos_kotlin`,
    /// `nostos_tauri`, and `nostos_node` enforce.
    #[test]
    fn write_before_connect_is_an_error() {
        let client =
            NostosClient::new("ws://localhost:0".into(), None, ":memory:".into()).expect("construct");

        let err = client
            .write("tasks".into(), "upsert".into(), "pk1".into(), None)
            .expect_err("write before connect should error");
        let msg = err.to_string();
        assert!(
            msg.contains("before connect"),
            "expected a before-connect error, got: {msg}"
        );
    }

    /// `subscribe()` before `connect()` surfaces a clear error — the same
    /// before-connect contract `write()` enforces. Mirrors `nostos_swift`'s +
    /// `nostos_kotlin`'s `subscribe_before_connect_is_an_error`.
    #[test]
    fn subscribe_before_connect_is_an_error() {
        let client =
            NostosClient::new("ws://localhost:0".into(), None, ":memory:".into()).expect("construct");

        let err = client
            .subscribe("tasks".into())
            .expect_err("subscribe before connect should error");
        let msg = err.to_string();
        assert!(
            msg.contains("before connect"),
            "expected a before-connect error, got: {msg}"
        );
    }

    /// `subscribe()` with a table that doesn't match the session fixed at
    /// `connect()` time surfaces a clear error — the same one-table-per-client
    /// guard `write()` enforces. Mirrors `nostos_swift`'s +
    /// `nostos_kotlin`'s `subscribe_table_mismatch_is_an_error`.
    #[test]
    fn subscribe_table_mismatch_is_an_error() {
        let client =
            NostosClient::new("ws://localhost:0".into(), None, ":memory:".into()).expect("construct");
        client.connect().expect("connect");

        let err = client
            .subscribe("not-tasks".into())
            .expect_err("mismatched-table subscribe should error");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match"),
            "expected a table-mismatch error, got: {msg}"
        );
    }

    /// `subscribe()` after `connect()` spawns the run loop and returns Ok.
    /// The loop tries to reach `ws://localhost:0` and fails forever; we
    /// swallow the error inside the spawned task (auto-reconnect contract).
    /// What we ARE proving here: (1) the call returns Ok, (2) it's idempotent
    /// (a second call is a no-op), (3) Drop cleans up the spawned task
    /// (Session::Drop aborts it; the test passing without hanging on runtime
    /// shutdown is the proof). Mirrors `nostos_swift`'s +
    /// `nostos_kotlin`'s `subscribe_after_connect_spawns_run_loop`.
    #[test]
    fn subscribe_after_connect_spawns_run_loop() {
        let client =
            NostosClient::new("ws://localhost:0".into(), None, ":memory:".into()).expect("construct");
        client.connect().expect("connect");

        client.subscribe("tasks".into()).expect("subscribe");
        // Idempotent: a second subscribe is a no-op (the run_task is already
        // Some). If this re-spawned, we'd leak a second loop and the session
        // Drop would only abort the latest.
        client
            .subscribe("tasks".into())
            .expect("subscribe idempotent");

        // Drop the client: the runtime shuts down, Session::Drop aborts the
        // spawned run_with_reconnect task. If abort is broken, this test
        // hangs on runtime shutdown.
        drop(client);
    }
}
