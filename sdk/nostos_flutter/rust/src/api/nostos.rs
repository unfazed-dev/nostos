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
//! - One `SyncClient` binds one `table` at construction (`nostos-client`'s
//!   Phase-0 predicate floor). `subscribe()` therefore represents ONE active
//!   subscription per [`NostosHandle`] — calling it again tears down the
//!   previous session and starts a fresh one. Independent concurrent
//!   subscriptions to multiple tables are a ponytail for a future
//!   `nostos-client` that supports multi-table sessions.
//! - This crate owns its own `tokio::runtime::Runtime` (not frb's internal
//!   executor) so the connect/apply/reconnect loop and the watch-stream pump
//!   keep running in the background for the lifetime of the [`NostosHandle`],
//!   independent of whatever executor frb used to service the `subscribe()`
//!   call itself.

use std::sync::Arc;
use std::time::Duration;

use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig};
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

/// The one active subscription (v1: single-table, see module docs). Dropping
/// this — including via `subscribe()` replacing it — aborts both background
/// tasks, so a superseded subscription's connect loop and watch pump actually
/// stop instead of leaking a live WebSocket + reconnect loop forever.
struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    table: String,
    run_task: tokio::task::JoinHandle<()>,
    pump_task: tokio::task::JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.run_task.abort();
        self.pump_task.abort();
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
        let rt = tokio::runtime::Runtime::new().expect("nostos_flutter: failed to start tokio runtime");
        NostosHandle {
            rt,
            url,
            token,
            db_path,
            session: AsyncMutex::new(None),
        }
    }

    /// Subscribe to `table` (optionally filtered by `where_sql`, the safe-SQL
    /// subset ADR-0012 documents). Replaces any prior subscription on this
    /// handle. `rows_sink` receives one JSON-array string per tick — the
    /// current full row set for `table`, emitted immediately (the durable
    /// snapshot already on disk, so an offline watcher sees data right away)
    /// and again after every applied batch. `state_sink` receives connection
    /// state transitions for the life of the handle (not just this
    /// subscription — see [`NostosConnectionState`]).
    ///
    /// # Errors
    /// Returns an error string if opening the local SQLite store fails. Once
    /// subscribed, network/session errors surface only as `state_sink`
    /// transitions (reconnect is automatic and silent, matching
    /// `SyncClient::run_with_reconnect`'s contract) — `write()` is what
    /// surfaces a durable-outbox failure to the caller.
    pub async fn subscribe(
        &self,
        table: String,
        where_sql: Option<String>,
        rows_sink: StreamSink<String>,
        state_sink: StreamSink<NostosConnectionState>,
    ) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        *guard = None; // drop (and stop) any prior subscription first

        let storage = SqliteStorage::open(&self.db_path).map_err(|e| e.to_string())?;
        let config = SyncClientConfig {
            table: table.clone(),
            token: self.token.clone(),
            where_sql,
            // Long-lived by design: no PER-BATCH idle disconnect, unbounded
            // retries. The bug this `None` used to paper over (a single
            // write on an otherwise-idle table buffering forever) is now
            // fixed precisely by `flush_quiesce` (left at its default —
            // `SyncClientConfig::default()` below), which closes a batch on
            // a short quiet gap WITHOUT tearing down the connection. This
            // `idle_timeout` stays a much longer, session-level backstop:
            // defense-in-depth in case `flush_quiesce` ever misses a case,
            // paid for by a periodic reconnect (cheap: re-handshake,
            // re-subscribe from the durable checkpoint, re-flush the
            // outbox) rather than by disconnecting on every ordinary quiet
            // period the way a short value would.
            idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
            ..SyncClientConfig::default()
        };
        let client = Arc::new(SyncClient::new(self.url.clone(), storage, config.clone()));

        // Immediate snapshot BEFORE any network activity: durable rows from a
        // prior session must be visible offline, not only after the first
        // commit of a fresh one.
        emit_snapshot(&client, &table, &rows_sink).await;

        // Watch pump: re-snapshot on every applied batch.
        let mut changes = client.subscribe_changes();
        let pump_client = Arc::clone(&client);
        let pump_table = table.clone();
        let pump_sink = rows_sink.clone();
        let pump_task = self.rt.spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(_) => emit_snapshot(&pump_client, &pump_table, &pump_sink).await,
                    // A slow Dart-side consumer lagged behind the broadcast
                    // buffer; the next snapshot is a full re-query (not a
                    // diff), so a missed tick is self-healing — just re-emit.
                    Err(RecvError::Lagged(_)) => {
                        emit_snapshot(&pump_client, &pump_table, &pump_sink).await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });

        // Connect/apply/reconnect loop, run on OUR runtime (not frb's), so it
        // outlives this async call and keeps going in the background.
        let run_client = Arc::clone(&client);
        let run_task = self
            .rt
            .spawn(async move { run_connection_loop(&run_client, &config, state_sink).await });

        *guard = Some(Session {
            client,
            table,
            run_task,
            pump_task,
        });
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
    /// is not one of `"upsert"` / `"delete"` / `"patch"`, `table` doesn't
    /// match the active subscription (v1 is one table per handle — see module
    /// docs), or the local durable enqueue itself failed (disk full, SQLite
    /// busy).
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
        if session.table != table {
            return Err(format!(
                "write() table {table:?} does not match the active subscription \
                 ({:?}) — v1 supports one table per Nostos instance; call \
                 subscribe({table:?}, ...) first",
                session.table
            ));
        }
        session
            .client
            .write(PendingWrite {
                table,
                op: write_op,
                pk,
                payload_json,
            })
            .await
            .map_err(|e: ClientError| e.to_string())
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
        let session = guard
            .as_ref()
            .ok_or_else(|| "no active subscription — call subscribe() before query()".to_string())?;
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

    /// Tear down the active subscription's background work — the
    /// connect/apply/reconnect loop and the watch-stream pump (see
    /// [`Session`]'s `Drop` impl, which aborts both tasks). Safe to call with
    /// no active subscription (a no-op) and safe to call more than once.
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
        *guard = None; // Drop aborts run_task + pump_task.
    }
}

/// Query the current full row set for `table` and push it as one JSON-array
/// string. Swallows storage/join errors (drops the tick) rather than closing
/// the stream — a transient read failure shouldn't kill `watch()`; the next
/// commit notification retries.
async fn emit_snapshot(client: &SyncClient<SqliteStorage>, table: &str, sink: &StreamSink<String>) {
    let table_owned = table.to_owned();
    let Ok(read) = client
        .with_storage(move |s| s.rows_for(&table_owned))
        .await
    else {
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
