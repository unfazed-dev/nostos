//! Appwrite Cloud direct transport over the shared Nostos SQLite engine.
//! ADR-0050: a revoked account must immediately invalidate every watcher.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flutter_rust_bridge::frb;
use nostos_client::appwrite::AppwriteDirectClient;
use nostos_client::sqlite::ClientTable;
use nostos_client::SqliteStorage;
use nostos_core::{Outbox, PendingWrite, WriteOp};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use crate::api::nostos::{
    ClientTableFfi, NostosConnectionState, NostosWriteInput, WriteQueueStatusFfi,
};
use crate::frb_generated::StreamSink;

#[frb(opaque)]
pub struct NostosAppwriteHandle {
    rt: tokio::runtime::Runtime,
    client: Arc<AppwriteDirectClient>,
    changes: broadcast::Sender<()>,
    wake: Arc<Notify>,
    run_task: Mutex<Option<JoinHandle<()>>>,
    pumps: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for NostosAppwriteHandle {
    fn drop(&mut self) {
        self.abort_run();
        self.abort_pumps();
    }
}

impl NostosAppwriteHandle {
    /// Open one provider-scoped SQLite file and bind a signed-in Appwrite user.
    /// No network call is made before `start` or `sync_now`.
    #[frb(sync)]
    pub fn connect(
        endpoint: String,
        project_id: String,
        function_id: String,
        user_id: String,
        jwt: String,
        db_path: String,
    ) -> Result<Self, String> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let storage = SqliteStorage::open(&db_path).map_err(|e| e.to_string())?;
        let client = AppwriteDirectClient::new(&endpoint, &project_id, &function_id, storage)
            .map_err(|e| e.to_string())?;
        rt.block_on(client.set_user(&user_id, &jwt))
            .map_err(|e| e.to_string())?;
        let (changes, _) = broadcast::channel(64);
        Ok(Self {
            rt,
            client: Arc::new(client),
            changes,
            wake: Arc::new(Notify::new()),
            run_task: Mutex::new(None),
            pumps: Mutex::new(Vec::new()),
        })
    }

    #[frb(sync)]
    pub fn apply_schema(&self, tables: Vec<ClientTableFfi>) -> Result<(), String> {
        let mapped: Vec<ClientTable> = tables.into_iter().map(ClientTable::from).collect();
        self.client
            .engine()
            .lock()
            .expect("appwrite: engine mutex poisoned")
            .storage()
            .apply_schema(&mapped)
            .map_err(|e| e.to_string())
    }

    /// Rotate a short-lived JWT; `None` pauses authenticated sync.
    pub async fn set_token(&self, token: Option<String>) {
        if let Some(jwt) = token {
            self.client.set_token(&jwt);
            self.wake.notify_one();
        } else {
            self.client.clear_token();
        }
    }

    /// Bind a different account, clearing the previous principal's rows and outbox.
    pub async fn set_user(&self, user_id: String, jwt: String) -> Result<(), String> {
        self.stop_run().await;
        self.client
            .set_user(&user_id, &jwt)
            .await
            .map_err(|e| e.to_string())?;
        let _ = self.changes.send(());
        Ok(())
    }

    pub async fn start(&self, state_sink: StreamSink<NostosConnectionState>) -> Result<(), String> {
        let mut slot = self.run_task.lock().expect("appwrite: run mutex poisoned");
        if slot.as_ref().is_some_and(|task| !task.is_finished()) {
            return Err("an Appwrite sync loop is already running".into());
        }
        let client = Arc::clone(&self.client);
        let changes = self.changes.clone();
        let wake = Arc::clone(&self.wake);
        *slot = Some(self.rt.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = interval.tick() => {},
                    _ = wake.notified() => {},
                }
                let bootstrap = client.needs_bootstrap();
                match client.sync().await {
                    Ok(outcome) => {
                        if bootstrap || outcome.resnapshotted || outcome.rows_applied > 0 || outcome.pushed > 0 {
                            let _ = changes.send(());
                        }
                        let _ = state_sink.add(NostosConnectionState::Connected);
                    }
                    Err(error) => {
                        if error.is_account_inactive() {
                            let _ = changes.send(());
                            let _ = state_sink.add(NostosConnectionState::AccessRevoked);
                            break;
                        }
                        eprintln!("nostos appwrite: sync failed: {error}");
                        let _ = state_sink.add(NostosConnectionState::Reconnecting);
                    }
                }
            }
        }));
        Ok(())
    }

    pub async fn disconnect(&self) {
        self.stop_run().await;
    }

    pub async fn resume(
        &self,
        state_sink: StreamSink<NostosConnectionState>,
    ) -> Result<(), String> {
        self.start(state_sink).await
    }

    pub async fn sync_now(&self) -> Result<u64, String> {
        let bootstrap = self.client.needs_bootstrap();
        let outcome = match self.client.sync().await {
            Ok(outcome) => outcome,
            Err(error) => {
                if error.is_account_inactive() {
                    let _ = self.changes.send(());
                }
                return Err(error.to_string());
            }
        };
        if bootstrap || outcome.resnapshotted || outcome.rows_applied > 0 || outcome.pushed > 0 {
            let _ = self.changes.send(());
        }
        Ok(outcome.rows_applied as u64)
    }

    pub async fn watch(&self, table: String, rows_sink: StreamSink<String>) -> Result<(), String> {
        let mut changes = self.changes.subscribe();
        if !self.client.needs_bootstrap() {
            emit_rows(&self.client, &table, &rows_sink);
        }
        let client = Arc::clone(&self.client);
        self.track(self.rt.spawn(async move {
            while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
                emit_rows(&client, &table, &rows_sink);
            }
        }));
        Ok(())
    }

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
        Ok(ids[0])
    }

    pub async fn write_batch(&self, ops: Vec<NostosWriteInput>) -> Result<Vec<u64>, String> {
        self.enqueue(ops)
    }

    pub async fn query(&self, sql: String) -> Result<String, String> {
        let rows = self
            .client
            .engine()
            .lock()
            .expect("appwrite: engine mutex poisoned")
            .storage()
            .query(&sql)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    pub async fn sign_out(&self) -> Result<(), String> {
        self.stop_run().await;
        self.abort_pumps();
        self.client.sign_out().await.map_err(|e| e.to_string())?;
        let _ = self.changes.send(());
        Ok(())
    }

    pub async fn close(&self) {
        self.stop_run().await;
        self.abort_pumps();
    }

    fn enqueue(&self, ops: Vec<NostosWriteInput>) -> Result<Vec<u64>, String> {
        let writes: Vec<PendingWrite> = ops
            .into_iter()
            .map(|op| {
                let kind = match op.op.as_str() {
                    "upsert" => WriteOp::Upsert,
                    "delete" => WriteOp::Delete,
                    other => return Err(format!("Appwrite does not support {other:?} writes")),
                };
                Ok(PendingWrite {
                    table: op.table,
                    op: kind,
                    pk: op.pk,
                    payload_json: op.payload_json,
                })
            })
            .collect::<Result<_, _>>()?;
        let ids = self
            .client
            .write_batch(&writes)
            .map_err(|e| e.to_string())?;
        let _ = self.changes.send(());
        self.wake.notify_one();
        Ok(ids)
    }

    fn track(&self, task: JoinHandle<()>) {
        let mut pumps = self.pumps.lock().expect("appwrite: pump mutex poisoned");
        pumps.retain(|task| !task.is_finished());
        pumps.push(task);
    }

    fn abort_run(&self) {
        if let Some(task) = self
            .run_task
            .lock()
            .expect("appwrite: run mutex poisoned")
            .take()
        {
            task.abort();
        }
    }

    async fn stop_run(&self) {
        let task = self
            .run_task
            .lock()
            .expect("appwrite: run mutex poisoned")
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    fn abort_pumps(&self) {
        for task in std::mem::take(&mut *self.pumps.lock().expect("appwrite: pump mutex poisoned"))
        {
            task.abort();
        }
    }
}

fn emit_rows(client: &AppwriteDirectClient, table: &str, sink: &StreamSink<String>) {
    let Ok(engine) = client.engine().lock() else {
        return;
    };
    let Ok(rows) = engine.storage().rows_for(table) else {
        return;
    };
    let arr: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(pk, payload)| {
            let mut obj = serde_json::from_slice::<serde_json::Value>(&payload)
                .unwrap_or_else(|_| serde_json::json!({}));
            if let serde_json::Value::Object(map) = &mut obj {
                map.insert("_pk".into(), serde_json::Value::String(pk));
            }
            obj
        })
        .collect();
    let _ = sink.add(serde_json::Value::Array(arr).to_string());
}

fn emit_status(client: &AppwriteDirectClient, sink: &StreamSink<WriteQueueStatusFfi>) {
    let Ok(engine) = client.engine().lock() else {
        return;
    };
    let storage = engine.storage();
    let (Ok(pending), Ok(dead)) = (storage.pending(), storage.dead_letter_entries()) else {
        return;
    };
    let last_error = storage
        .query(
            "SELECT last_error FROM nostos_outbox WHERE dlq = 1 AND last_error IS NOT NULL ORDER BY id DESC LIMIT 1",
        )
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|row| row.get("last_error"))
                .and_then(|value| value.as_str().map(str::to_owned))
        });
    let _ = sink.add(WriteQueueStatusFfi {
        pending: pending.len() as u64,
        dead_lettered: dead.len() as u64,
        last_error,
    });
}
