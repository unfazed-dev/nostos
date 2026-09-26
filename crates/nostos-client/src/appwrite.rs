//! Native Appwrite Function transport over the Nostos SQLite apply/outbox core.
//! ADR-0050 defines this project's journal, scope, and local wipe rules.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostos_core::{
    ApplyEngine, AppwriteCursor, AppwritePullError, Outbox, PendingWrite, Storage, StorageError,
    WriteOp,
};
use serde_json::{json, Value};

use crate::{direct::SyncOutcome, SqliteStorage};

const MAX_PAGES_PER_SYNC: usize = 64;
const PAGE_SIZE: usize = 100;

/// A transport, authorization, or local-apply failure.
#[derive(Debug, thiserror::Error)]
pub enum AppwriteSyncError {
    /// Invalid client configuration.
    #[error("invalid Appwrite configuration: {0}")]
    Configuration(String),
    /// No signed-in user was supplied.
    #[error("Appwrite session required")]
    Unsigned,
    /// A network call failed before receiving a response.
    #[error("Appwrite request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// Appwrite or the Function rejected a request.
    #[error("Appwrite HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Bounded response body, never a credential.
        body: String,
    },
    /// Function execution returned an invalid envelope.
    #[error("invalid Appwrite Function response: {0}")]
    Protocol(String),
    /// The write cannot be encoded for the Function.
    #[error("unsupported Appwrite write: {0}")]
    BadWrite(String),
    /// The pull page failed validation or local apply.
    #[error(transparent)]
    Pull(#[from] AppwritePullError),
    /// The local SQLite operation failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl AppwriteSyncError {
    /// The Function explicitly revoked this profile. Other 403 responses can
    /// reject one mutation without revoking the whole session.
    #[must_use]
    pub fn is_account_inactive(&self) -> bool {
        matches!(self, Self::Status { status: 403, body } if body == "account inactive")
    }
    /// Whether another attempt with the same outbox entry cannot succeed.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::BadWrite(_) | Self::Configuration(_) => true,
            Self::Status { status, .. } => !matches!(*status, 401 | 409 | 429 | 500..=599),
            Self::Unsigned
            | Self::Transport(_)
            | Self::Protocol(_)
            | Self::Pull(_)
            | Self::Storage(_) => false,
        }
    }
}

/// An Appwrite-authenticated device, using the same `ApplyEngine` and outbox
/// as the Postgres and Supabase transports.
pub struct AppwriteDirectClient {
    engine: Arc<Mutex<ApplyEngine<SqliteStorage>>>,
    cursor: Mutex<AppwriteCursor>,
    sync_lock: tokio::sync::Mutex<()>,
    http: reqwest::Client,
    endpoint: String,
    project_id: String,
    function_id: String,
    gateway_url: Option<String>,
    token: Mutex<Option<String>>,
    user_id: Mutex<Option<String>>,
    first_sync_done: std::sync::atomic::AtomicBool,
}

impl AppwriteDirectClient {
    /// Open a local store pointed at one Appwrite Cloud project and Function.
    ///
    /// No network request is made until [`Self::sync`]. Use a provider-specific
    /// SQLite file; a Supabase `horizon` has different meaning.
    ///
    /// # Errors
    /// Returns an invalid URL or local cursor error.
    pub fn new(
        endpoint: &str,
        project_id: &str,
        function_id: &str,
        storage: SqliteStorage,
    ) -> Result<Self, AppwriteSyncError> {
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://"))
            || project_id.is_empty()
            || function_id.is_empty()
        {
            return Err(AppwriteSyncError::Configuration(
                "endpoint, project ID and Function ID are required".into(),
            ));
        }
        let saved_horizon = storage.horizon()?;
        let cursor = match saved_horizon.as_ref() {
            Some(raw) => AppwriteCursor::resume(raw.parse().map_err(|_| {
                AppwriteSyncError::Configuration("local Appwrite cursor is not decimal".into())
            })?),
            None => AppwriteCursor::fresh(),
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            engine: Arc::new(Mutex::new(ApplyEngine::new(storage))),
            cursor: Mutex::new(cursor),
            sync_lock: tokio::sync::Mutex::new(()),
            http,
            endpoint: endpoint.trim_end_matches('/').into(),
            project_id: project_id.into(),
            function_id: function_id.into(),
            gateway_url: None,
            token: Mutex::new(None),
            user_id: Mutex::new(None),
            first_sync_done: std::sync::atomic::AtomicBool::new(saved_horizon.is_some()),
        })
    }

    /// Use an always-on `nostos-server` Appwrite gateway with the same cloud
    /// journal and Function authorization rules (ADR-0052).
    ///
    /// # Errors
    /// Returns an invalid URL or local cursor error.
    pub fn new_server(
        endpoint: &str,
        project_id: &str,
        function_id: &str,
        gateway_url: &str,
        storage: SqliteStorage,
    ) -> Result<Self, AppwriteSyncError> {
        let url = reqwest::Url::parse(gateway_url)
            .map_err(|error| AppwriteSyncError::Configuration(error.to_string()))?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            return Err(AppwriteSyncError::Configuration(
                "gateway URL must use HTTPS outside loopback".into(),
            ));
        }
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(AppwriteSyncError::Configuration(
                "gateway URL must not contain credentials, query or fragment".into(),
            ));
        }
        let mut client = Self::new(endpoint, project_id, function_id, storage)?;
        client.gateway_url = Some(gateway_url.trim_end_matches('/').into());
        Ok(client)
    }

    /// Local apply engine for watches, queries, and queued writes.
    #[must_use]
    pub fn engine(&self) -> &Arc<Mutex<ApplyEngine<SqliteStorage>>> {
        &self.engine
    }

    /// True until the first successful hosted pull completes.
    #[must_use]
    pub fn needs_bootstrap(&self) -> bool {
        !self
            .first_sync_done
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bind a real Appwrite user and JWT, wiping another user's local rows.
    ///
    /// The Function verifies the JWT. The explicit `user_id` only guards the
    /// local cache; it is not sent as an authority claim.
    ///
    /// # Errors
    /// Returns a local storage error if the principal check or wipe fails.
    pub async fn set_user(&self, user_id: &str, jwt: &str) -> Result<(), AppwriteSyncError> {
        if user_id.is_empty() || jwt.is_empty() {
            return Err(AppwriteSyncError::Unsigned);
        }
        let _guard = self.sync_lock.lock().await;
        let mut engine = self.engine.lock().expect("appwrite: engine mutex poisoned");
        let storage = engine.storage_mut();
        let stored_principal = storage.principal()?;
        let unscoped = self.local_principal(user_id, None);
        let scoped_admin = self.local_principal(user_id, Some("admin"));
        let scoped_customer = self.local_principal(user_id, Some("customer"));
        if !matches!(stored_principal.as_deref(), Some(raw) if raw == unscoped || raw == scoped_admin || raw == scoped_customer)
        {
            Storage::clear(storage)?;
            *self.cursor.lock().expect("appwrite: cursor mutex poisoned") = AppwriteCursor::fresh();
            self.first_sync_done
                .store(false, std::sync::atomic::Ordering::Relaxed);
            storage.save_principal(&unscoped)?;
        }
        *self.user_id.lock().expect("appwrite: user mutex poisoned") = Some(user_id.into());
        *self.token.lock().expect("appwrite: token mutex poisoned") = Some(jwt.into());
        Ok(())
    }

    fn local_principal(&self, user_id: &str, role: Option<&str>) -> String {
        if let Some(gateway_url) = &self.gateway_url {
            return serde_json::to_string(&json!([
                self.endpoint,
                self.project_id,
                self.function_id,
                gateway_url,
                user_id,
                role
            ]))
            .expect("Appwrite server principal contains only strings");
        }
        serde_json::to_string(&json!([
            self.endpoint,
            self.project_id,
            self.function_id,
            user_id,
            role
        ]))
        .expect("Appwrite principal contains only strings")
    }

    /// Compare the Function's current role with the cache scope before apply.
    /// Returns true when a different role forced a fresh pull from zero.
    fn reconcile_access_scope(
        &self,
        server_user: &str,
        access_scope: &str,
    ) -> Result<bool, AppwriteSyncError> {
        if !matches!(access_scope, "admin" | "customer") {
            return Err(AppwriteSyncError::Protocol("invalid access scope".into()));
        }
        let expected = self
            .user_id
            .lock()
            .expect("appwrite: user mutex poisoned")
            .clone();
        let mut engine = self.engine.lock().expect("appwrite: engine mutex poisoned");
        let storage = engine.storage_mut();
        if expected.as_deref() != Some(server_user) {
            Storage::clear(storage)?;
            return Err(AppwriteSyncError::Protocol(
                "JWT user differs from local user".into(),
            ));
        }
        let scoped = self.local_principal(server_user, Some(access_scope));
        let unscoped = self.local_principal(server_user, None);
        let stored = storage.principal()?;
        if stored.as_deref() == Some(&scoped) {
            return Ok(false);
        }
        if stored.as_deref() == Some(&unscoped) && storage.horizon()?.is_none() {
            // A new user may queue writes offline before the first cloud pull.
            // No cloud rows exist yet, so add the role without dropping them.
            storage.save_principal(&scoped)?;
            return Ok(false);
        }
        Storage::clear(storage)?;
        storage.save_principal(&scoped)?;
        *self.cursor.lock().expect("appwrite: cursor mutex poisoned") = AppwriteCursor::fresh();
        self.first_sync_done
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }

    /// Replace an expiring Appwrite JWT without closing the local database.
    pub fn set_token(&self, jwt: &str) {
        *self.token.lock().expect("appwrite: token mutex poisoned") = Some(jwt.into());
    }

    /// Stop authenticating new requests while keeping the offline cache intact.
    pub fn clear_token(&self) {
        *self.token.lock().expect("appwrite: token mutex poisoned") = None;
        *self.user_id.lock().expect("appwrite: user mutex poisoned") = None;
    }

    /// Forget credentials and clear rows, outbox, cursor, and principal.
    ///
    /// # Errors
    /// Returns a SQLite error if the wipe cannot commit.
    pub async fn sign_out(&self) -> Result<(), AppwriteSyncError> {
        let _guard = self.sync_lock.lock().await;
        self.clear_session_locked()?;
        Ok(())
    }

    fn clear_session_locked(&self) -> Result<(), AppwriteSyncError> {
        *self.token.lock().expect("appwrite: token mutex poisoned") = None;
        *self.user_id.lock().expect("appwrite: user mutex poisoned") = None;
        Storage::clear(
            self.engine
                .lock()
                .expect("appwrite: engine mutex poisoned")
                .storage_mut(),
        )?;
        *self.cursor.lock().expect("appwrite: cursor mutex poisoned") = AppwriteCursor::fresh();
        self.first_sync_done
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Optimistically render and durably queue one or more writes.
    ///
    /// # Errors
    /// Returns an outbox storage error.
    pub fn write_batch(&self, writes: &[PendingWrite]) -> Result<Vec<u64>, AppwriteSyncError> {
        let mut engine = self.engine.lock().expect("appwrite: engine mutex poisoned");
        if self
            .user_id
            .lock()
            .expect("appwrite: user mutex poisoned")
            .is_none()
        {
            return Err(AppwriteSyncError::Unsigned);
        }
        let ids = engine.storage_mut().enqueue_batch(writes.to_vec())?;
        for write in writes {
            if let Err(error) = engine.storage_mut().apply_local(write) {
                tracing::warn!(table = %write.table, pk = %write.pk, %error, "local Appwrite render failed; write stays queued");
            }
        }
        Ok(ids)
    }

    /// Push the outbox, then pull all bounded pages available now.
    ///
    /// # Errors
    /// Returns a transport, Function, protocol, or storage error. A transient
    /// push failure leaves the entry queued for the next sync.
    pub async fn sync(&self) -> Result<SyncOutcome, AppwriteSyncError> {
        let _guard = self.sync_lock.lock().await;
        let token = self
            .token
            .lock()
            .expect("appwrite: token mutex poisoned")
            .clone()
            .ok_or(AppwriteSyncError::Unsigned)?;
        let pushed = match self.push_pending(&token).await {
            Err(error) if error.is_account_inactive() => {
                self.clear_session_locked()?;
                return Err(error);
            }
            result => result?,
        };
        let mut rows_applied = 0;
        let mut resnapshotted = false;
        for _ in 0..MAX_PAGES_PER_SYNC {
            let after = self
                .cursor
                .lock()
                .expect("appwrite: cursor mutex poisoned")
                .after();
            let response = self
                .call(
                    &token,
                    "/sync/pull",
                    &json!({"after":after.to_string(),"limit":PAGE_SIZE}),
                )
                .await;
            let body = match response {
                Err(error) if error.is_account_inactive() => {
                    self.clear_session_locked()?;
                    return Err(error);
                }
                result => result?,
            };
            let server_user = body["principal_id"]
                .as_str()
                .ok_or_else(|| AppwriteSyncError::Protocol("pull omitted principal".into()))?;
            let access_scope = body["access_scope"]
                .as_str()
                .ok_or_else(|| AppwriteSyncError::Protocol("pull omitted access scope".into()))?;
            if self.reconcile_access_scope(server_user, access_scope)? {
                resnapshotted = true;
                continue;
            }
            let outcome = {
                let mut cursor = self.cursor.lock().expect("appwrite: cursor mutex poisoned");
                let mut engine = self.engine.lock().expect("appwrite: engine mutex poisoned");
                cursor.apply(&mut engine, &body.to_string())?
            };
            rows_applied += outcome.rows_applied;
            self.first_sync_done
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if !outcome.has_more {
                return Ok(SyncOutcome {
                    pushed,
                    rows_applied,
                    resnapshotted,
                });
            }
        }
        Ok(SyncOutcome {
            pushed,
            rows_applied,
            resnapshotted,
        })
    }

    async fn push_pending(&self, token: &str) -> Result<usize, AppwriteSyncError> {
        let pending = self
            .engine
            .lock()
            .expect("appwrite: engine mutex poisoned")
            .storage()
            .pending()?;
        let mut pushed = 0;
        for (id, write) in pending {
            let mutation_id = self
                .engine
                .lock()
                .expect("appwrite: engine mutex poisoned")
                .storage()
                .mutation_id(id)?;
            let body = match write_body(&mutation_id, &write) {
                Ok(body) => body,
                Err(error) => {
                    self.engine
                        .lock()
                        .expect("appwrite: engine mutex poisoned")
                        .storage()
                        .mark_dead_letter_with_error(id, Some(&error.to_string()))?;
                    continue;
                }
            };
            match self.call(token, "/sync/push", &body).await {
                Ok(_) => {
                    self.engine
                        .lock()
                        .expect("appwrite: engine mutex poisoned")
                        .storage_mut()
                        .mark_done(id)?;
                    pushed += 1;
                }
                Err(error) if error.is_account_inactive() => return Err(error),
                Err(error) if error.is_permanent() => {
                    self.engine
                        .lock()
                        .expect("appwrite: engine mutex poisoned")
                        .storage()
                        .mark_dead_letter_with_error(id, Some(&error.to_string()))?;
                }
                Err(error) => {
                    let _ = self
                        .engine
                        .lock()
                        .expect("appwrite: engine mutex poisoned")
                        .storage()
                        .bump_attempts(id);
                    return Err(error);
                }
            }
        }
        Ok(pushed)
    }

    async fn call(&self, jwt: &str, path: &str, body: &Value) -> Result<Value, AppwriteSyncError> {
        let request = if let Some(gateway_url) = &self.gateway_url {
            self.http
                .post(format!("{gateway_url}/appwrite{path}"))
                .bearer_auth(jwt)
                .json(body)
        } else {
            self.http
                .post(format!(
                    "{}/functions/{}/executions",
                    self.endpoint, self.function_id
                ))
                .header("X-Appwrite-Project", &self.project_id)
                .header("X-Appwrite-JWT", jwt)
                .json(&json!({"body":body.to_string(),"method":"POST","path":path}))
        };
        let response = request.send().await?;
        let status = response.status();
        let execution: Value = response.json().await?;
        if !status.is_success() {
            return Err(AppwriteSyncError::Status {
                status: status.as_u16(),
                body: bounded_message(&execution),
            });
        }
        let code = execution["responseStatusCode"]
            .as_u64()
            .ok_or_else(|| AppwriteSyncError::Protocol("missing Function status".into()))?;
        let body: Value =
            serde_json::from_str(execution["responseBody"].as_str().unwrap_or("null"))
                .map_err(|error| AppwriteSyncError::Protocol(error.to_string()))?;
        if !(200..300).contains(&code) {
            let status = u16::try_from(code)
                .map_err(|_| AppwriteSyncError::Protocol("invalid Function status".into()))?;
            return Err(AppwriteSyncError::Status {
                status,
                body: bounded_message(&body),
            });
        }
        Ok(body)
    }
}

fn write_body(mutation_id: &str, write: &PendingWrite) -> Result<Value, AppwriteSyncError> {
    let op = match write.op {
        WriteOp::Upsert => "upsert",
        WriteOp::Delete => "delete",
        other => {
            return Err(AppwriteSyncError::BadWrite(format!(
                "{} / {other:?}",
                write.table
            )))
        }
    };
    let payload = write
        .payload_json
        .as_ref()
        .map(|raw| serde_json::from_str::<Value>(raw))
        .transpose()
        .map_err(|error| AppwriteSyncError::BadWrite(error.to_string()))?;
    if op == "upsert" && !payload.as_ref().is_some_and(Value::is_object) {
        return Err(AppwriteSyncError::BadWrite("upsert needs an object".into()));
    }
    Ok(
        json!({"mutation_id":mutation_id,"table":write.table,"pk":write.pk,"op":op,"payload":payload}),
    )
}

fn bounded_message(value: &Value) -> String {
    value["message"]
        .as_str()
        .unwrap_or_else(|| value["error"].as_str().unwrap_or("unknown"))
        .chars()
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::{http::HeaderMap, routing::post, Json, Router};
    use nostos_core::{Outbox, PendingWrite, Storage, WriteOp};
    use serde_json::{json, Value};

    use super::{AppwriteDirectClient, AppwriteSyncError, SqliteStorage};

    #[tokio::test]
    async fn server_transport_uses_bearer_and_scopes_local_principal() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = upstream.local_addr().unwrap();
        let app = Router::new().route(
            "/appwrite/sync/pull",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer jwt-a");
                assert_eq!(body["after"], "0");
                Json(json!({
                    "responseStatusCode": 200,
                    "responseBody": "{\"head\":\"0\"}"
                }))
            }),
        );
        let task = tokio::spawn(async move { axum::serve(upstream, app).await.unwrap() });
        let client = AppwriteDirectClient::new_server(
            "https://example.invalid/v1",
            "project",
            "function",
            &format!("http://{addr}"),
            SqliteStorage::open_in_memory().unwrap(),
        )
        .unwrap();
        client.set_user("alice", "jwt-a").await.unwrap();
        assert_eq!(
            client
                .call("jwt-a", "/sync/pull", &json!({"after":"0"}))
                .await
                .unwrap()["head"],
            "0"
        );
        let direct = AppwriteDirectClient::new(
            "https://example.invalid/v1",
            "project",
            "function",
            SqliteStorage::open_in_memory().unwrap(),
        )
        .unwrap();
        assert_ne!(
            client.local_principal("alice", Some("customer")),
            direct.local_principal("alice", Some("customer"))
        );
        task.abort();
    }

    #[tokio::test]
    async fn switching_user_before_first_pull_discards_private_offline_writes() {
        let client = AppwriteDirectClient::new(
            "https://example.invalid/v1",
            "project",
            "function",
            SqliteStorage::open_in_memory().unwrap(),
        )
        .unwrap();
        client.set_user("alice", "jwt-a").await.unwrap();
        client
            .write_batch(&[PendingWrite {
                table: "sessions".into(),
                op: WriteOp::Upsert,
                pk: "private".into(),
                payload_json: Some(r#"{"title":"Alice"}"#.into()),
            }])
            .unwrap();
        client.set_user("bob", "jwt-b").await.unwrap();
        let engine = client.engine().lock().unwrap();
        assert!(engine.storage().rows_for("sessions").unwrap().is_empty());
        assert!(engine.storage().pending().unwrap().is_empty());
        assert_eq!(
            engine.storage().principal().unwrap().as_deref(),
            Some(client.local_principal("bob", None).as_str())
        );
    }

    #[test]
    fn saved_horizon_allows_immediate_offline_reads_after_reopen() {
        let mut storage = SqliteStorage::open_in_memory().unwrap();
        storage.save_horizon("42").unwrap();
        let client =
            AppwriteDirectClient::new("https://example.invalid/v1", "project", "function", storage)
                .unwrap();
        assert!(!client.needs_bootstrap());
    }

    #[tokio::test]
    async fn role_change_clears_cached_private_rows_and_outbox() {
        let client = AppwriteDirectClient::new(
            "https://example.invalid/v1",
            "project",
            "function",
            SqliteStorage::open_in_memory().unwrap(),
        )
        .unwrap();
        client.set_user("alice", "jwt-a").await.unwrap();
        client
            .write_batch(&[PendingWrite {
                table: "orders".into(),
                op: WriteOp::Upsert,
                pk: "other-customer-order".into(),
                payload_json: Some(r#"{"status":"paid"}"#.into()),
            }])
            .unwrap();
        assert!(!client.reconcile_access_scope("alice", "admin").unwrap());
        assert!(client.reconcile_access_scope("alice", "customer").unwrap());
        let engine = client.engine().lock().unwrap();
        assert!(engine.storage().rows_for("orders").unwrap().is_empty());
        assert!(engine.storage().pending().unwrap().is_empty());
        assert_eq!(
            engine.storage().principal().unwrap().as_deref(),
            Some(client.local_principal("alice", Some("customer")).as_str())
        );
    }

    #[tokio::test]
    async fn project_or_endpoint_change_wipes_previous_projects_private_rows() {
        let path = std::env::temp_dir().join(format!(
            "appwrite-principal-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let first = AppwriteDirectClient::new(
            "https://one.invalid/v1",
            "one",
            "function",
            SqliteStorage::open(path.to_str().unwrap()).unwrap(),
        )
        .unwrap();
        first.set_user("alice", "jwt-a").await.unwrap();
        first
            .write_batch(&[PendingWrite {
                table: "sessions".into(),
                op: WriteOp::Upsert,
                pk: "private".into(),
                payload_json: Some(r#"{"title":"Alice"}"#.into()),
            }])
            .unwrap();
        drop(first);
        // Reopen the same SQLite file with a different cloud identity.
        let second = AppwriteDirectClient::new(
            "https://one.invalid/v1",
            "two",
            "function",
            SqliteStorage::open(path.to_str().unwrap()).unwrap(),
        )
        .unwrap();
        second.set_user("alice", "jwt-b").await.unwrap();
        assert!(second
            .engine
            .lock()
            .unwrap()
            .storage()
            .pending()
            .unwrap()
            .is_empty());
        assert!(second
            .engine
            .lock()
            .unwrap()
            .storage()
            .rows_for("sessions")
            .unwrap()
            .is_empty());
        assert!(second.needs_bootstrap());
        second
            .write_batch(&[PendingWrite {
                table: "sessions".into(),
                op: WriteOp::Upsert,
                pk: "new-private".into(),
                payload_json: Some(r#"{"title":"New project"}"#.into()),
            }])
            .unwrap();
        drop(second);
        let third = AppwriteDirectClient::new(
            "https://two.invalid/v1",
            "two",
            "function",
            SqliteStorage::open(path.to_str().unwrap()).unwrap(),
        )
        .unwrap();
        third.set_user("alice", "jwt-c").await.unwrap();
        assert!(third
            .engine
            .lock()
            .unwrap()
            .storage()
            .pending()
            .unwrap()
            .is_empty());
        drop(third);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn revoked_session_wipes_cache_and_rejects_stale_writes() {
        let client = AppwriteDirectClient::new(
            "https://example.invalid/v1",
            "project",
            "function",
            SqliteStorage::open_in_memory().unwrap(),
        )
        .unwrap();
        client.set_user("alice", "jwt-a").await.unwrap();
        let write = PendingWrite {
            table: "sessions".into(),
            op: WriteOp::Upsert,
            pk: "private".into(),
            payload_json: Some(r#"{"title":"Alice"}"#.into()),
        };
        client.write_batch(std::slice::from_ref(&write)).unwrap();
        let rejected = AppwriteSyncError::Status {
            status: 403,
            body: "account inactive".into(),
        };
        assert!(rejected.is_account_inactive());
        client.clear_session_locked().unwrap();
        assert!(client
            .engine
            .lock()
            .unwrap()
            .storage()
            .pending()
            .unwrap()
            .is_empty());
        assert!(client
            .engine
            .lock()
            .unwrap()
            .storage()
            .principal()
            .unwrap()
            .is_none());
        assert!(matches!(
            client.write_batch(&[write]),
            Err(AppwriteSyncError::Unsigned)
        ));
        assert!(!AppwriteSyncError::Status {
            status: 403,
            body: "forbidden".into()
        }
        .is_account_inactive());
    }
}
