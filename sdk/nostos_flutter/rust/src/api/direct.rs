//! Direct mode's Flutter surface: the same device engine, with no Nostos server
//! on the other end of it.
//!
//! [`crate::api::nostos::NostosHandle`] dials a `/sync` WebSocket that somebody
//! has to operate. This one dials the customer's own Supabase project —
//! PostgREST for the pull and the push, Realtime for the doorbell, RLS for the
//! authorization — so an app can ship to end-user devices without shipping a
//! process to run beside it (ADR-0045,
//! `docs/plans/direct-mode-sync-protocol.md`).
//!
//! What is deliberately NOT here, because direct mode does not have it:
//!
//! - **`subscribe(tables)`** — there is no session to negotiate. What a device
//!   may read is decided by RLS in Postgres, from the JWT it presents.
//! - **`subscribe_stream`** — a stream is a server-held predicate template.
//! - **OR-sets** — add-wins merge is the server's job; direct mode's one
//!   serialized op is `cairn_increment`, exposed as
//!   [`NostosDirectHandle::increment`].
//!
//! The rest — storage, apply, watch, the durable outbox — is byte-for-byte the
//! server-mode engine, because it is the same `nostos-core`.

use std::sync::Arc;

use nostos_client::sqlite::ClientTable;
use nostos_client::{DirectClient, SqliteStorage};
use nostos_core::{Outbox, PendingWrite, WriteOp};
use flutter_rust_bridge::frb;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::api::nostos::{
    NostosConnectionState, NostosWriteInput, ClientTableFfi, WriteQueueStatusFfi,
};
use crate::frb_generated::StreamSink;

/// A device synced straight from Postgres. Owns the tokio runtime the pull loop
/// and the watch pumps run on.
#[frb(opaque)]
pub struct NostosDirectHandle {
    rt: tokio::runtime::Runtime,
    client: Arc<DirectClient<SqliteStorage>>,
    /// Fires after every sync that committed rows, and after every local
    /// enqueue. Each pump holds a receiver and re-reads its table on a tick — a
    /// full re-read, so a lagged pump heals itself instead of drifting.
    changes: broadcast::Sender<()>,
    /// The scope `start` was called with, so `resume` can respawn the same loop
    /// without the caller having to hold onto it.
    scope: std::sync::Mutex<Option<String>>,
    /// The sync loop. Separate from [`Self::pumps`] because `disconnect` stops
    /// syncing while `watch` streams and local writes keep working offline.
    run_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    pumps: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for NostosDirectHandle {
    fn drop(&mut self) {
        self.abort_run();
        self.abort_pumps();
    }
}

impl NostosDirectHandle {
    /// Open the device's database and point it at a Supabase project. Touches
    /// no network until [`Self::start`] or [`Self::sync_now`].
    ///
    /// `supabase_url` is the project URL (`https://<ref>.supabase.co`),
    /// `anon_key` its publishable key — the only credential that ships inside
    /// the app, and the reason direct mode needs RLS rather than trust.
    ///
    /// # Errors
    /// The SQLite file cannot be opened or migrated, or `supabase_url` is not a
    /// usable PostgREST base.
    #[frb(sync)]
    pub fn connect(
        supabase_url: String,
        anon_key: String,
        token: Option<String>,
        db_path: String,
    ) -> Result<NostosDirectHandle, String> {
        let rt =
            tokio::runtime::Runtime::new().expect("nostos_flutter: failed to start tokio runtime");
        let storage = SqliteStorage::open(&db_path).map_err(|e| e.to_string())?;
        let client =
            DirectClient::new(&supabase_url, &anon_key, storage).map_err(|e| e.to_string())?;
        if let Some(jwt) = token {
            rt.block_on(client.set_token(jwt));
        }
        let (changes, _) = broadcast::channel(64);
        Ok(NostosDirectHandle {
            rt,
            client: Arc::new(client),
            changes,
            scope: std::sync::Mutex::new(None),
            run_task: std::sync::Mutex::new(None),
            pumps: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Materialize the read-views for `tables` in the device's SQLite file, as
    /// [`crate::api::nostos::NostosHandle::apply_schema`] does. Idempotent.
    ///
    /// # Errors
    /// Any view DDL that fails.
    #[frb(sync)]
    pub fn apply_schema(&self, tables: Vec<ClientTableFfi>) -> Result<(), String> {
        let mapped: Vec<ClientTable> = tables.into_iter().map(ClientTable::from).collect();
        self.client
            .engine()
            .lock()
            .expect("direct: engine mutex poisoned")
            .storage()
            .apply_schema(&mapped)
            .map_err(|e| e.to_string())
    }

    /// Present `token` as the signed-in user. Direct mode has no session to
    /// renegotiate, so this takes effect on the next pull — call it whenever
    /// Supabase refreshes the JWT, and nothing the UI is holding tears down.
    pub async fn set_token(&self, token: Option<String>) {
        match token {
            Some(jwt) => self.client.set_token(jwt).await,
            None => self.client.clear_token().await,
        }
    }

    /// Start syncing: pull now, then on every doorbell ring and every
    /// reconnect, until [`Self::disconnect`] or [`Self::close`].
    ///
    /// `scope` is the value the change-log trigger stamps — `sub:<user-uuid>`
    /// for a user-scoped app, which is also the private Realtime channel the
    /// device is allowed to join.
    ///
    /// The state stream reports what the loop is doing: `connected` after a
    /// sync that reached the project, `reconnecting` after one that did not.
    /// There is no socket to be "connecting" on before the first pull, so the
    /// first value comes from the first sync rather than ahead of it.
    ///
    /// # Errors
    /// A loop is already running.
    pub async fn start(
        &self,
        scope: String,
        state_sink: StreamSink<NostosConnectionState>,
    ) -> Result<(), String> {
        *self.scope.lock().expect("direct: scope mutex poisoned") = Some(scope.clone());
        self.spawn_run(scope, state_sink)
    }

    /// Stop syncing without losing the device: the loop is aborted, `watch`
    /// streams and writes keep working offline. Pair with [`Self::resume`].
    /// Idempotent.
    pub async fn disconnect(&self) {
        self.abort_run();
    }

    /// Resume syncing after [`Self::disconnect`], on the scope [`Self::start`]
    /// was given. The outbox flushes on the first sync.
    ///
    /// # Errors
    /// [`Self::start`] was never called, or a loop is already running.
    pub async fn resume(&self, state_sink: StreamSink<NostosConnectionState>) -> Result<(), String> {
        let scope = self
            .scope
            .lock()
            .expect("direct: scope mutex poisoned")
            .clone()
            .ok_or("resume() before start(): no scope to resume")?;
        self.spawn_run(scope, state_sink)
    }

    /// Push the outbox and pull once, now. What [`Self::start`]'s loop does on
    /// a ring — for a pull-to-refresh, or a test.
    ///
    /// # Errors
    /// Whatever the round trip failed with.
    pub async fn sync_now(&self) -> Result<u64, String> {
        let outcome = self.client.sync().await.map_err(|e| e.to_string())?;
        if outcome.rows_applied > 0 || outcome.resnapshotted {
            let _ = self.changes.send(());
        }
        Ok(outcome.rows_applied as u64)
    }

    /// Stream `table`'s rows as a JSON array string, re-emitted on every change
    /// — local write or applied sync.
    ///
    /// Emits immediately from durable storage, before any network: rows from a
    /// previous run must render offline, not only after the first pull.
    ///
    /// # Errors
    /// Never, currently — the signature matches server mode's `watch` so the
    /// two Dart engines stay interchangeable.
    pub async fn watch(&self, table: String, rows_sink: StreamSink<String>) -> Result<(), String> {
        // Subscribe BEFORE the first read: the broadcast does not replay, so a
        // sync landing in between would otherwise stay invisible until the next
        // one (server mode's "connected but the list is empty" regression).
        let mut changes = self.changes.subscribe();
        emit_rows(&self.client, &table, &rows_sink);

        let client = Arc::clone(&self.client);
        self.track(self.rt.spawn(async move {
            // A lagged receiver is not an error here: every tick re-reads the
            // whole table, so one read catches up on any number of missed
            // ticks. Only a closed channel ends the pump.
            while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
                emit_rows(&client, &table, &rows_sink);
            }
        }));
        Ok(())
    }

    /// Stream durable-outbox status: queued writes, permanently-failed writes,
    /// and the message the server gave for the last permanent failure.
    ///
    /// Emits the current value immediately, because writes queued in a previous
    /// run are already pending at construction — a status widget built later in
    /// the app's life must render the true count rather than wait for a change
    /// that may never come offline.
    ///
    /// # Errors
    /// Never, currently — matches server mode's signature.
    pub async fn watch_write_status(
        &self,
        status_sink: StreamSink<WriteQueueStatusFfi>,
    ) -> Result<(), String> {
        let mut changes = self.changes.subscribe();
        emit_status(&self.client, &status_sink);

        let client = Arc::clone(&self.client);
        self.track(self.rt.spawn(async move {
            while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
                emit_status(&client, &status_sink);
            }
        }));
        Ok(())
    }

    /// Queue one write. Returns once it is durable on the device; the push to
    /// PostgREST runs behind it, so a write made offline is kept, not lost.
    ///
    /// # Errors
    /// An unknown `op`, or an outbox that would not commit.
    pub async fn write(
        &self,
        table: String,
        op: String,
        pk: String,
        payload_json: Option<String>,
    ) -> Result<u64, String> {
        let ids = self.enqueue(vec![NostosWriteInput {
            table,
            op,
            pk,
            payload_json,
        }])?;
        self.kick();
        Ok(ids.first().copied().unwrap_or_default())
    }

    /// Queue a batch atomically — all of it lands in one SQLite transaction or
    /// none of it does (ADR-0032 T3). Ids come back in the order given.
    ///
    /// # Errors
    /// As [`Self::write`]; one bad op rejects the whole batch before the outbox
    /// is touched.
    pub async fn write_batch(&self, ops: Vec<NostosWriteInput>) -> Result<Vec<u64>, String> {
        let ids = self.enqueue(ops)?;
        self.kick();
        Ok(ids)
    }

    /// Add `delta` to `field` through the generated `cairn_increment` — the one
    /// write Postgres serializes for us, so two devices incrementing the same
    /// row sum instead of clobbering (ADR-0030, direct-mode plan step 5).
    ///
    /// # Errors
    /// As [`Self::write`].
    pub async fn increment(
        &self,
        table: String,
        pk: String,
        field: String,
        delta: f64,
    ) -> Result<u64, String> {
        let payload = serde_json::json!({ "field": field, "delta": delta }).to_string();
        let ids = self.enqueue(vec![NostosWriteInput {
            table,
            op: "increment".to_owned(),
            pk,
            payload_json: Some(payload),
        }])?;
        self.kick();
        Ok(ids.first().copied().unwrap_or_default())
    }

    /// Run read-only SQL against the device's own database, returning a JSON
    /// array of row objects.
    ///
    /// # Errors
    /// The SQL failed to prepare, or a row failed to decode.
    pub async fn query(&self, sql: String) -> Result<String, String> {
        let engine = Arc::clone(self.client.engine());
        let rows = tokio::task::spawn_blocking(move || {
            engine
                .lock()
                .expect("direct: engine mutex poisoned")
                .storage()
                .query(&sql)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    /// Stop the loop and every pump. The database and its outbox stay on disk,
    /// so a later `connect` resumes from the same horizon.
    pub async fn close(&self) {
        self.abort_run();
        self.abort_pumps();
    }

    /// Sign out (ADR-0029): stop syncing, drop the token, and wipe local rows,
    /// the outbox and the horizon — everything the next principal must not see.
    ///
    /// The pumps stop FIRST so no watch re-reads the database halfway through
    /// the delete.
    ///
    /// # Errors
    /// The wipe failed. The token is dropped either way, so a failed sign-out
    /// cannot leave the device still pulling as the old user.
    pub async fn sign_out(&self) -> Result<(), String> {
        self.abort_run();
        self.abort_pumps();
        self.client.sign_out().await.map_err(|e| e.to_string())
    }

    /// Spawn the sync loop, refusing to run two of them.
    fn spawn_run(
        &self,
        scope: String,
        state_sink: StreamSink<NostosConnectionState>,
    ) -> Result<(), String> {
        let mut slot = self.run_task.lock().expect("direct: run mutex poisoned");
        // A second loop would double every pull and fight the first for the
        // cursor. One per handle, like `subscribe()` in server mode.
        if slot.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err("a sync loop is already running — disconnect() first".into());
        }

        let client = Arc::clone(&self.client);
        let changes = self.changes.clone();
        *slot = Some(self.rt.spawn(async move {
            let Err(fatal) = client
                .run(&scope, move |outcome| match outcome {
                    Ok(synced) => {
                        if synced.rows_applied > 0 || synced.resnapshotted {
                            let _ = changes.send(());
                        }
                        let _ = state_sink.add(NostosConnectionState::Connected);
                    }
                    Err(_) => {
                        // Not fatal: the next ring or reconnect retries it. What
                        // the UI needs to know is that the device is not
                        // current — which is what `reconnecting` means in
                        // server mode too.
                        let _ = state_sink.add(NostosConnectionState::Reconnecting);
                    }
                })
                .await;
            // `run` returns only when reconnecting cannot help: a malformed
            // token, a URL that is not a Realtime endpoint.
            eprintln!("nostos direct: sync loop stopped: {fatal}");
        }));
        Ok(())
    }

    /// Validate and enqueue, without touching the network.
    fn enqueue(&self, ops: Vec<NostosWriteInput>) -> Result<Vec<u64>, String> {
        let mut writes = Vec::with_capacity(ops.len());
        for op in ops {
            let write_op = match op.op.as_str() {
                "upsert" => WriteOp::Upsert,
                "patch" => WriteOp::Patch,
                "delete" => WriteOp::Delete,
                "increment" => WriteOp::Increment,
                other => {
                    return Err(format!(
                        "unknown write op {other:?}: expected \"upsert\", \"patch\", \
                         \"delete\" or \"increment\""
                    ))
                }
            };
            writes.push(PendingWrite {
                table: op.table,
                op: write_op,
                pk: op.pk,
                payload_json: op.payload_json,
            });
        }
        // Queue AND render: `write_batch` applies each write optimistically in
        // the same storage transaction, because `watch()` reads `cairn_data`
        // and never the outbox — enqueueing alone leaves the user's own write
        // invisible until the server echoes it, and invisible forever if the
        // server refuses it.
        let ids = self
            .client
            .write_batch(&writes)
            .map_err(|e| e.to_string())?;
        // The row is on the device now; render it before the network agrees.
        let _ = self.changes.send(());
        Ok(ids)
    }

    /// Push what was just queued, without making the caller wait for it.
    fn kick(&self) {
        let client = Arc::clone(&self.client);
        let changes = self.changes.clone();
        self.track(self.rt.spawn(async move {
            if let Ok(outcome) = client.sync().await {
                if outcome.rows_applied > 0 {
                    let _ = changes.send(());
                }
            }
        }));
    }

    /// Keep a task handle so `close` can stop it, dropping the ones that have
    /// already finished — otherwise every write leaks a handle for the life of
    /// the app.
    fn track(&self, task: JoinHandle<()>) {
        let mut pumps = self.pumps.lock().expect("direct: pump mutex poisoned");
        pumps.retain(|t| !t.is_finished());
        pumps.push(task);
    }

    fn abort_run(&self) {
        if let Some(task) = self
            .run_task
            .lock()
            .expect("direct: run mutex poisoned")
            .take()
        {
            task.abort();
        }
    }

    fn abort_pumps(&self) {
        let pumps = std::mem::take(&mut *self.pumps.lock().expect("direct: pump mutex poisoned"));
        for task in pumps {
            task.abort();
        }
    }
}

/// Read `table` out of durable storage and push it to Dart as one JSON array.
fn emit_rows(client: &DirectClient<SqliteStorage>, table: &str, sink: &StreamSink<String>) {
    let Ok(engine) = client.engine().lock() else {
        return;
    };
    let Ok(rows) = engine.storage().rows_for(table) else {
        return;
    };
    let arr: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(pk, payload)| {
            let mut obj = match serde_json::from_slice::<serde_json::Value>(&payload) {
                Ok(v @ serde_json::Value::Object(_)) => v,
                _ => serde_json::json!({}),
            };
            if let serde_json::Value::Object(map) = &mut obj {
                map.insert("_pk".to_owned(), serde_json::Value::String(pk));
            }
            obj
        })
        .collect();
    let _ = sink.add(serde_json::Value::Array(arr).to_string());
}

/// Count the outbox and push it to Dart.
fn emit_status(client: &DirectClient<SqliteStorage>, sink: &StreamSink<WriteQueueStatusFfi>) {
    let Ok(engine) = client.engine().lock() else {
        return;
    };
    let storage = engine.storage();
    let (Ok(pending), Ok(dead)) = (storage.pending(), storage.dead_letter_entries()) else {
        return;
    };
    // The reason lives in the outbox's own `last_error` column, which no trait
    // method exposes — `dead_letter_entries` returns the writes, not why they
    // failed. One SELECT beats widening the trait for a single string.
    let last_error = storage
        .query(
            "SELECT last_error FROM cairn_outbox \
             WHERE dlq = 1 AND last_error IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        )
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|r| r.get("last_error"))
                .and_then(|v| v.as_str().map(str::to_owned))
        });
    let _ = sink.add(WriteQueueStatusFfi {
        pending: pending.len() as u64,
        dead_lettered: dead.len() as u64,
        last_error,
    });
}
