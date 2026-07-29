//! The `nostos_flutter` Rust glue — a thin frb-exposed wrapper around
//! `nostos_client::SyncClient<SqliteStorage>`.
//!
//! Design constraints (see docs/plans/flutter-supabase-plug-and-play-launch.md
//! W4 and docs/plans/w4-packaging-fallback.md for the packaging path this
//! builds on):
//!
//! - Rust owns SQLite. This crate never returns raw rows to Dart in a typed
//!   shape — it decodes the opaque payload bytes to JSON here and hands Dart a
//!   JSON array string per tick, which the Dart side `jsonDecode`s. No
//!   client-side schema artifact, no generated Dart model classes.
//! - One `SyncClient` binds N `tables` at construction, all multiplexed over
//!   ONE `/sync` WebSocket (D1/ADR-0022 multi-table-per-handle: one resume LSN,
//!   one checkpoint, one ack stream). `subscribe(tables)` represents ONE active
//!   subscription per [`NostosHandle`] — calling it again tears down the previous
//!   session and starts a fresh one. `watch(table)` attaches a per-table row
//!   stream to the active session; call it once per table you want to observe.
//! - This crate owns its own `tokio::runtime::Runtime` (not frb's internal
//!   executor) so the connect/apply/reconnect loop and the watch-stream pump
//!   keep running in the background for the lifetime of the [`NostosHandle`],
//!   independent of whatever executor frb used to service the `subscribe()`
//!   call itself.

use std::sync::Arc;
use std::time::Duration;

use std::collections::HashSet;

use nostos_client::sqlite::ClientTable;
use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig, TableSub};
use nostos_core::{PendingWrite, WriteOp};
use flutter_rust_bridge::frb;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex as AsyncMutex;

use crate::frb_generated::StreamSink;

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

/// Coarse connection-state signal for `Stream<NostosConnectionState>` on the
/// Dart side.
///
/// ponytail: `Connected` is a heuristic, not a precise signal from
/// `SyncClient`. `SyncClient::run_once` blocks for the entire session
/// (connect → subscribe → apply loop) and only returns on error or a clean
/// server-initiated close — there is no mid-call hook to observe "the WS
/// handshake + subscribe succeeded" without a further `nostos-client` change,
/// which this task deliberately avoided to keep the additive surface minimal
/// (see `SyncClient::subscribe_changes` / `with_storage`, which WERE worth
/// adding). Instead: if `run_once()` hasn't already failed within a short
/// grace window (`CONNECT_GRACE`), assume the handshake succeeded — a real
/// connection refusal / DNS failure surfaces near-instantly. A slow-but-doomed
/// connect can show a brief false `Connected` before flipping to
/// `Disconnected`; acceptable for a v1 UI-facing signal. Upgrade path: add a
/// `connected`/`subscribed` broadcast to `SyncClient` alongside `changes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NostosConnectionState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

/// FFI mirror of [`nostos_client::WriteQueueStatus`] — flutter_rust_bridge can
/// only generate Dart for types declared in this crate's `api` module, so the
/// engine type is re-declared here rather than re-exported.
#[derive(Debug, Clone)]
pub struct WriteQueueStatusFfi {
    /// Writes durably queued but not yet ack'd. `> 0` while offline is the
    /// offline-first promise working, not an error.
    pub pending: u64,
    /// Writes that permanently failed this session.
    pub dead_lettered: u64,
    /// Server error text from the most recent permanent failure. Set ONLY on a
    /// dead-letter — a plain rejection is usually transient and retries, so
    /// surfacing it would train users to ignore write errors.
    pub last_error: Option<String>,
}

impl From<nostos_client::WriteQueueStatus> for WriteQueueStatusFfi {
    fn from(s: nostos_client::WriteQueueStatus) -> Self {
        Self {
            pending: s.pending,
            dead_lettered: s.dead_lettered,
            last_error: s.last_error,
        }
    }
}

/// frb-friendly mirror of `nostos_client`'s `ClientTable` — the client-side
/// schema projection the WS2 view layer consumes. frb generates Dart bindings
/// for structs declared in THIS crate, so we mirror (rather than configuring
/// frb to reflect an external crate's type). The Dart side builds these from
/// the server's `GET /schema` `SchemaDescriptor` (drop per-column affinity down
/// to names) and hands them to [`NostosHandle::apply_schema`].
#[derive(Debug, Clone)]
pub struct ClientTableFfi {
    /// Canonical table id (matches `cairn_data.table_name` / the wire `table`).
    pub name: String,
    /// Primary-key column names (informational for the view; carried for the
    /// future materialized-table path).
    pub primary_key: Vec<String>,
    /// Column names in tuple order.
    pub columns: Vec<String>,
}

impl From<ClientTableFfi> for ClientTable {
    fn from(t: ClientTableFfi) -> Self {
        Self {
            name: t.name,
            primary_key: t.primary_key,
            columns: t.columns,
        }
    }
}

const CONNECT_GRACE: Duration = Duration::from_millis(250);

/// Session-level reconnect backstop for a long-lived `Nostos` subscription —
/// see the doc on the `idle_timeout` field set from this constant in
/// [`NostosHandle::subscribe`]. Deliberately much longer than
/// `SyncClientConfig::flush_quiesce` (the actual per-batch flush bound): this
/// is a rare, defense-in-depth reconnect, not the mechanism a real write
/// depends on for latency.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// A live Nostos connection. Owns the tokio runtime the background sync loop
/// and watch-stream pump run on, plus at most one active subscription.
#[frb(opaque)]
pub struct NostosHandle {
    rt: tokio::runtime::Runtime,
    url: String,
    token: Option<String>,
    db_path: String,
    session: AsyncMutex<Option<Session>>,
}

/// One subscription's table spec for [`NostosHandle::subscribe`]: a table name
/// plus an optional safe-SQL `where_sql` (ADR-0012). A connection subscribes to a
/// `Vec` of these over one `/sync` socket (D1/ADR-0022 multi-table-per-handle).
pub struct TableSubFfi {
    /// Table name to subscribe to.
    pub name: String,
    /// Optional safe-SQL predicate scoped to this table (ADR-0012).
    pub where_sql: Option<String>,
}

/// The one active subscription: ONE `SyncClient` bound to N tables over one
/// `/sync` socket, plus a per-table watch pump per attached `watch()` call.
/// Dropping this — including via `subscribe()` replacing it — aborts the
/// connect loop AND every watch pump, so a superseded subscription's tasks
/// actually stop instead of leaking a live WebSocket + pumps forever.
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    /// Every subscribed table name (drives the `write`/`watch` membership check).
    tables: HashSet<String>,
    /// The connect/apply/reconnect loop. `None` while paused via `disconnect()`
    /// (D2/ADR-0022): the client + storage + `watch_tasks` stay alive so reads,
    /// writes (durable outbox), and the UI keep working offline; `resume()`
    /// respawns it on the SAME client.
    run_task: Option<tokio::task::JoinHandle<()>>,
    /// Stashed at `subscribe()` so `resume()` can respawn the loop WITHOUT
    /// rebuilding the client or reopening storage (the spawn moves the original).
    config: SyncClientConfig,
    /// One re-snapshot pump per `watch(table)` call.
    watch_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = self.run_task.take() {
            task.abort();
        }
        for task in &self.watch_tasks {
            task.abort();
        }
    }
}

impl NostosHandle {
    /// Open a connection. Does not touch the network yet — `subscribe()`
    /// starts the actual WebSocket session. `db_path` is the SQLite file the
    /// durable client state lives in (Dart picks the directory, e.g. via
    /// `path_provider`'s `getApplicationSupportDirectory()` — see the
    /// `nostos_flutter` Dart wrapper).
    #[frb(sync)]
    #[must_use]
    pub fn connect(url: String, token: Option<String>, db_path: String) -> NostosHandle {
        let rt =
            tokio::runtime::Runtime::new().expect("nostos_flutter: failed to start tokio runtime");
        NostosHandle {
            rt,
            url,
            token,
            db_path,
            session: AsyncMutex::new(None),
        }
    }

    /// Materialize the WS2 read-views for `tables` in the on-device SQLite
    /// file (`CREATE VIEW IF NOT EXISTS <table> AS SELECT json_extract(...) AS
    /// col, ... FROM cairn_data WHERE table_name = '<table>'` — see
    /// `SqliteStorage::apply_schema`). Idempotent for an unchanged schema; the
    /// views persist in the SQLite file, so this is called ONCE after `connect`
    /// (and before the first `query()` / Dart `db.execute(SELECT ...)` that
    /// names a table).
    ///
    /// Opens a TRANSIENT storage connection at `db_path` (separate from the
    /// `subscribe` session's) purely to run the DDL, then drops it.
    /// ponytail: the transient double-open is a one-time setup cost (cheap);
    /// the upgrade path is to stash the schema on the handle and apply it
    /// inside `subscribe` on the session's own connection — deferred because
    /// this is not on any hot path.
    ///
    /// The Dart side owns the `GET /schema` fetch + `SchemaDescriptor` →
    /// `ClientTableFfi` mapping, keeping the Rust FFI crate HTTP-free
    /// (ADR-0015: no `reqwest` dep in `nostos_flutter_rust`).
    ///
    /// # Errors
    /// Returns an error string if the SQLite file can't be opened/migrated or
    /// any view DDL fails (`StorageError::Backend`).
    #[frb(sync)]
    pub fn apply_schema(&self, tables: Vec<ClientTableFfi>) -> Result<(), String> {
        let storage = SqliteStorage::open(&self.db_path).map_err(|e| e.to_string())?;
        let mapped: Vec<ClientTable> = tables.into_iter().map(ClientTable::from).collect();
        storage.apply_schema(&mapped).map_err(|e| e.to_string())
    }

    /// Subscribe to `tables` over ONE `/sync` WebSocket (D1/ADR-0022 multi-
    /// table-per-handle). The first entry is the primary; the rest are extra
    /// subscriptions on the same socket, all sharing one resume LSN, one
    /// checkpoint, and one ack stream (ADR-0009). Replaces any prior
    /// subscription on this handle. `state_sink` receives connection-state
    /// transitions for the life of the handle.
    ///
    /// Does NOT emit rows — call [`Self::watch`] per table to receive its row
    /// stream. (Snapshot pumps are attached separately so each table gets its
    /// own Dart stream, matching the `db.watch(table)` surface.)
    ///
    /// # Errors
    /// Returns an error string if `tables` is empty or opening the local
    /// SQLite store fails. Once subscribed, network/session errors surface
    /// only as `state_sink` transitions (reconnect is automatic and silent,
    /// matching `SyncClient::run_with_reconnect`'s contract).
    pub async fn subscribe(
        &self,
        tables: Vec<TableSubFfi>,
        state_sink: StreamSink<NostosConnectionState>,
    ) -> Result<(), String> {
        if tables.is_empty() {
            return Err("subscribe() requires at least one table".to_string());
        }
        let mut guard = self.session.lock().await;
        *guard = None; // drop (and stop) any prior subscription first

        let mut iter = tables.into_iter();
        let primary = iter.next().expect("checked non-empty");
        let extra: Vec<TableSub> = iter
            .map(|t| TableSub {
                name: t.name,
                where_sql: t.where_sql,
            })
            .collect();
        // The subscribed set (primary + extras) drives the write/watch checks.
        let mut table_set: HashSet<String> = HashSet::new();
        table_set.insert(primary.name.clone());
        for t in &extra {
            table_set.insert(t.name.clone());
        }

        let storage = SqliteStorage::open(&self.db_path).map_err(|e| e.to_string())?;
        let config = SyncClientConfig {
            table: primary.name,
            token: self.token.clone(),
            where_sql: primary.where_sql,
            extra_tables: extra,
            // Long-lived by design: no PER-BATCH idle disconnect, unbounded
            // retries. `flush_quiesce` (left at its default) closes a batch on
            // a short quiet gap WITHOUT tearing down the connection; this
            // `idle_timeout` is a much longer session-level backstop — paid for
            // by a periodic reconnect (re-handshake, re-subscribe from the
            // durable checkpoint, re-flush the outbox).
            idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
            ..SyncClientConfig::default()
        };
        let client = Arc::new(SyncClient::new(self.url.clone(), storage, config.clone()));

        // Connect/apply/reconnect loop, run on OUR runtime (not frb's), so it
        // outlives this async call and keeps going in the background.
        let run_client = Arc::clone(&client);
        let stashed_config = config.clone();
        let run_task = self
            .rt
            .spawn(async move { run_connection_loop(&run_client, &config, state_sink).await });

        *guard = Some(Session {
            client,
            tables: table_set,
            run_task: Some(run_task),
            config: stashed_config,
            watch_tasks: Vec::new(),
        });
        Ok(())
    }

    /// Attach a row stream for `table`: emits the current full row set
    /// immediately (the durable snapshot already on disk — visible offline)
    /// and again after every applied batch. One `watch` per table; `table`
    /// must be among those passed to [`Self::subscribe`]. Dropping the
    /// subscription (via `subscribe()` again or [`Self::close`]) aborts every
    /// watch pump.
    ///
    /// # Errors
    /// Returns an error string if `subscribe()` hasn't been called or `table`
    /// is not in the subscribed set.
    pub async fn watch(&self, table: String, rows_sink: StreamSink<String>) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| "watch() called before subscribe()".to_string())?;
        if !session.tables.contains(&table) {
            return Err(format!(
                "watch() table {table:?} is not in the subscribed set — add it to subscribe()"
            ));
        }

        // Subscribe to the change broadcast BEFORE emitting the initial
        // snapshot. The broadcast is no-replay (`broadcast::channel(64)` in
        // nostos-client), so a commit landing between emit_snapshot and
        // subscribe_changes would be permanently lost — the "connected but
        // lists render empty" regression (nostos-client/src/client.rs:1051,
        // invariant `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`
        // at client.rs:1097). Subscribing first closes that window; a commit
        // in the remaining gap just triggers a redundant re-snapshot from the
        // pump (idempotent — a full snapshot, self-healing on lag).
        let mut changes = session.client.subscribe_changes();

        // Immediate snapshot AFTER subscribing: durable rows from a prior
        // session must be visible offline, not only after the first commit of
        // a fresh one. Ordering relative to subscribe_changes is load-bearing
        // (see comment above).
        emit_snapshot(&session.client, &table, &rows_sink).await;

        // Re-snapshot on every applied batch. Each watch gets its own broadcast
        // receiver; a tick that didn't touch this table just re-queries
        // cheaply (a full snapshot, not a diff — self-healing on lag).
        let pump_client = Arc::clone(&session.client);
        let pump_table = table;
        let pump_sink = rows_sink;
        let pump_task = self.rt.spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(_) => {
                        emit_snapshot(&pump_client, &pump_table, &pump_sink).await;
                    }
                    Err(RecvError::Lagged(_)) => {
                        emit_snapshot(&pump_client, &pump_table, &pump_sink).await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        session.watch_tasks.push(pump_task);
        Ok(())
    }

    /// Stream durable-outbox status: how many writes are queued, how many have
    /// permanently failed, and the server's message for the last permanent
    /// failure.
    ///
    /// This is the write-side counterpart to `subscribe`'s connection-state
    /// sink. Without it a Dart app cannot tell its user that a write was lost:
    /// [`Self::write`] returns once the write is durable locally, and a server
    /// rejection afterwards was previously only a `tracing` warning inside the
    /// Rust client. Flutter's own optimistic-state guidance assumes a failed
    /// write surfaces so the UI can revert; this is the signal that makes that
    /// pattern expressible on Nostos.
    ///
    /// Emits the current value immediately on subscribe (the backing channel is
    /// a `watch`, not a broadcast), so a status widget built at any point in the
    /// app's life renders the true count rather than waiting for the next
    /// change.
    ///
    /// # Errors
    /// Returns an error string if `subscribe()` hasn't been called.
    pub async fn watch_write_status(
        &self,
        status_sink: StreamSink<WriteQueueStatusFfi>,
    ) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| "watch_write_status() called before subscribe()".to_string())?;

        let mut rx = session.client.subscribe_write_status();
        // Spawn BEFORE touching the sink, and emit the current value from
        // inside the pump task — not from here. Emitting while `guard` is held
        // keeps the session mutex locked across an FFI hop into Dart, and this
        // handle's every other entry point (`write`, `watch`, `disconnect`)
        // needs that same mutex, so the app stalls for as long as the Dart side
        // takes to accept the frame.
        let pump_task = self.rt.spawn(async move {
            // Current value first: writes queued in a PREVIOUS session are
            // already pending at construction, so a fresh subscriber must see
            // them without waiting for a change that may never come offline.
            if status_sink
                .add(WriteQueueStatusFfi::from(rx.borrow_and_update().clone()))
                .is_err()
            {
                return;
            }
            // `changed()` errors only when every sender is gone, i.e. the
            // client was dropped — end the pump rather than spin.
            while rx.changed().await.is_ok() {
                let next = rx.borrow_and_update().clone();
                if status_sink.add(WriteQueueStatusFfi::from(next)).is_err() {
                    break; // Dart side closed the stream.
                }
            }
        });
        session.watch_tasks.push(pump_task);
        drop(guard);
        Ok(())
    }

    /// Enqueue a durable write against the active subscription's table.
    /// Returns once the write is captured in the local outbox (NOT once the
    /// server acks it — that happens asynchronously; the row round-trips back
    /// through `subscribe`'s `rows_sink` once applied, same as any other
    /// replicated change, per `nostos-client`'s ADR-0013 outbox contract).
    ///
    /// `op` is `"upsert"` (insert-or-update), `"delete"`, or `"patch"`
    /// (column-level UPDATE of an existing row — `payload` carries only the
    /// columns to change; P3 PowerSync PATCH parity).
    ///
    /// # Errors
    /// Returns an error string if `subscribe()` hasn't been called yet, `op`
    /// is not one of `"upsert"` / `"delete"` / `"patch"`, `table` is not in
    /// the subscribed set (see [`Self::subscribe`]), or the local durable
    /// enqueue itself failed (disk full, SQLite busy).
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
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| "write() called before subscribe()".to_string())?;
        if !session.tables.contains(&table) {
            return Err(format!(
                "write() table {table:?} is not in the subscribed set — add it to subscribe() first"
            ));
        }
        let r = session
            .client
            .write(PendingWrite {
                table,
                op: write_op,
                pk,
                payload_json,
            })
            .await
            .map_err(|e: ClientError| e.to_string());
        r
    }

    /// Run an arbitrary `SELECT` against the on-device SQLite (the synced
    /// `cairn_data` table). Returns a JSON-array-of-objects STRING — one
    /// object per row, keyed by column name — which is the SAME shape the
    /// `rows` tick stream emits, so Dart decodes it identically with
    /// `jsonDecode`. The SQL typically uses
    /// `json_extract(payload, '$.col')` to project the opaque payload (JSON1
    /// ships in the bundled SQLite; ADR-0019) — e.g.
    /// `SELECT json_extract(payload, '$.title') AS title FROM cairn_data`.
    ///
    /// Requires an active subscription: the [`Session`] owns the
    /// [`SqliteStorage`] the client binds at `subscribe()` time, and
    /// `with_storage` reaches the concrete backend the same way `rows_for`
    /// does in `emit_snapshot` below (the closure param is `&SqliteStorage`,
    /// not `&Storage`, so `.query()` is callable in that position). Parity
    /// feature P1 — see `docs/plans/powersync-sdk-parity-plan.md`.
    ///
    /// This is a read-side accessor on the same `Mutex<Connection>` as the
    /// write path; it shares no mutation surface with the outbox (see the
    /// `Storage::query` doc in `nostos-core` for the trait-bound rationale).
    ///
    /// # Errors
    /// Returns an error string if `subscribe()` hasn't been called yet, the
    /// storage task panicked (`ClientError::Join`), or the SQL fails to
    /// prepare / a row fails to decode (`StorageError::Backend`).
    pub async fn query(&self, sql: String) -> Result<String, String> {
        let guard = self.session.lock().await;
        let session = guard.as_ref().ok_or_else(|| {
            "no active subscription — call subscribe() before query()".to_string()
        })?;
        // `with_storage` returns `Result<R, ClientError>` where `R` is whatever
        // the closure returns — here `s.query()` itself yields a
        // `Result<Vec<Map>, StorageError>`. Flatten both layers to a
        // `Vec<Map>` (which serializes) before `to_string`.
        let rows = session
            .client
            .with_storage(move |s| s.query(&sql))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    /// Pause syncing: abort ONLY the connect/apply/reconnect loop, keeping the
    /// `SyncClient`, its `SqliteStorage`, and every `watch()` pump alive. Reads,
    /// writes (which land in the durable outbox), and the UI keep working
    /// offline. `resume()` restarts it. Idempotent: a no-op when already paused
    /// or when there is no active subscription.
    ///
    /// Emits nothing on `state_sink` here (the aborted loop leaves it mid
    /// `connecting`/`reconnecting`); the Dart wrapper surfaces `disconnected`
    /// so the UI signal has one owner. Cancellation is task-abort: `run_once`
    /// respects no stop token, and `tokio::sync::Mutex` (no poison) + `Arc`
    /// client state mean the client stays usable for local work after the abort.
    pub async fn disconnect(&self) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            if let Some(task) = session.run_task.take() {
                task.abort();
                // Reap: the future drops on abort, so this resolves promptly
                // (an in-flight `spawn_blocking` finishes independently — it
                // never blocked this handle). A `JoinError` (Cancelled) is
                // expected and discarded.
                let _ = task.await;
            }
        }
        Ok(())
    }

    /// Resume syncing after [`disconnect`]: respawn the connect/apply/reconnect
    /// loop on the SAME `SyncClient` (reusable across aborts — `run_once(&self)`,
    /// all per-session state local, `tokio::sync::Mutex` carries no poison). The
    /// durable outbox drains on the new session's startup flush; live updates
    /// resume. `state_sink` receives the fresh run's transitions
    /// (`connecting → connected → …`). Requires a prior `subscribe()`.
    ///
    /// Named `resume`, not `connect`: the `#[frb(sync)]` constructor is already
    /// `NostosHandle::connect`, and Rust forbids two inherent items of the same
    /// name; the Dart public API mirrors the pause/resume pair (WS5) for the
    /// same reason — `connect` clashes with `Nostos.connect`/`NostosDatabase.connect`.
    pub async fn resume(&self, state_sink: StreamSink<NostosConnectionState>) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .ok_or_else(|| "resume() requires a prior subscribe()".to_string())?;
        // Abort any lingering loop first (idempotent — usually `None` after
        // disconnect).
        if let Some(old) = session.run_task.take() {
            old.abort();
            let _ = old.await;
        }
        let run_client = Arc::clone(&session.client);
        let run_config = session.config.clone();
        let run_task = self
            .rt
            .spawn(async move { run_connection_loop(&run_client, &run_config, state_sink).await });
        session.run_task = Some(run_task);
        Ok(())
    }

    /// Tear down the active subscription's background work — the
    /// connect/apply/reconnect loop and every watch pump (see [`Session`]'s
    /// `Drop` impl, which aborts all of them). Safe to call with no active
    /// subscription (a no-op) and safe to call more than once.
    ///
    /// Named `close`, not `dispose`: every `#[frb(opaque)]` handle already
    /// implements `RustOpaqueInterface`, which declares its own synchronous
    /// `void dispose()` for releasing the FFI handle itself — reusing that
    /// name here would collide with an unrelated method of a different
    /// signature (async vs sync). `close` mirrors `dart:io`'s
    /// `WebSocket.close()`, which this SDK's own session shape is closest to.
    ///
    /// This does NOT close the underlying SQLite file or shut down this
    /// handle's own tokio runtime — a subsequent `subscribe()` call on the
    /// SAME `NostosHandle` reopens a fresh session against the same durable
    /// store. The runtime itself is torn down when the handle is dropped
    /// (frb's generated `Drop` glue for an `#[frb(opaque)]` type, which is
    /// what `RustOpaqueInterface::dispose()` triggers), which happens once
    /// Dart releases its last reference — `close()` exists for a caller that
    /// wants to stop syncing sooner than that, e.g. a widget's `dispose()`
    /// lifecycle callback, rather than waiting on GC.
    pub async fn close(&self) {
        let mut guard = self.session.lock().await;
        *guard = None; // Drop aborts run_task + all watch pumps.
    }
}

/// Query the current full row set for `table` and push it as one JSON-array
/// string. Swallows storage/join errors (drops the tick) rather than closing
/// the stream — a transient read failure shouldn't kill `watch()`; the next
/// commit notification retries.
async fn emit_snapshot(client: &SyncClient<SqliteStorage>, table: &str, sink: &StreamSink<String>) {
    let table_owned = table.to_owned();
    let Ok(read) = client.with_storage(move |s| s.rows_for(&table_owned)).await else {
        return;
    };
    let Ok(rows) = read else {
        return;
    };
    let arr: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(pk, payload)| row_to_json_object(&pk, &payload))
        .collect();
    let _ = sink.add(serde_json::Value::Array(arr).to_string());
}

/// Decode a row's opaque payload bytes into a JSON object, stamping the
/// primary key onto it as `_pk` (the payload alone doesn't identify its own
/// row). Real Postgres-sourced rows are always a JSON object
/// (`PgReplicator::tuple_to_json_payload`); anything that fails to parse as
/// one — notably `nostos-server`'s zero-setup `NOSTOS_REPLICATOR=fake` default,
/// which emits deterministic filler bytes, not JSON — surfaces as `_raw` hex
/// instead of throwing, so `watch()` never breaks on a non-Postgres source.
fn row_to_json_object(pk: &str, payload: &[u8]) -> serde_json::Value {
    let mut obj = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => serde_json::json!({ "_raw": hex_encode(payload) }),
    };
    if let serde_json::Value::Object(map) = &mut obj {
        map.insert("_pk".to_owned(), serde_json::Value::String(pk.to_owned()));
    }
    obj
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The glue-owned connect/apply/reconnect loop (mirrors
/// `SyncClient::run_with_reconnect`'s backoff shape, reimplemented here rather
/// than calling it directly so we can emit [`NostosConnectionState`]
/// transitions between attempts — see the module + enum docs for why this
/// lives in the glue instead of as a further `nostos-client` change).
async fn run_connection_loop(
    client: &SyncClient<SqliteStorage>,
    config: &SyncClientConfig,
    state_sink: StreamSink<NostosConnectionState>,
) {
    let mut backoff = config.base_backoff;
    let mut attempt: u32 = 0;

    loop {
        let _ = state_sink.add(NostosConnectionState::Connecting);

        let run_fut = client.run_once();
        tokio::pin!(run_fut);
        let immediate = tokio::select! {
            res = &mut run_fut => Some(res),
            () = tokio::time::sleep(CONNECT_GRACE) => None,
        };
        let result = match immediate {
            Some(res) => res,
            None => {
                let _ = state_sink.add(NostosConnectionState::Connected);
                run_fut.await
            }
        };

        // Both a clean return (Ok — a server-initiated close, OR now our own
        // `idle_timeout` backstop firing, see `IDLE_RECONNECT_BACKSTOP`) and
        // an error mean "not connected anymore" for a long-lived client.
        // They are NOT equally serious, though: an `Ok` reconnect is healthy
        // housekeeping (nothing failed), so it must not escalate `backoff` —
        // only a genuine `Err` should climb toward `max_backoff`. Without
        // this reset, a long-running healthy session would eventually pay
        // `max_backoff` on every routine backstop reconnect, since `attempt`
        // never comes back down otherwise.
        let is_err = result.is_err();
        let _ = state_sink.add(NostosConnectionState::Disconnected);

        if is_err {
            attempt += 1;
        } else {
            attempt = 0;
            backoff = config.base_backoff;
        }
        if let Some(max) = config.max_retries {
            if attempt >= max {
                break;
            }
        }
        let _ = state_sink.add(NostosConnectionState::Reconnecting);
        tokio::time::sleep(backoff).await;
        if is_err {
            backoff = (backoff * 2).min(config.max_backoff);
        }
    }
}
