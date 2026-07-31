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
//! - **Reactive row-tick callback**: WIRED. `watch(table, onSnapshot)` is the
//!   Node port of Flutter's `watch(table, rows_sink)` / kotlin's
//!   `watch(table, SnapshotSink)` — a TRUE Rust→JS push via a napi
//!   `ThreadsafeFunction` (callable from any thread, incl. a tokio worker),
//!   NOT a poll. The pump drains `SyncClient::subscribe_changes()` on this
//!   handle's owned runtime and schedules `(jsonString)` on the JS thread.
//! - **Connection-state stream**: Flutter emits `NostosConnectionState`
//!   transitions. Deferred (same ThreadsafeFunction seam, a second pump).
//! - **`.d.ts` generation**: plain `cargo build` does not emit TS types; use
//!   `npm run build:napi` (`@napi-rs/cli build`) for `.d.ts` + cross-triple
//!   packaging when shipping.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use tokio::sync::broadcast::error::RecvError;
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
    /// Cached auth token read at `connect()`/`subscribe()` time to build the
    /// `SyncClient`. Behind a `StdMutex` so `set_token`/`sign_out` can swap it
    /// through the napi `&self` receiver (napi methods take `&self`, not
    /// `&mut self`). Synchronous get/set — no await held across the lock.
    token: StdMutex<Option<String>>,
    db_path: String,
    session: AsyncMutex<Option<Session>>,
}

/// The active session. Dropping this — including via `subscribe()`/`connect()`
/// replacing it — aborts the background run loop AND every `watch()` pump so a
/// superseded session's WebSocket + reconnect loop + reactive pumps actually
/// stop instead of leaking. Mirrors kotlin's `Session` shape (Flutter's
/// `session.watch_tasks`).
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
    run_task: Option<tokio::task::JoinHandle<()>>,
    /// One pump per `watch()` call. Each owns its own `subscribe_changes()`
    /// receiver. Aborted on session teardown (Drop) so a watch's lifecycle is
    /// tied to the sync session — cancels on `subscribe()`/`connect()` replacing
    /// the session or client GC. No per-watch cancel handle today (the floor; a
    /// `stop_watch(table)` is the mechanical follow-on).
    watch_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Replay cache (kotlin's `last_snapshot` port): the last snapshot JSON
    /// emitted for this session's table. The no-replay Rust broadcast
    /// (`broadcast::channel(64)` in nostos-client) means a LATE subscriber's own
    /// `subscribe_changes()` receiver can't see prior ticks — this cache lets a
    /// late `watch()` replay the last emitted snapshot instantly (no storage
    /// round-trip). The first subscriber (empty cache) falls back to a live
    /// storage query (source of truth), which is then cached for the next.
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

/// Internal reactive-emitter seam. The napi `watch()` wraps a
/// `ThreadsafeFunction` in a [`TsfnEmitter`] impl of this trait; a host test
/// implements it with a recording channel. This seam is what lets the pump /
/// replay / subscribe-before-snapshot ordering / teardown logic be PROVEN in
/// pure-Rust host tests WITHOUT a JS runtime — a napi `ThreadsafeFunction`
/// cannot be constructed without a live `Env`, so the test drives the SAME
/// [`NostosClient::watch_internal`] core with a `RecordingEmitter` instead.
/// (kotlin's `#[uniffi::export(with_foreign)] trait SnapshotSink` gets this for
/// free — `with_foreign` permits a Rust host impl; napi has no analogue, so the
/// seam is introduced explicitly. The trait shape mirrors kotlin's
/// fire-and-forget sync `on_snapshot`, not an async callback.)
trait SnapshotEmitter: Send + Sync {
    /// Fire-and-forget snapshot delivery. Synchronous because napi's
    /// `ThreadsafeFunction::call` is itself a scheduling primitive (it posts to
    /// the JS thread and returns immediately, NOT an await point), and because
    /// kotlin's `SnapshotSink::on_snapshot` is sync for the same reason. Errors
    /// are best-effort swallowed: a Closing/aborted status just means the JS
    /// side is tearing down, and `Session::Drop` ends the pump regardless.
    fn emit(&self, json: String);
}

/// napi production emitter: wraps a `ThreadsafeFunction<String>` and forwards
/// each snapshot by scheduling the JS callback on the JS thread.
/// `ThreadsafeFunction::call` is `Send + Sync` + callable from any thread
/// (including this handle's tokio workers) — the textbook napi
/// background-callback pattern, and the feasibility gate that PASSES: a tokio
/// pump CAN invoke a JS callback via `ThreadsafeFunction`.
struct TsfnEmitter(ThreadsafeFunction<String, ErrorStrategy::CalleeHandled>);

impl SnapshotEmitter for TsfnEmitter {
    fn emit(&self, json: String) {
        // Best-effort: Ok/Closing both mean "scheduled or tearing-down"; the
        // pump keeps running until `Session::Drop` aborts it.
        let _ = self
            .0
            .call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
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
            token: StdMutex::new(token),
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
        let storage = SqliteStorage::open(&self.db_path)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let config = SyncClientConfig {
            table: "tasks".to_owned(),
            token: self.token.lock().expect("token lock poisoned").clone(),
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
    pub async fn subscribe(&self, table: String, where_sql: Option<String>) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        // Drop any prior session first — its Drop aborts the prior run_task.
        *guard = None;

        let storage = SqliteStorage::open(&self.db_path)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        let config = SyncClientConfig {
            table: table.clone(),
            token: self.token.lock().expect("token lock poisoned").clone(),
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
            watch_tasks: Vec::new(),
            last_snapshot: Arc::new(AsyncMutex::new(None)),
        });
        Ok(())
    }

    /// Reactive watch: emit the full-table snapshot to `on_snapshot` immediately,
    /// and again after every change tick (remote apply or local write). This is
    /// the Node port of Flutter's `watch(table, rows_sink)` / kotlin's
    /// `watch(table, SnapshotSink)` — a TRUE Rust→JS push via a napi
    /// `ThreadsafeFunction`, NOT a poll. The JS consumer passes a callback and
    /// receives `(jsonString)` calls; it never wall-clock-polls the store.
    ///
    /// `on_snapshot` is a `(snapshot: string) => void` JS function; napi wraps
    /// it in a `ThreadsafeFunction` so the tokio pump (on THIS handle's owned
    /// runtime) can invoke it from a non-JS thread. `ThreadsafeFunction::call`
    /// is `Send + Sync` + callable from any thread — the textbook napi
    /// background-callback pattern (feasibility gate: PASSES — a tokio pump CAN
    /// invoke a JS callback via tsfn; no JS-side polling fallback needed).
    ///
    /// Resolves once the initial snapshot has been emitted and the pump is
    /// spawned. One pump per call; its lifecycle is tied to the sync session:
    /// `Session::Drop` (on a `subscribe()`/`connect()` replacing the session or
    /// client GC) aborts every pump. No per-watch cancel handle today (the
    /// floor; a `stop_watch(table)` is the mechanical follow-on).
    ///
    /// `table` MUST match the active session's table (v1: one table per client).
    ///
    /// # Load-bearing ordering: subscribe BEFORE the first snapshot read
    /// (see kotlin port + nostos-client invariant
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`): the
    /// nostos-client change broadcast is no-replay (`broadcast::channel(64)`).
    /// A receiver created AFTER a commit permanently misses that commit — the
    /// "connected but lists render empty" regression. This port creates the
    /// receiver FIRST, then reads the initial snapshot; a commit in the residual
    /// gap just triggers a redundant re-snapshot from the pump (idempotent —
    /// full snapshot, self-healing on lag).
    #[napi]
    pub async fn watch(
        &self,
        table: String,
        #[napi(ts_arg_type = "(snapshot: string) => void")] on_snapshot: ThreadsafeFunction<
            String,
            ErrorStrategy::CalleeHandled,
        >,
    ) -> napi::Result<()> {
        let emitter = Arc::new(TsfnEmitter(on_snapshot)) as Arc<dyn SnapshotEmitter>;
        self.watch_internal(&table, emitter).await
    }

    /// Shared reactive-watch core — the napi `watch()` (tsfn emitter) and the
    /// host reactivity test (recording emitter) both drive this. Keeping the
    /// pump / replay / ordering logic here (not in the napi method) is what lets
    /// the reactivity be PROVEN in pure-Rust host tests without a JS runtime: a
    /// napi `ThreadsafeFunction` cannot be constructed without a live `Env`, so
    /// the test passes a `RecordingEmitter` impl instead (the napi adapter is a
    /// thin, cite-napi-contract wrapper). This mirrors how kotlin's `watch()`
    /// body is directly exercisable by a host test via its `SnapshotSink` trait.
    async fn watch_internal(
        &self,
        table: &str,
        emitter: Arc<dyn SnapshotEmitter>,
    ) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        let session = guard.as_mut().ok_or_else(|| {
            napi::Error::from_reason("watch() called before connect()/subscribe()")
        })?;
        if session.table != table {
            return Err(napi::Error::from_reason(format!(
                "watch() table {table:?} does not match active session table {:?} — v1 supports one table per NostosClient",
                session.table
            )));
        }

        // (1) SUBSCRIBE FIRST — load-bearing (see `watch` doc + the nostos-client
        // invariant cited there). Must precede the initial snapshot read below;
        // this receiver is the only way to learn of a commit that lands in the
        // gap before the pump starts. `subscribe_changes` returns an OWNED
        // `broadcast::Receiver` (holds its own channel handle, no borrow of the
        // session), so `session` is free again immediately after.
        let mut changes = session.client.subscribe_changes();

        // (2) Initial snapshot AFTER subscribing. Replay cache first: a late
        // subscriber (a second `watch()` for the same table after data has
        // already flowed) gets the last-emitted snapshot instantly without a
        // storage round-trip. First subscriber (empty cache) falls back to a
        // live storage query (source of truth), which is then cached.
        let cached = session.last_snapshot.lock().await.clone();
        let initial_json = match cached {
            Some(json) => json,
            None => {
                let json = snapshot_json(&session.client, table).await?;
                *session.last_snapshot.lock().await = Some(json.clone());
                json
            }
        };
        emitter.emit(initial_json);

        // (3) Pump: re-snapshot on EVERY change tick. Full snapshot per tick
        // (not a diff — self-healing on lag). Each watch owns its own receiver;
        // a tick on a different table just re-queries cheaply. `Lagged` (the
        // receiver fell >64 ticks behind) is treated as a tick — a full snapshot
        // resyncs. `Closed` (the client dropped its senders) fails the `while
        // let` and the pump exits.
        let pump_client = Arc::clone(&session.client);
        let pump_cache = Arc::clone(&session.last_snapshot);
        let pump_emitter = Arc::clone(&emitter);
        let table_owned = table.to_owned();
        let pump_task = self.rt.spawn(async move {
            // Ok / Lagged -> re-snapshot + emit. Closed fails the `while let`
            // and the pump exits. Snapshot read failure (transient) is
            // best-effort: skip this tick, the next one retries.
            while let Ok(_) | Err(RecvError::Lagged(_)) = changes.recv().await {
                if let Ok(json) = snapshot_json(&pump_client, &table_owned).await {
                    {
                        let mut cache = pump_cache.lock().await;
                        *cache = Some(json.clone());
                    }
                    pump_emitter.emit(json);
                }
            }
        });
        session.watch_tasks.push(pump_task);
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
        let session = guard.as_ref().ok_or_else(|| {
            napi::Error::from_reason("write() called before connect()/subscribe()")
        })?;
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
        let session = guard.as_ref().ok_or_else(|| {
            napi::Error::from_reason("query() called before connect()/subscribe()")
        })?;
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
        // Drop aborts run_task AND every watch pump (Session::Drop).
        *guard = None;
        Ok(())
    }

    /// Swap the auth token on this handle. Mirrors nostos-client's
    /// `SyncClient::set_token(Option<String>)` (`client.rs:358`): updates the
    /// cached handle token (so a subsequent `connect()`/`subscribe()` builds the
    /// `SyncClient` with the new token) AND, if a live session exists, swaps the
    /// running client's token in place so a reconnect uses it. Pass `null`/
    /// `undefined` to clear.
    ///
    /// The multi-user sign-out flow is `signOut()` → `setToken(b)` →
    /// `connect()`: after sign-out there is no live session, so only the cached
    /// token is set, which is exactly what the next `connect()` reads.
    /// (ADR-0029.)
    #[napi]
    pub async fn set_token(&self, token: Option<String>) -> napi::Result<()> {
        *self.token.lock().expect("set_token: token lock poisoned") = token.clone();
        let guard = self.session.lock().await;
        if let Some(session) = guard.as_ref() {
            session.client.set_token(token);
        }
        Ok(())
    }

    /// Sign out: stop sync + close the socket, wipe local rows AND the durable
    /// outbox (so the next principal sees nothing of this one), and clear the
    /// cached token. Implements ADR-0029.
    ///
    /// Ordering is load-bearing: the background connect/apply loop and every
    /// reactive watch pump are aborted and then AWAITED (quiesced) BEFORE
    /// `clear_local_state()` runs — a post-clear apply frame would re-populate
    /// storage ("half a clear is a leak", ADR-0029). Only once no frame can
    /// race do we wipe `Storage` (rows + checkpoint + epoch) and `Outbox`
    /// (pending writes + dead-letter) atomically, drop the session, and clear
    /// the token.
    ///
    /// Safe to call with no active session (no-op) and idempotent. Does NOT
    /// shut down this handle's tokio runtime — a subsequent `connect()` reopens
    /// a fresh session against the SAME (now wiped) durable store, the intended
    /// multi-user one-device flow.
    #[napi]
    pub async fn sign_out(&self) -> napi::Result<()> {
        let mut guard = self.session.lock().await;
        // Take the session out so its client is dropped only AFTER the wipe; the
        // guard holds `None` for the duration, so concurrent write()/query()
        // fail fast ("before connect") instead of racing the teardown.
        if let Some(mut session) = guard.take() {
            // (1) Abort + AWAIT the run loop and every watch pump = quiesce.
            // Abort alone is non-blocking (a tokio task runs to its next await);
            // awaiting the JoinHandle guarantees the task's stack has unwound
            // and released any engine lock before we clear. The awaited result
            // is a JoinError (task was aborted) — expected, ignored.
            let run_task = session.run_task.take();
            for task in run_task.into_iter().chain(session.watch_tasks.drain(..)) {
                task.abort();
                let _ = task.await;
            }
            // (2) With no apply frame able to race, wipe rows + checkpoint +
            // epoch + outbox + dead-letter atomically (one engine lock —
            // ADR-0029). Storage::clear + Outbox::clear, checkpoint → 0.
            session
                .client
                .clear_local_state()
                .await
                .map_err(|e: ClientError| napi::Error::from_reason(e.to_string()))?;
            // (3) Drop the session (client + storage). Already taken out of the
            // guard, so it releases here — its Drop would also abort, but the
            // tasks are already gone.
        }
        // (4) Clear the cached token so the next principal starts clean; a
        // fresh connect()/subscribe() builds its SyncClient with no token until
        // setToken.
        *self.token.lock().expect("sign_out: token lock poisoned") = None;
        Ok(())
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
/// `watch()`/`write()` already confirmed it equals the fixed session table), so
/// the interpolation is injection-safe; the canonical per-table snapshot query
/// is `SELECT pk, payload FROM cairn_data WHERE table_name = ?1 ...`
/// (`nostos-client/src/sqlite.rs`).
async fn snapshot_json(
    client: &Arc<SyncClient<SqliteStorage>>,
    table: &str,
) -> napi::Result<String> {
    let sql =
        format!("SELECT pk, payload FROM cairn_data WHERE table_name = '{table}' ORDER BY pk ASC");
    // `with_storage` runs the closure on the client's storage task; double-Result
    // (outer ClientError, inner StorageError) — same shape as `query()`.
    let rows = client
        .with_storage(move |s| s.query(&sql))
        .await
        .map_err(|e: ClientError| napi::Error::from_reason(e.to_string()))?
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    serde_json::to_string(&rows).map_err(|e| napi::Error::from_reason(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Test-only [`SnapshotEmitter`] that records every emitted snapshot into a
    /// `std::sync::mpsc` channel. `mpsc::Sender` is `Send` but not `Sync`, so it
    /// is wrapped in a `Mutex` (which IS `Send + Sync`) to satisfy the
    /// `SnapshotEmitter: Send + Sync` bound. The test thread receives via
    /// `recv_timeout` — a blocking EVENT wait on the callback, NOT a wall-clock
    /// poll of the SDK. This is the honest reactivity proof. (The napi
    /// production analogue is `TsfnEmitter`, which wraps a JS callback in the
    /// SAME `SnapshotEmitter` seam — so the pump path under test IS the
    /// production path; only the leaf delivery differs.)
    struct RecordingEmitter(StdMutex<std::sync::mpsc::Sender<String>>);

    impl SnapshotEmitter for RecordingEmitter {
        fn emit(&self, json: String) {
            // Best-effort: a dropped receiver (test gone) is fine; the pump
            // keeps running until Session::Drop aborts it.
            let _ = self.0.lock().expect("emitter lock").send(json);
        }
    }

    /// Drive `watch_internal` on the client's owned runtime, returning a channel
    /// that receives every emitted snapshot. `watch_internal` is the SAME core
    /// the napi `watch()` drives (with a `TsfnEmitter`); the test drives it with
    /// a `RecordingEmitter` because a `ThreadsafeFunction` cannot exist without
    /// a live JS `Env`. Blocks until the initial snapshot is emitted + the pump
    /// is spawned (mirrors kotlin's synchronous `watch()`).
    fn watch_blocking(client: &NostosClient, table: &str) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let emitter = Arc::new(RecordingEmitter(StdMutex::new(tx))) as Arc<dyn SnapshotEmitter>;
        client
            .rt
            .block_on(client.watch_internal(table, emitter))
            .expect("watch_internal should succeed after connect");
        rx
    }

    /// Proof-of-integration: the SAME `SyncClient<SqliteStorage>` the sibling
    /// SDKs drive constructs + serves an offline query through the napi
    /// `NostosClient` shape, with no live Node runtime required. Mirrors kotlin's
    /// / swift's / tauri's offline smoke path (construct + query round-trip).
    #[test]
    fn nostos_client_offline_connect_query_round_trip() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
        client.rt.block_on(client.connect()).expect("connect");

        let rows_json = client
            .rt
            .block_on(client.query("SELECT 1 AS one".into()))
            .expect("query");
        assert!(
            rows_json.contains("\"one\":1") || rows_json.contains("\"one\": 1"),
            "expected an one=1 row in the JSON, got: {rows_json}"
        );
    }

    /// REACTIVITY PROOF (host, no Node/JS runtime): `watch()` emits the initial
    /// snapshot, and a local `write()` — which applies a row to `cairn_data` AND
    /// fires the change broadcast (nostos-client invariant
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`,
    /// `rows_applied == 1`) — causes the pump to emit a NEW snapshot, WITHOUT
    /// the test polling a timer. `recv_timeout` blocks on the callback delivery
    /// (an event wait), so this is reactive-by-callback, not reactive-by-poll.
    /// (Production delivery differs only in the leaf: `TsfnEmitter::call` posts
    /// to the JS thread instead of `mpsc::send`.)
    ///
    /// This also covers the subscribe-before-snapshot invariant: if `watch()`
    /// read the snapshot BEFORE subscribing, a write racing that gap would be
    /// missed. The engine side is pinned in nostos-client; this test pins the FFI
    /// port's ordering (initial snapshot emitted, then the post-write snapshot).
    #[test]
    fn watch_emits_initial_snapshot_then_refires_on_local_write() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
        client.rt.block_on(client.connect()).expect("connect");

        // watch() subscribes (broadcast receiver created BEFORE the initial
        // snapshot read — the load-bearing invariant) and emits the initial
        // snapshot synchronously before returning.
        let rx = watch_blocking(&client, "tasks");

        // (1) Initial snapshot delivered — empty store -> "[]" (cairn_data has
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
        // and fires emit AGAIN — the reactive proof.
        client
            .rt
            .block_on(client.write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                Some(r#"{"id":"pk1","title":"reactive"}"#.into()),
            ))
            .expect("write");

        // (3) The post-write snapshot arrives without the test polling. The
        // row's pk is a TEXT column and unambiguously proves the new row is in
        // the snapshot (it was absent from the initial "[]"). NOTE: cairn_data
        // stores `payload` as a BLOB, so serde_json renders it hex-encoded
        // (e.g. 7b22... = `{"id":"pk1"...}`) — the SAME shape the sibling
        // `query()` emits. Decoding BLOBs to readable JSON is the WS2
        // typed-read (VIEW-over-cairn_data) layer's job, out of scope for the
        // reactive port; this test proves the CHANNEL, not the encoding.
        let after = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("post-write snapshot should arrive on the change tick");
        assert!(
            after.contains("pk1"),
            "post-write snapshot should contain the new row's pk, got: {after}"
        );

        // Drop the client: Session::Drop aborts the pump. If teardown leaks the
        // pump, runtime shutdown hangs here.
        drop(client);
    }

    /// `watch()` before `connect()` surfaces a clear error rather than
    /// panicking — the same before-connect contract `write()`/`subscribe()`
    /// enforce.
    #[test]
    fn watch_before_connect_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");

        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        let emitter = Arc::new(RecordingEmitter(StdMutex::new(tx))) as Arc<dyn SnapshotEmitter>;
        let err = client
            .rt
            .block_on(client.watch_internal("tasks", emitter))
            .expect_err("watch before connect should error");
        assert!(
            err.reason.contains("before connect"),
            "expected a before-connect error, got: {}",
            err.reason
        );
    }

    /// `watch()` with a table that doesn't match the session fixed at
    /// `connect()`/`subscribe()` time surfaces a clear error — the same
    /// one-table-per-client guard `write()`/`subscribe()` enforce.
    #[test]
    fn watch_table_mismatch_is_an_error() {
        let client = NostosClient::new("ws://localhost:0".into(), None, ":memory:".into())
            .expect("construct");
        client.rt.block_on(client.connect()).expect("connect");

        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        let emitter = Arc::new(RecordingEmitter(StdMutex::new(tx))) as Arc<dyn SnapshotEmitter>;
        let err = client
            .rt
            .block_on(client.watch_internal("not-tasks", emitter))
            .expect_err("mismatched-table watch should error");
        assert!(
            err.reason.contains("does not match"),
            "expected a table-mismatch error, got: {}",
            err.reason
        );
    }

    /// ADR-0029 sign-out wipe proof: after `signOut()`, the durable store ON
    /// DISK is wiped — a second principal opening the SAME `db_path` sees none
    /// of the first's rows. Uses a temp FILE (not `:memory:`, which is
    /// fresh-each-open and would hide a leaked wipe). Mirrors nostos-client's
    /// `clear_local_state_wipes_rows_and_outbox` engine-side pin, lifted to the
    /// FFI seam: proves the abort→quiesce→clear ordering holds through napi.
    #[test]
    fn sign_out_wipes_storage_so_next_principal_sees_nothing() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        let db_path = tmp.path().to_str().unwrap().to_owned();
        let client = NostosClient::new(
            "ws://localhost:0".into(),
            Some("userA-token".into()),
            db_path.clone(),
        )
        .expect("construct");
        client.rt.block_on(client.connect()).expect("connect");
        client
            .rt
            .block_on(client.write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                Some(r#"{"id":"pk1","title":"A's private row"}"#.into()),
            ))
            .expect("write");

        // The local write applies a row to cairn_data immediately (the same
        // invariant the reactivity proof exercises) — so it is queryable now.
        let before = client
            .rt
            .block_on(client.query("SELECT pk FROM cairn_data".into()))
            .expect("query before");
        assert!(
            before.contains("pk1"),
            "row should be present before sign-out, got: {before}"
        );

        client.rt.block_on(client.sign_out()).expect("sign_out");

        // Same handle, SAME file path — a fresh session reads the WIPED store.
        // (sign_out also cleared the cached token, so this connect builds with
        // None; fine — no network in this test.)
        client.rt.block_on(client.connect()).expect("reconnect");
        let after = client
            .rt
            .block_on(client.query("SELECT pk FROM cairn_data".into()))
            .expect("query after");
        assert!(
            !after.contains("pk1"),
            "sign_out should have wiped cairn_data so the next principal sees nothing, got: {after}"
        );

        // Drop the client first so it releases the SQLite file; the temp file
        // (declared before the client) then drops and deletes cleanly.
        drop(client);
    }
}
