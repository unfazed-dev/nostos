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
//! - **`subscribe(table)` run loop**: WIRED. `subscribe()` spawns
//!   `client.run_with_reconnect()` on the owned runtime; the loop drives the
//!   WS session (subscribe-ack + drain + flush) and applies incoming rows to
//!   the on-device SQLite store via the engine. `subscribe()` stays the
//!   run-loop DRIVER — reactive push is a separate call (`watch()`).
//! - **`watch(table, sink)` reactive push**: WIRED. A TRUE Rust→.NET push via
//!   a UniFFI SYNCHRONOUS callback interface (`SnapshotSink::on_snapshot`),
//!   draining `SyncClient::subscribe_changes()`'s broadcast on the owned
//!   runtime — the .NET port of Flutter's `watch(table, rows_sink)` and
//!   Kotlin's `watch(table, sink)` (commit 41265fd). The Nord UniFFI-CS
//!   bindgen (v0.9.2+v0.28.3) supports `#[uniffi::export(with_foreign)]` the
//!   same way mainline UniFFI does for Kotlin/Swift — verified empirically
//!   (bindgen generates an `ISnapshotSink` C# interface + the foreign-callback
//!   vtable from the Rust trait). The app consumer implements `SnapshotSink`
//!   (typically adapting `OnSnapshot` onto an `IObservable<string>` /
//!   `Channel<string>`) and receives full-snapshot-per-tick callbacks; it
//!   never wall-clock-polls.
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
use tokio::sync::broadcast::error::RecvError;
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

/// Reactive push channel: .NET implements this interface, Rust invokes it.
///
/// This is the .NET port of the Kotlin SDK's `SnapshotSink` (commit 41265fd)
/// and Flutter's `rows_sink: StreamSink<String>` — a TRUE Rust→foreign PUSH
/// (the app consumer does NOT poll). Chosen over a C#-side poll over
/// `subscribe_changes` because it is the faithful reactive port and the
/// Nord UniFFI-CS bindgen (v0.9.2+v0.28.3) supports the `with_foreign`
/// callback-interface the same way mainline UniFFI does for Kotlin/Swift
/// (verified empirically — bindgen generates a C# `ISnapshotSink` interface
/// + the foreign-callback vtable).
///
/// # Why a SYNC callback (UniFFI 0.28)
/// UniFFI 0.28's **async**-foreign-callback path (a foreign-implemented
/// method that returns a `Future`) is genuinely awkward — that is NOT what we
/// use. A fire-and-forget `on_snapshot(json) -> ()` is a SYNCHRONOUS foreign
/// callback (`#[uniffi::export(with_foreign)]`), the stable, well-supported
/// path in UniFFI 0.28: the Rust pump task invokes the callback through
/// UniFFI's vtable (callable from any Rust thread, including a tokio worker),
/// blocking that worker only for the duration of the C# method body (which a
/// sink just forwards to a `Channel<T>` / `IObservable<T>` — microseconds).
/// `with_foreign` (vs the legacy `callback_interface`) ALSO permits a RUST
/// impl, which is what the host reactivity test exercises without a .NET
/// runtime.
///
/// # Snapshot shape
/// `json` is a JSON array-of-objects string: one object per row of the watched
/// table's rows in `cairn_data`, full snapshot per tick (NOT a diff —
/// self-healing on lag, mirrors Flutter's `emit_snapshot`).
#[uniffi::export(with_foreign)]
pub trait SnapshotSink: Send + Sync {
    /// Receive a full-table snapshot. Invoked once with the initial snapshot
    /// (immediately after `watch()` subscribes) and again after every change
    /// tick (remote apply or local write).
    fn on_snapshot(&self, json: String);
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
/// background run loop (`run_task`) AND every `watch()` pump (`watch_tasks`)
/// so a superseded session's WebSocket + reconnect loop + reactive pumps
/// actually stop instead of leaking. Mirrors `nostos_swift`'s and
/// `nostos_kotlin`'s `Session` shape, extended with the reactive pumps
/// Flutter's `Session` carries.
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
    run_task: Option<tokio::task::JoinHandle<()>>,
    /// One pump per `watch()` call. Each owns its own `subscribe_changes()`
    /// receiver. Aborted on session teardown (Drop) so the watch lifecycle is
    /// tied to the sync session — cancels on `connect()`-replacing-a-session or
    /// client drop. Mirrors Flutter's `session.watch_tasks`.
    watch_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Replay cache (Flutter's `_replayLatest` port): the last snapshot JSON
    /// emitted for this session's table. The no-replay Rust broadcast
    /// (`broadcast::channel(64)` in nostos-client) means a LATE subscriber's own
    /// `subscribe_changes()` receiver can't see prior ticks — this cache lets a
    /// late `watch()` replay the last emitted snapshot instantly (no storage
    /// round-trip) instead of forcing it to wait for the next tick. The first
    /// subscriber (empty cache) falls back to a live storage query (source of
    /// truth), which is then cached for the next subscriber.
    last_snapshot: Arc<AsyncMutex<Option<String>>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = self.run_task.take() {
            task.abort();
        }
        // Abort every reactive pump too — the watches are tied to this session,
        // so they must not outlive it. (A pump whose receiver goes Closed on
        // client drop would exit anyway, but abort is immediate + explicit and
        // guards against a pump mid-storage-query.)
        for task in self.watch_tasks.drain(..) {
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
                watch_tasks: Vec::new(),
                last_snapshot: Arc::new(AsyncMutex::new(None)),
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
    /// # Reactive push vs poll
    /// `subscribe()` itself stays the run-loop driver (it does NOT push row
    /// ticks into .NET). Reactive push is a SEPARATE call — `watch(table,
    /// sink)` — which drains `SyncClient::subscribe_changes()`'s broadcast on
    /// the owned runtime and invokes a `SnapshotSink` callback per tick (the
    /// .NET port of Flutter's `rows_sink` / Kotlin's `SnapshotSink`, commit
    /// 41265fd). Callers who want push implement `SnapshotSink`; callers who
    /// want poll still have `query()`. `subscribe()` + `watch()` compose:
    /// `subscribe()` keeps the store fed, `watch()` fans ticks out.
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
                let session = guard.as_ref().ok_or_else(|| NostosError::Message {
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
                let session = guard.as_ref().ok_or_else(|| NostosError::Message {
                    message: "checkpoint() called before connect()".to_string(),
                })?;
                Arc::clone(&session.client)
            };
            let lsn: Lsn = client.checkpoint().await.map_err(NostosError::wrap)?;
            Ok(lsn.0)
        })
    }

    /// Reactive watch: emit the full-table snapshot to `sink` immediately, and
    /// again after every change tick (remote apply or local write). This is
    /// the .NET port of Flutter's `watch(table, rows_sink)` and Kotlin's
    /// `watch(table, sink)` (commit 41265fd) — a TRUE Rust→.NET push via a
    /// UniFFI callback interface, not a poll. The .NET consumer implements
    /// [`SnapshotSink`] and receives `on_snapshot(json)` calls; it never
    /// wall-clock-polls the store. The natural C# adapter is an
    /// `IObservable<string>` / `Channel<string>` fed from `OnSnapshot`.
    ///
    /// One pump per call. The pump's lifecycle is tied to the sync session:
    /// `Session::Drop` (on a session-replacing `connect()` or client drop)
    /// aborts every pump. There is no per-watch handle to cancel today (the
    /// floor; a `stop_watch(table)` is the mechanical follow-on if a caller
    /// needs to unsubscribe mid-session).
    ///
    /// `table` MUST match the active session's table (v1: one table per client).
    ///
    /// # Load-bearing ordering: subscribe BEFORE the first snapshot read
    /// The nostos-client change broadcast is no-replay
    /// (`broadcast::channel(64)`). A receiver created AFTER a commit permanently
    /// misses that commit — the "connected but lists render empty" regression.
    /// The invariant is encoded directly in nostos-client at
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`, and this
    /// port honors it: the broadcast receiver is created FIRST, the initial
    /// snapshot is read AFTER. A commit in the residual gap just triggers a
    /// redundant re-snapshot from the pump (idempotent — full snapshot,
    /// self-healing on lag).
    ///
    /// # Errors
    /// `NostosError` if `connect()` hasn't run or `table` doesn't match the
    /// session fixed at `connect()` time.
    pub fn watch(&self, table: String, sink: Arc<dyn SnapshotSink>) -> Result<(), NostosError> {
        self.rt.block_on(async {
            let mut guard = self.session.lock().await;
            let session = guard
                .as_mut()
                .ok_or_else(|| NostosError::Message {
                    message: "watch() called before connect()".to_string(),
                })?;
            if session.table != table {
                return Err(NostosError::Message {
                    message: format!(
                        "watch() table {table:?} does not match active session table {:?} — v1 supports one table per NostosClient",
                        session.table
                    ),
                });
            }

            // (1) SUBSCRIBE FIRST — load-bearing (see method doc). Must precede
            // the initial snapshot read below; this receiver is the only way to
            // learn of a commit that lands in the gap before the pump starts.
            let mut changes = session.client.subscribe_changes();

            // (2) Initial snapshot AFTER subscribing. Replay cache first: a late
            // subscriber (a second `watch()` for the same table after data has
            // already flowed) gets the last-emitted snapshot instantly without a
            // storage round-trip. First subscriber (empty cache) falls back to a
            // live storage query — the source of truth — which is then cached.
            let cached = session.last_snapshot.lock().await.clone();
            let initial_json = match cached {
                Some(json) => json,
                None => {
                    let json = snapshot_json(&session.client, &table).await?;
                    *session.last_snapshot.lock().await = Some(json.clone());
                    json
                }
            };
            sink.on_snapshot(initial_json);

            // (3) Pump: re-snapshot on EVERY change tick. Full snapshot per tick
            // (not a diff — self-healing on lag). Each watch owns its own
            // receiver; a tick on a different table just re-queries cheaply.
            // `Lagged` (the receiver fell >64 ticks behind) is treated as a tick
            // — a full snapshot resyncs. `Closed` (the client dropped its
            // senders) exits the pump.
            let pump_client = Arc::clone(&session.client);
            let pump_sink = Arc::clone(&sink);
            let pump_cache = Arc::clone(&session.last_snapshot);
            let pump_task = self.rt.spawn(async move {
                // Ok / Lagged → re-snapshot + emit. Closed (the client dropped
                // its senders) fails the `while let` and the pump exits.
                while let Ok(_) | Err(RecvError::Lagged(_)) = changes.recv().await {
                    // Snapshot read failure (e.g. transient busy) is best-effort:
                    // skip this tick, the next one retries. Mirrors Flutter's
                    // emit-on-tick contract.
                    if let Ok(json) = snapshot_json(&pump_client, &table).await {
                        {
                            let mut cache = pump_cache.lock().await;
                            *cache = Some(json.clone());
                        }
                        pump_sink.on_snapshot(json);
                    }
                }
            });
            session.watch_tasks.push(pump_task);
            Ok(())
        })
    }
}

/// Read the full row snapshot for `table` as a JSON array-of-objects string.
///
/// Queries `cairn_data` directly (NOT a `SELECT * FROM {table}` VIEW): the
/// `tasks`/etc. VIEW is only created by `SqliteStorage::apply_schema` once the
/// server has shipped a schema, but `cairn_data` exists on every store right
/// after `open()` (`CREATE TABLE IF NOT EXISTS cairn_data` in
/// `nostos-client/src/sqlite.rs`). So this snapshot succeeds on a fresh/empty
/// store (returning `"[]"`) as well as a populated one — the correct
/// offline-first UX. `table` is the session-validated value (the caller's
/// `watch()` already confirmed it equals the fixed session table), so the
/// interpolation is injection-safe; the canonical per-table snapshot query is
/// `SELECT pk, payload FROM cairn_data WHERE table_name = ?1 ...`
/// (nostos-client/src/sqlite.rs).
///
/// Mirrors `nostos_kotlin`'s `snapshot_json` verbatim (commit 41265fd).
async fn snapshot_json(
    client: &Arc<SyncClient<SqliteStorage>>,
    table: &str,
) -> Result<String, NostosError> {
    let sql =
        format!("SELECT pk, payload FROM cairn_data WHERE table_name = '{table}' ORDER BY pk ASC");
    let rows = client
        .with_storage(move |s| s.query(&sql))
        .await
        .map_err(|e: ClientError| NostosError::wrap(e))?
        .map_err(NostosError::wrap)?;
    serde_json::to_string(&rows).map_err(NostosError::wrap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only [`SnapshotSink`] that records every emitted snapshot into a
    /// `std::sync::mpsc` channel. `mpsc::Sender` is `Send` but not `Sync`, so
    /// it is wrapped in a `Mutex` (which IS `Send + Sync`) to satisfy the
    /// `SnapshotSink: Send + Sync` bound. The test thread receives via
    /// `recv_timeout` — a blocking EVENT wait on the callback, NOT a wall-clock
    /// poll of the SDK. This is the honest reactivity proof.
    struct RecordingSink(std::sync::Mutex<std::sync::mpsc::Sender<String>>);

    impl SnapshotSink for RecordingSink {
        fn on_snapshot(&self, json: String) {
            // Best-effort: a dropped receiver (test gone) is fine; the pump
            // keeps running until Session::Drop aborts it.
            let _ = self.0.lock().expect("sink lock").send(json);
        }
    }

    /// Proof-of-integration: the SAME `SyncClient<SqliteStorage>` the sibling
    /// SDKs drive constructs + serves an offline query through the UniFFI
    /// `NostosClient` shape, with no live .NET runtime required. Mirrors
    /// `nostos_swift`'s + `nostos_kotlin`'s offline smoke path (construct +
    /// query round-trip).
    #[test]
    fn nostos_client_offline_connect_query_round_trip() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
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
    /// panicking — the same contract `nostos_swift`, `nostos_kotlin`,
    /// `nostos_tauri`, and `nostos_node` enforce.
    #[test]
    fn write_before_connect_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
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

    /// `subscribe()` before `connect()` surfaces a clear error — the same
    /// before-connect contract `write()` enforces. Mirrors `nostos_swift`'s +
    /// `nostos_kotlin`'s `subscribe_before_connect_is_an_error`.
    #[test]
    fn subscribe_before_connect_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");

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
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
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
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
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

    /// REACTIVITY PROOF (host, no device/.NET runtime): `watch()` emits the
    /// initial snapshot, and a local `write()` — which applies a row to
    /// `cairn_data` AND fires the change broadcast (nostos-client/client.rs
    /// invariant `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`,
    /// `rows_applied == 1`) — causes the pump to emit a NEW snapshot, WITHOUT
    /// the test polling a timer. `recv_timeout` blocks on the callback
    /// delivery (an event wait), so this is reactive-by-callback, not
    /// reactive-by-poll. Mirrors `nostos_kotlin`'s
    /// `watch_emits_initial_snapshot_then_refires_on_local_write` (commit
    /// 41265fd) verbatim.
    ///
    /// This also implicitly covers the subscribe-before-snapshot invariant: if
    /// `watch()` read the snapshot BEFORE subscribing, a write racing in that
    /// gap would be missed. The dedicated nostos-client test
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot` pins the
    /// engine side; this test pins the FFI port's ordering (initial snapshot
    /// emitted, then the post-write snapshot arrives).
    #[test]
    fn watch_emits_initial_snapshot_then_refires_on_local_write() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
        client.connect().expect("connect");

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let sink = Arc::new(RecordingSink(std::sync::Mutex::new(tx))) as Arc<dyn SnapshotSink>;

        // watch() subscribes (broadcast receiver created BEFORE the initial
        // snapshot read — the load-bearing invariant) and emits the initial
        // snapshot synchronously before returning.
        client.watch("tasks".into(), sink).expect("watch");

        // (1) Initial snapshot delivered — empty store → "[]" (cairn_data has
        // no rows for tasks yet). No polling: blocking event wait, 5s ceiling.
        let initial = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("initial snapshot should arrive immediately");
        assert_eq!(
            initial, "[]",
            "fresh store tasks snapshot should be empty array"
        );

        // (2) Local write applies a row to cairn_data AND fires the change
        // broadcast tick. The pump (on the owned runtime) wakes, re-snapshots,
        // and fires on_snapshot AGAIN — the reactive proof.
        client
            .write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                Some(r#"{"id":"pk1","title":"reactive"}"#.to_owned()),
            )
            .expect("write");

        // No polling: blocking event wait on the NEXT callback delivery.
        let after_write = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("post-write snapshot should arrive reactively");
        assert!(
            after_write.contains("pk1"),
            "post-write snapshot should contain the upserted row, got: {after_write}"
        );

        // Drop the client: Session::Drop aborts the pump. If abort is broken,
        // this test hangs on runtime shutdown.
        drop(client);
    }

    /// `watch()` before `connect()` surfaces a clear error — the same
    /// before-connect contract `write()`/`subscribe()` enforce. Mirrors
    /// `nostos_kotlin`'s `watch_before_connect_is_an_error` (commit 41265fd).
    #[test]
    fn watch_before_connect_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");

        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        let sink = Arc::new(RecordingSink(std::sync::Mutex::new(tx))) as Arc<dyn SnapshotSink>;

        let err = client
            .watch("tasks".into(), sink)
            .expect_err("watch before connect should error");
        let msg = err.to_string();
        assert!(
            msg.contains("before connect"),
            "expected a before-connect error, got: {msg}"
        );
    }

    /// `watch()` with a table that doesn't match the session fixed at
    /// `connect()` time surfaces a clear error — the same one-table-per-client
    /// guard `write()`/`subscribe()` enforce. Mirrors `nostos_kotlin`'s
    /// `watch_table_mismatch_is_an_error` (commit 41265fd).
    #[test]
    fn watch_table_mismatch_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
        client.connect().expect("connect");

        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        let sink = Arc::new(RecordingSink(std::sync::Mutex::new(tx))) as Arc<dyn SnapshotSink>;

        let err = client
            .watch("not-tasks".into(), sink)
            .expect_err("mismatched-table watch should error");
        let msg = err.to_string();
        assert!(
            msg.contains("does not match"),
            "expected a table-mismatch error, got: {msg}"
        );
    }
}
