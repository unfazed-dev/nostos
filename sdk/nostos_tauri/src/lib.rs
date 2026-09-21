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
//! The ten `#[tauri::command]` handlers (`connect` / `subscribe` / `write` /
//! `query` / `checkpoint` / `watch` / `set_token` / `sign_out` /
//! `register_push_token` / `deregister_push_token`) are thin wrappers over `impl NostosState` async methods, which
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
//! - **Reactive watch**: IMPLEMENTED — the `watch` command subscribes to
//!   `SyncClient::subscribe_changes()` and pushes a fresh full-table snapshot
//!   to the JS frontend over a `tauri::ipc::Channel<NostosSnapshot>` on every
//!   change tick (remote apply OR local write) — the Tauri port of node's
//!   `watch()` / Flutter's `rows_sink` (ADR-0024), NOT a poll. Floors: no
//!   per-watch cancel handle (the pump self-terminates when JS drops the
//!   channel — the unsubscribe path — and on session teardown).
//! - **Multi-table (2026-09-21)**: `plugins.cairn.tables[]` is the session's
//!   table set — the first entry is `SyncClient`'s primary table, the rest ride
//!   `extra_tables` (ADR-0022, one socket, one checkpoint, server cap 32).
//!   Every table-taking command checks membership in that set. `subscribe`'s
//!   `table` arg is API parity only: the run loop is per session, not per
//!   table. Per-table `where_sql` comes from `plugins.cairn.whereSql`
//!   (`{table: predicate}`, ADR-0012 safe-SQL). There is no per-table
//!   `resume_lsn` by design: the LSN is stream-global — one socket, one
//!   checkpoint per session (ADR-0022).
//! - **Permissions**: only the six default command permissions are listed in
//!   `permissions/default.toml`. A shipped plugin would also publish scoped
//!   permission sets per table.
//! - **JS bindings / `.d.ts`**: SHIPPED — `guest-js/` carries a typed
//!   ESM `@nostos-sync/tauri` package (no build step: plain `.js` + hand-written
//!   `.d.ts`). Ceiling: ESM-only (no CJS/iife bundle — Tauri frontends are
//!   ESM-native); run `tauri-plugin`'s scaffolder if a `withGlobalTauri`
//!   bundle is ever needed.

#![forbid(unsafe_code)]
// The Tauri plugin shape uses a generic `R: Runtime` init + `State<'_, T>`
// commands. Clippy pedantic noise about "missing errors_doc" / "missing_panics_doc"
// on the `#[tauri::command]` async fns and the generic init is not load-bearing
// for a scaffold; keep the surface readable instead.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nostos_client::{ClientError, SqliteStorage, SyncClient, SyncClientConfig, TableSub};
use nostos_core::{PendingWrite, WriteOp};
use nostos_domain::Lsn;
use serde::Deserialize;
use tauri::plugin::{Builder, TauriPlugin};
use tauri::{Manager, Runtime, State};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex as AsyncMutex;

/// std Mutex for fields locked across `.await`-free critical sections only
/// (the token cache / push-token registry / session URL) — mirroring
/// `sdk/nostos_node`'s handle fields. The session itself stays an AsyncMutex
/// because its methods hold the guard across awaits.
use std::sync::Mutex as StdMutex;

/// Session-level reconnect backstop — mirrors `sdk/nostos_node`'s
/// `IDLE_RECONNECT_BACKSTOP` and the Flutter glue's constant of the same name.
/// Long relative to per-batch flush bounds: this is a rare defense-in-depth
/// reconnect, not a per-write latency mechanism.
const IDLE_RECONNECT_BACKSTOP: Duration = Duration::from_secs(120);

/// The `plugins.cairn` block of `tauri.conf.json` (A2 config story). Every
/// field is optional: an absent block deserializes to all-`None` (Tauri hands
/// the plugin `{}` when `plugins.cairn` is missing — verified against tauri
/// 2.11.5 `plugin.rs` `initialize`: `.get(name).cloned().unwrap_or_default()`),
/// and `deny_unknown_fields` turns a typo'd key into a loud startup error
/// ("Error deserializing 'plugins.cairn' within your Tauri configuration").
///
/// These are DEFAULTS for `connect()`, not a second way to open a session:
/// `connect`'s explicit args win, then these, then the hard-coded floor
/// (table `"tasks"`, db path `"cairn.db"`). One config, one precedence rule —
/// the same "config is the floor, args are the override" shape the official
/// plugins use.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NostosPluginConfig {
    /// Default sync endpoint (`ws://…/sync` / `wss://…/sync`) used when
    /// `connect` is called without an explicit `url`.
    pub sync_url: Option<String>,
    /// Default bearer JWT used when `connect` is called without an explicit
    /// `token` — also the credential push-token registration sends.
    pub token: Option<String>,
    /// Every table this session syncs. The first entry is the primary
    /// `SyncClient` table, the rest are `extra_tables` (ADR-0022 — one socket,
    /// one resume LSN, server cap 32). Supersedes `table`; when non-empty,
    /// `table` is ignored. Defaults to `[table]` → `["tasks"]`.
    pub tables: Option<Vec<String>>,
    /// Single-table shorthand (the v1 shape). Kept so existing
    /// `tauri.conf.json` blocks keep working; prefer `tables`.
    pub table: Option<String>,
    /// Optional per-table safe-SQL predicates (ADR-0012), keyed by table
    /// name: `"whereSql": {"tasks": "owner_id = 'u1'"}`. Every key MUST be in
    /// the session table set — `connect` refuses otherwise. Tables without an
    /// entry sync unfiltered. The server parses the predicate; a bad one
    /// closes the socket with `invalid where_sql:` (ADR-0012), it never widens
    /// scope past the tenant.
    pub where_sql: Option<HashMap<String, String>>,
    /// Default on-device SQLite path. Relative paths open relative to the
    /// process working directory — desktop apps should pass an absolute path
    /// (e.g. from `app.path().app_data_dir()`) either here or per `connect`.
    /// Defaults to `"cairn.db"`.
    pub db_path: Option<String>,
    /// Tables this session treats as add-wins OR-sets (ADR-0030) — the
    /// client-side gate for the `orSetAdd`/`orSetRemove` commands. MUST
    /// triple-match the storage tags and the server's `NOSTOS_OR_SET_COLUMNS`
    /// (the sibling SDKs' three-views-of-one-truth rule).
    pub or_set_tables: Option<Vec<String>>,
    /// Tables this session treats as PN-Counters (ADR-0030 addendum) — the
    /// gate for `counterIncrement`/`counterDecrement`. Same triple-match
    /// rule against the server's `NOSTOS_COUNTER_COLUMNS`.
    pub counter_tables: Option<Vec<String>>,
}

impl NostosPluginConfig {
    /// The resolved session table set: `tables` if non-empty, else `[table]`,
    /// else the `"tasks"` floor (the same default `sdk/nostos_node` pins).
    fn tables(&self) -> Vec<String> {
        match &self.tables {
            Some(t) if !t.is_empty() => t.clone(),
            _ => vec![self.table.clone().unwrap_or_else(|| "tasks".to_owned())],
        }
    }

    /// The resolved default SQLite path (`"cairn.db"` floor).
    fn db_path(&self) -> String {
        self.db_path
            .clone()
            .unwrap_or_else(|| "cairn.db".to_owned())
    }
}

/// Plugin state, managed by Tauri. Owns a `tokio::runtime::Runtime` (home of
/// the `subscribe()` run loop) plus at most one active session covering the
/// configured table set (`plugins.cairn.tables`).
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
    // A2 config defaults (plugins.cairn from tauri.conf.json). Immutable
    // after init() — connect() merges per-call args over it.
    config: NostosPluginConfig,
    // A3 push-REST credentials — the same "one credential source, one URL
    // source" cache sdk/nostos_node keeps on its handle. SyncClient offers
    // set_token but NO token getter, so the SDK layer must remember what it
    // connected with to send Authorization: Bearer on the push-token REST
    // round-trips (the SAME JWT the WS handshake uses — ADR-0037 §3).
    // Updated by connect()/set_token(); cleared by sign_out() (which captures
    // the pre-clear value first — the deregistration hook needs it).
    token_cache: StdMutex<Option<String>>,
    // The WS URL of the (last) successful connect — the push REST base is
    // derived from it per call (http_base), so registration follows the
    // same server the session syncs with even after a config change.
    session_url: StdMutex<Option<String>>,
    // Push tokens registered THIS session (ADR-0037 §3), deregistered
    // best-effort by sign_out — a leaked registration would push the
    // previous principal's data to the next user.
    //
    // ponytail: in-memory only — tokens registered before a process restart
    // are not auto-deregistered (the set dies with the process). The stale
    // case is covered server-side: the rails prune on APNs 410 / FCM
    // UNREGISTERED. Upgrade path: persist the set in the local store if
    // rail-prune proves too slow for real tenants.
    registered_push_tokens: StdMutex<Vec<String>>,
}

struct Session {
    client: Arc<SyncClient<SqliteStorage>>,
    /// The configured table set; every table-taking command checks membership.
    tables: Vec<String>,
    // `JoinHandle` for the background `run_once()` task spawned by
    // `subscribe()`. `None` until subscribe() is called; aborted on
    // `abort_subscribe()` or session drop. Tied to `NostosState.rt`'s runtime.
    run_handle: Option<tokio::task::JoinHandle<()>>,
    // `JoinHandle`s for the background `watch()` pumps spawned since the
    // session opened (one per `watch()` call). Aborted on `abort_subscribe()`
    // for deterministic teardown; each pump ALSO self-terminates when its Tauri
    // channel closes (JS unsubscribe) or the client's change broadcast ends.
    watch_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Session {
    /// The membership guard every table-taking command shares.
    fn client_for(&self, table: &str, op: &str) -> Result<&Arc<SyncClient<SqliteStorage>>, String> {
        if self.tables.iter().any(|t| t == table) {
            Ok(&self.client)
        } else {
            Err(format!(
                "{op}() table {table:?} is not in the session tables {:?} — add it to plugins.cairn.tables",
                self.tables
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Reactive watch facade (ADR-0024) — Tauri port of node's `watch()` / Flutter's
// `rows_sink`. Pushes a fresh full-table snapshot to the JS frontend over a
// `tauri::ipc::Channel` on every change tick (remote apply OR local write),
// draining `SyncClient::subscribe_changes()`. NOT a poll.
// ---------------------------------------------------------------------------

/// One reactive-watch snapshot pushed to the JS frontend via a Tauri
/// `ipc::Channel`. The FULL per-table row set, re-queried on every change tick
/// (not a diff — self-healing on lag). `rows` is one JSON object per row (keyed
/// by column name) — the SAME shape `query()` returns, so a JS caller reads
/// each row identically; a `Vec` (not a JSON string) so Tauri serializes the
/// channel event natively.
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NostosSnapshot {
    /// The session table this snapshot covers.
    pub table: String,
    /// One JSON object per row (`pk`, `payload`) read from `cairn_data`.
    pub rows: Vec<serde_json::Value>,
}

/// The outbox status surfaced as the unified-verb `deadLetters` command —
/// the ADR-0027 `WriteQueueStatus` shape, camelCase for the JS guest.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NostosWriteStatus {
    /// Writes durably queued but not yet server-ack'd (> 0 offline is the
    /// offline-first promise working, not an error).
    pub pending: u64,
    /// Writes permanently failed this session (quarantined, inspectable —
    /// NOT deleted). Zero is the healthy steady state.
    pub dead_lettered: u64,
    /// The server's error text from the most recent dead-letter, verbatim
    /// (names the exact env var for allowlist rejections).
    pub last_error: Option<String>,
}

/// Internal reactive-emitter seam (mirrors `sdk/nostos_node`'s `SnapshotEmitter`):
/// keeps the pump / ordering logic drivable in pure-Rust host tests WITHOUT a
/// live Tauri `ipc::Channel` (which needs a Tauri app env to construct). The
/// production leaf is `ChannelEmitter`; the reactivity test uses a recording
/// leaf. `emit` returns `false` when the consumer is gone (channel closed =
/// JS unsubscribed) so the pump can self-terminate for clean teardown.
trait SnapshotEmitter: Send + Sync {
    /// Fire-and-forget snapshot delivery. Synchronous: the Tauri
    /// `Channel::send` (production) and `mpsc::send` (test) are both
    /// scheduling primitives, not async callbacks — mirroring node's
    /// `SnapshotEmitter::emit`. Returns `false` iff the consumer is gone.
    fn emit(&self, snapshot: NostosSnapshot) -> bool;
}

/// Tauri production emitter: wraps an `ipc::Channel<NostosSnapshot>` and forwards
/// each snapshot by `send`. `Channel` is `Clone + Send + Sync`, so a tokio pump
/// on the owned runtime can drive it from any worker. `send` errors when the
/// frontend has dropped its end (the unsubscribe path); `emit` returns `false`
/// then, which the pump treats as teardown — no orphan task re-querying forever.
struct ChannelEmitter(tauri::ipc::Channel<NostosSnapshot>);

impl SnapshotEmitter for ChannelEmitter {
    fn emit(&self, snapshot: NostosSnapshot) -> bool {
        self.0.send(snapshot).is_ok()
    }
}

/// Snapshot the session table's rows directly from `cairn_data` (NOT a typed
/// VIEW — mirrors `sdk/nostos_node::snapshot_json`): `cairn_data` exists on every
/// store right after `open()`, so this works before any server schema ships. The
/// query is `SELECT pk, payload FROM cairn_data WHERE table_name = '{table}'` —
/// the same shape `query()` emits, minus the serialize-to-string step (the Tauri
/// channel serializes `NostosSnapshot` natively).
///
/// # Errors
/// `String` for a `ClientError` (outer) or `StorageError` (inner) from the
/// storage round-trip — the same double-`map_err` shape `query()` uses.
async fn snapshot_rows(
    client: &SyncClient<SqliteStorage>,
    table: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let sql =
        format!("SELECT pk, payload FROM cairn_data WHERE table_name = '{table}' ORDER BY pk ASC");
    let rows: Vec<serde_json::Map<String, serde_json::Value>> = client
        .with_storage(move |s| s.query(&sql))
        .await
        .map_err(|e: ClientError| e.to_string())? // outer: ClientError
        .map_err(|e| e.to_string())?; // inner: StorageError (nested Result)
    Ok(rows.into_iter().map(serde_json::Value::Object).collect())
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
        Self::with_config(NostosPluginConfig::default())
    }

    /// Construct with plugins.cairn config defaults (the production path:
    /// init() calls this with the deserialized tauri.conf.json block).
    /// See NostosPluginConfig for the per-field precedence.
    ///
    /// # Panics
    /// Panics if the tokio runtime can't be constructed (OS resource
    /// exhaustion) or if invoked from inside an async context.
    pub fn with_config(config: NostosPluginConfig) -> Self {
        Self {
            rt: Some(tokio::runtime::Runtime::new().expect("construct nostos-tauri runtime")),
            session: AsyncMutex::new(None),
            config,
            token_cache: StdMutex::new(None),
            session_url: StdMutex::new(None),
            registered_push_tokens: StdMutex::new(Vec::new()),
        }
    }

    /// Open the local SQLite store and build a `SyncClient` against `url`.
    /// No network I/O — the subscribe/run loop is a separate command.
    /// Idempotent: a second call while a session is live is a no-op.
    ///
    /// A2 precedence — per-call args override `plugins.cairn` config, config
    /// overrides the floor: `url` falls back to `config.syncUrl`, `token` to
    /// `config.token`, `db_path` to `config.dbPath` (floor `"cairn.db"`), and
    /// the session tables to `config.tables` (floor `["tasks"]`, matching
    /// `nostos_node`). With a fully-populated config, `connect()` needs no
    /// args at all.
    ///
    /// # Errors
    /// `String` if neither the arg nor config supplies a sync URL, or the
    /// SQLite store can't be opened/migrated.
    pub async fn connect(
        &self,
        url: Option<String>,
        token: Option<String>,
        db_path: Option<String>,
    ) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        // A2 precedence: arg > config. A missing URL is the one hard error —
        // every other field has a floor.
        let Some(url) = url.or_else(|| self.config.sync_url.clone()) else {
            return Err("connect() called with no url and no plugins.cairn.syncUrl config — one of the two is required".to_string());
        };
        let token = token.or_else(|| self.config.token.clone());
        let db_path = db_path.unwrap_or_else(|| self.config.db_path());
        let tables = self.config.tables();
        let where_sql = self.config.where_sql.clone().unwrap_or_default();
        if let Some(stray) = where_sql.keys().find(|k| !tables.contains(k)) {
            return Err(format!(
                "plugins.cairn.whereSql names table {stray:?} which is not in the session tables {tables:?}"
            ));
        }
        let storage = SqliteStorage::open(&db_path).map_err(|e| e.to_string())?;
        let config = SyncClientConfig {
            table: tables[0].clone(),
            where_sql: where_sql.get(&tables[0]).cloned(),
            extra_tables: tables[1..]
                .iter()
                .map(|t| TableSub {
                    name: t.clone(),
                    where_sql: where_sql.get(t).cloned(),
                })
                .collect(),
            token: token.clone(),
            idle_timeout: Some(IDLE_RECONNECT_BACKSTOP),
            or_set_tables: self
                .config
                .or_set_tables
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            counter_tables: self
                .config
                .counter_tables
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            ..SyncClientConfig::default()
        };
        let client = Arc::new(SyncClient::new(url.clone(), storage, config));
        *guard = Some(Session {
            client,
            tables,
            run_handle: None,
            watch_tasks: Vec::new(),
        });
        // A3: remember the credential + URL the push-token REST round-trips
        // need (SyncClient exposes no token getter; http_base derives the
        // REST origin from the WS URL).
        *self.session_url.lock().expect("session_url lock poisoned") = Some(url);
        *self.token_cache.lock().expect("token_cache lock poisoned") = token;
        Ok(())
    }

    /// Start the live-replication run loop on the owned runtime. The session's
    /// `SyncClient::run_once()` drives the real WS subscribe → apply pipeline:
    /// server-pushed rows land in the on-device SQLite store, and the server's
    /// echo `WriteBack` re-emits this client's own `write()`s back through the
    /// same path. Received rows are observed via `query()` (a JS-layer row-tick
    /// `Channel` is the ponytail upgrade). `table` MUST be in the session's
    /// table set (the run loop itself is per session). Idempotent in the
    /// sense that a second call replaces + aborts a prior run loop for the same
    /// session.
    ///
    /// # Errors
    /// `String` if no session is active or the table is not configured.
    pub async fn subscribe(&self, table: String) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let client = match guard.as_ref() {
            None => {
                return Err("subscribe() called before connect()".to_string());
            }
            Some(s) => Arc::clone(s.client_for(&table, "subscribe")?),
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

    /// Abort the background run loop spawned by `subscribe()`, if any, AND any
    /// live `watch()` pumps. No-op if neither was started or both have already
    /// been aborted/finished. The session itself stays open — `query()` /
    /// `checkpoint()` keep working against the on-device store; only live
    /// replication + reactive pushes pause. (Each watch pump also self-
    /// terminates when its Tauri channel closes — the JS unsubscribe path.)
    pub async fn abort_subscribe(&self) {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            if let Some(handle) = session.run_handle.take() {
                handle.abort();
            }
            for handle in session.watch_tasks.drain(..) {
                handle.abort();
            }
        }
    }

    /// ADR-0029 sign-out: tear the session down so the next principal sees a
    /// blank device. Order is load-bearing (the `clear_local_state` contract):
    /// (1) abort the run loop + watch pumps, (2) AWAIT the handles — true
    /// quiescence, not just the abort signal, so a frame already in flight when
    /// `abort()` fired can't re-populate storage after the clear ("half a clear
    /// is a cross-user leak"), (3) `clear_local_state()` wipes rows + outbox +
    /// resets the checkpoint/epoch under one engine lock, (4) clear the token,
    /// (5) drop the Session. The run loop's drop closes the WS socket. After
    /// this the slot is `None`, so the next `connect()` builds a fresh client.
    ///
    /// `abort_subscribe()` is NOT reused here: it aborts but does not await, so
    /// it cannot guarantee the quiescence sign-out requires.
    ///
    /// # Errors
    /// `String` if the local-state wipe itself failed (disk error). A failed
    /// wipe is surfaced, not swallowed — half a clear is a leak.
    pub async fn sign_out(&self) -> Result<(), String> {
        // ADR-0037 §3: the sign-out deregistration needs the JWT from BEFORE
        // step (4) clears it — capture it (and the push registry) now.
        let auth = self
            .token_cache
            .lock()
            .expect("sign_out: token_cache lock poisoned")
            .clone();
        let registered = std::mem::take(
            &mut *self
                .registered_push_tokens
                .lock()
                .expect("sign_out: registered_push_tokens lock poisoned"),
        );
        let mut guard = self.session.lock().await;
        let Some(session) = guard.take() else {
            return Ok(()); // idempotent — nothing to sign out
        };
        // (1)+(2) Abort AND await the run loop + watch pumps for quiescence.
        // `abort()` only signals cancellation; awaiting the handle guarantees
        // the future is actually dropped before we clear, so no post-clear
        // apply/flush can race the wipe. The awaited handle resolves
        // `Err(JoinError { is_cancelled: true })` — discarded. The handles live
        // on `self.rt`; awaiting them from the command-handler runtime is a
        // cross-runtime await (registers a waker), not a block.
        if let Some(handle) = session.run_handle {
            handle.abort();
            let _ = handle.await;
        }
        for handle in session.watch_tasks {
            handle.abort();
            let _ = handle.await;
        }
        // (3) Wipe local rows + outbox under one engine lock. Safe now: the run
        // loop is quiesced, no apply/flush can re-populate after the clear.
        session
            .client
            .clear_local_state()
            .await
            .map_err(|e: ClientError| e.to_string())?;
        // (4) Clear the token. Defensive — the client drops at (6) with the
        // session, but clearing here matches the signOut contract and guards a
        // stray `Arc` clone from re-authing on a reconnect. A3: the SDK cache
        // clears too, so a post-sign-out push registration has no credential
        // to send.
        session.client.set_token(None);
        *self
            .token_cache
            .lock()
            .expect("sign_out: token_cache lock poisoned") = None;
        // (5) ADR-0037 §3: deregister this session's push tokens — best-effort
        // (a failed DELETE is swallowed; the server prunes stale rows on a rail
        // 410/UNREGISTERED). AFTER the local wipe, mirroring the Flutter + Node
        // SDKs' hook ordering. Uses the token captured before (4) cleared it.
        for token in registered {
            let _ =
                Self::deregister_push_token_http(&self.session_url, auth.as_deref(), &token).await;
        }
        // (6) `session` drops here; its handles are already drained and the
        // session slot is already `None`.
        Ok(())
    }

    /// ADR-0029: swap the auth token on the LIVE client without reconnecting.
    /// A refresh self-heals within one backoff window — the next socket open
    /// picks the new token up; storage, outbox, and `changes` subscribers all
    /// survive. Thin wrapper over the sync `SyncClient::set_token`
    /// (`client.rs:358`). Requires `connect()` to have run.
    ///
    /// # Errors
    /// `String` if no session is active.
    pub async fn set_token(&self, token: Option<String>) -> Result<(), String> {
        let client = {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| "set_token() called before connect()".to_string())?;
            Arc::clone(&session.client)
        };
        // `set_token` is a sync RwLock swap — no `.await` on the call itself.
        // A3: mirror the swap into the SDK's token cache so push-token REST
        // round-trips send the SAME credential the next WS open will.
        *self.token_cache.lock().expect("token_cache lock poisoned") = token.clone();
        client.set_token(token);
        Ok(())
    }

    /// Register this device's push token with the server (ADR-0037 §3):
    /// `POST /push-tokens` with `{"platform": …, "token": …}`, authenticated
    /// by the SAME token the sync connection uses (`Authorization: Bearer`,
    /// read from this state's token cache — the credential `connect()` built
    /// the `SyncClient` from). The server stamps tenant/account itself; the
    /// SDK never attests identity fields.
    ///
    /// A3 parity: byte-identical wire shape to the Flutter SDK's
    /// `NostosDatabase.registerPushToken` (`nostos_database.dart`) and
    /// `sdk/nostos_node`'s `registerPushToken` — one REST contract across
    /// SDKs. On iOS/Android the token comes from the Tauri mobile shell's
    /// native push hooks (APNs device token / FCM registration token).
    /// Desktop has no OS rail: an online session already receives everything
    /// over the WS, and doorbells only target offline devices (ADR-0037 §1),
    /// so desktop apps usually do not register at all; a Web Push
    /// subscription the host app obtains may register under `"webpush"`.
    ///
    /// `platform` is `"fcm"`, `"apns"`, or `"webpush"`. Resolves on the
    /// pinned `204`; any other status errors with the status + body.
    /// Registered tokens are deregistered best-effort by `sign_out`.
    ///
    /// ponytail: a fresh reqwest client per call — registration is a rare
    /// path, not a hot loop. Share one `Client` on the state if a
    /// measurement ever says otherwise.
    ///
    /// # Errors
    /// `String` for an unknown platform, no prior `connect()` (no URL to
    /// derive the REST base from), a transport failure, or any non-204 reply.
    pub async fn register_push_token(&self, platform: String, token: String) -> Result<(), String> {
        match platform.as_str() {
            "fcm" | "apns" | "webpush" => {}
            other => {
                return Err(format!(
                    "unknown push platform {other:?}: expected \"fcm\", \"apns\", or \"webpush\""
                ))
            }
        }
        let auth = self
            .token_cache
            .lock()
            .expect("token_cache lock poisoned")
            .clone();
        let url = self
            .session_url
            .lock()
            .expect("session_url lock poisoned")
            .clone()
            .ok_or_else(|| "register_push_token() called before connect()".to_string())?;
        let body = serde_json::json!({"platform": platform, "token": token}).to_string();
        let mut request = reqwest::Client::new()
            .post(format!("{}/push-tokens", http_base(&url)))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(jwt) = &auth {
            request = request.bearer_auth(jwt);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("push-token register failed: {e}"))?;
        expect_204(response, "register").await?;
        self.registered_push_tokens
            .lock()
            .expect("registered_push_tokens lock poisoned")
            .push(token);
        Ok(())
    }

    /// Deregister a push token (ADR-0037 §3): `DELETE /push-tokens/{token}`
    /// with the same auth as `register_push_token`. Resolves on the pinned
    /// `204`. `sign_out` calls this for every session-registered token
    /// automatically; call it directly when the app can no longer receive on
    /// the token (e.g. the user disables notifications).
    ///
    /// The token rides the path percent-encoded as ONE segment
    /// (`encode_path_segment`): a webpush token is the full
    /// `pushSubscription` JSON and contains `/`, which would split the path
    /// and 404 the DELETE.
    ///
    /// # Errors
    /// `String` for no prior `connect()`, a transport failure, or any
    /// non-204 reply.
    pub async fn deregister_push_token(&self, token: String) -> Result<(), String> {
        let auth = self
            .token_cache
            .lock()
            .expect("token_cache lock poisoned")
            .clone();
        Self::deregister_push_token_http(&self.session_url, auth.as_deref(), &token).await?;
        self.registered_push_tokens
            .lock()
            .expect("registered_push_tokens lock poisoned")
            .retain(|t| t != &token);
        Ok(())
    }

    /// Shared DELETE core — `deregister_push_token` (reads the live cached
    /// token) and `sign_out` (reads the token captured BEFORE it was
    /// cleared) both ride this, so there is one wire shape.
    async fn deregister_push_token_http(
        session_url: &StdMutex<Option<String>>,
        auth: Option<&str>,
        token: &str,
    ) -> Result<(), String> {
        let url = session_url
            .lock()
            .expect("session_url lock poisoned")
            .clone()
            .ok_or_else(|| "push-token deregister called before connect()".to_string())?;
        let mut request = reqwest::Client::new().delete(format!(
            "{}/push-tokens/{}",
            http_base(&url),
            encode_path_segment(token)
        ));
        if let Some(jwt) = auth {
            request = request.bearer_auth(jwt);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("push-token deregister failed: {e}"))?;
        expect_204(response, "deregister").await
    }

    /// Enqueue a durable write against the active session's table. Resolves
    /// once the write is captured in the local outbox (NOT once the server
    /// acks it — ADR-0013 outbox contract). `op` is `"upsert"` / `"delete"` /
    /// `"patch"` (column-level UPDATE — `payload_json` carries only the
    /// changed columns). `table` MUST be in the session's table set.
    ///
    /// # Errors
    /// `String` if no session is active, the table is not configured, the op string
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
        let client = self.session_client(&table, "write").await?;
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

    /// The active session's client for a table-taking command — ONE guard
    /// discipline shared by write/subscribe/watch/CRDT verbs (no session →
    /// "<op>() called before connect()"; unknown table → names the
    /// configured set).
    async fn session_client(
        &self,
        table: &str,
        op: &str,
    ) -> Result<Arc<SyncClient<SqliteStorage>>, String> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| format!("{op}() called before connect()"))?;
        Ok(Arc::clone(session.client_for(table, op)?))
    }

    /// ADR-0030 add-wins OR-set: add `element` to the OR-set at `pk`.
    /// Optimistic-local like every write — resolves once the merge-upsert
    /// is durable in the outbox. The table must be declared in
    /// `plugins.cairn.orSetTables` (the client gate; the server's
    /// `NOSTOS_OR_SET_COLUMNS` must agree).
    pub async fn or_set_add(
        &self,
        table: String,
        pk: String,
        element: String,
    ) -> Result<u64, String> {
        let client = self.session_client(&table, "orSetAdd").await?;
        client
            .or_set_add(&table, &pk, &element)
            .await
            .map_err(|e: ClientError| e.to_string())
    }

    /// ADR-0030 add-wins OR-set remove — a tombstone at a fresh HLC; a
    /// concurrent/later re-add re-activates the element.
    pub async fn or_set_remove(
        &self,
        table: String,
        pk: String,
        element: String,
    ) -> Result<u64, String> {
        let client = self.session_client(&table, "orSetRemove").await?;
        client
            .or_set_remove(&table, &pk, &element)
            .await
            .map_err(|e: ClientError| e.to_string())
    }

    /// ADR-0030 PN-Counter increment by `delta` (bumps this replica's
    /// positive counter). Table must be in `plugins.cairn.counterTables`.
    pub async fn counter_increment(
        &self,
        table: String,
        pk: String,
        delta: i64,
    ) -> Result<u64, String> {
        let client = self.session_client(&table, "counterIncrement").await?;
        client
            .counter_increment(&table, &pk, delta)
            .await
            .map_err(|e: ClientError| e.to_string())
    }

    /// ADR-0030 PN-Counter decrement by `delta` (bumps the negative
    /// counter for this replica).
    pub async fn counter_decrement(
        &self,
        table: String,
        pk: String,
        delta: u64,
    ) -> Result<u64, String> {
        let client = self.session_client(&table, "counterDecrement").await?;
        client
            .counter_decrement(&table, &pk, delta)
            .await
            .map_err(|e: ClientError| e.to_string())
    }

    /// ADR-0027 outbox status — pending count, dead-letter count, and the
    /// most recent dead-letter error verbatim (the unified-verb surface's
    /// `deadLetters`).
    pub async fn write_queue_status(&self) -> Result<NostosWriteStatus, String> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| "deadLetters() called before connect()".to_string())?;
        let status = session.client.write_status();
        Ok(NostosWriteStatus {
            pending: status.pending,
            dead_lettered: status.dead_lettered,
            last_error: status.last_error,
        })
    }

    /// True once the live session has PROVEN a subscription (first frame
    /// or write ack) — the unified-verb surface's `connectionState`.
    pub async fn is_subscribed(&self) -> Result<bool, String> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| "connectionState() called before connect()".to_string())?;
        Ok(*session.client.subscribed().borrow())
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

    /// Reactive watch (ADR-0024): push the session table's full snapshot to the
    /// JS frontend via `on_event` immediately, and again after every change
    /// tick (remote apply OR local write). The Tauri port of node's `watch()` /
    /// Flutter's `watch(table, rows_sink)` — a TRUE Rust→JS push over a Tauri
    /// `ipc::Channel`, NOT a poll.
    ///
    /// `on_event` is a `Channel<NostosSnapshot>` Tauri constructs from the JS
    /// caller; it is `Clone + Send + Sync`, so the tokio pump (on the owned
    /// runtime) can `send` from any worker. The pump self-terminates when the
    /// channel closes (JS drops its end = unsubscribe) — clean teardown without
    /// an explicit stop — and `abort_subscribe()` aborts any live pump
    /// deterministically.
    ///
    /// # Load-bearing ordering: subscribe BEFORE the first snapshot read
    /// (nostos-client invariant
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`, same as
    /// node/kotlin): the change broadcast is no-replay (`broadcast::channel(64)`),
    /// so a receiver created AFTER a commit permanently misses it — the
    /// "connected but lists render empty" regression. This port creates the
    /// receiver FIRST, then reads the initial snapshot; a commit in the residual
    /// gap just triggers a redundant re-snapshot (idempotent — full snapshot,
    /// self-healing on lag).
    ///
    /// `table` MUST be in the session's table set; each `watch()` pumps ONE
    /// table — call it once per table you render.
    ///
    /// # Errors
    /// `String` if no session is active, the table is not configured, or the
    /// initial snapshot query fails.
    pub async fn watch(
        &self,
        table: String,
        on_event: tauri::ipc::Channel<NostosSnapshot>,
    ) -> Result<(), String> {
        let emitter = Arc::new(ChannelEmitter(on_event)) as Arc<dyn SnapshotEmitter>;
        self.watch_internal(table, emitter).await
    }

    /// Shared reactive-watch core — the Tauri `watch()` (channel emitter) and
    /// the host reactivity test (recording emitter) both drive this, so the
    /// pump / ordering logic is provable WITHOUT a Tauri app env (a `Channel`
    /// can't be constructed in a plain unit test).
    async fn watch_internal(
        &self,
        table: String,
        emitter: Arc<dyn SnapshotEmitter>,
    ) -> Result<(), String> {
        let client = self.session_client(&table, "watch").await?;

        // (1) SUBSCRIBE FIRST — load-bearing (see `watch` doc + the nostos-client
        // invariant). Must precede the initial snapshot read; this owned
        // receiver is the only way to learn of a commit landing in the gap
        // before the pump starts. `subscribe_changes` returns an OWNED receiver
        // (no session borrow).
        let mut changes = client.subscribe_changes();

        // (2) Initial snapshot AFTER subscribing, emitted immediately.
        let rows = snapshot_rows(&client, &table).await?;
        emitter.emit(NostosSnapshot {
            table: table.clone(),
            rows,
        });

        // (3) Pump on the OWNED runtime (NOT the command-handler runtime): re-snapshot
        // on EVERY change tick. Full snapshot per tick (not a diff — self-healing
        // on lag). `Lagged` (receiver fell >64 ticks behind) is treated as a tick
        // — a full snapshot resyncs. `Closed` (client dropped its senders) fails
        // the `while let` and the pump exits. A `false` emit (channel closed =
        // JS unsubscribed) also breaks → clean teardown without an explicit stop.
        let pump_table = table;
        let pump_emitter = Arc::clone(&emitter);
        let rt = self
            .rt
            .as_ref()
            .expect("runtime present until NostosState drops");
        let handle = rt.spawn(async move {
            while let Ok(_) | Err(RecvError::Lagged(_)) = changes.recv().await {
                if let Ok(rows) = snapshot_rows(&client, &pump_table).await {
                    if !pump_emitter.emit(NostosSnapshot {
                        table: pump_table.clone(),
                        rows,
                    }) {
                        break; // channel closed → unsubscribe teardown
                    }
                }
                // snapshot read failure (transient): skip this tick, next retries.
            }
        });

        // Track the pump for deterministic teardown on `abort_subscribe()`.
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_mut() {
            session.watch_tasks.push(handle);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Push-token REST helpers (ADR-0037 §3) — ported from sdk/nostos_node so both
// JS-facing SDKs speak a byte-identical wire shape against the same pinned
// server contract (crates/nostos-server/src/push_api.rs).
// ---------------------------------------------------------------------------

/// Derive the HTTP base for the push-token REST endpoints from the WS `/sync`
/// URL: `wss`→`https`, `ws`→`http`, trailing path stripped — the same
/// derivation the Flutter SDK uses for `GET /schema`
/// (`NostosDatabase._deriveHttpBase`) and node's `http_base`. One credential
/// source, one URL source.
fn http_base(ws_url: &str) -> String {
    match ws_url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = match scheme {
                "wss" => "https",
                "ws" => "http",
                other => other,
            };
            // Authority runs to the first `/` (or end); the path is dropped.
            let authority = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{authority}")
        }
        None => ws_url.to_owned(),
    }
}

/// Percent-encode a push token as ONE path segment: every byte outside RFC
/// 3986's unreserved set (`A-Za-z0-9-._~`) becomes `%XX`. A webpush token
/// is the full `pushSubscription` JSON — it contains `/`, which un-encoded
/// splits the path and 404s the DELETE. Hand-rolled: this standalone
/// workspace has no `percent-encoding` dep and the path-safe subset is this
/// small; the server's `Path` extractor decodes it back verbatim.
fn encode_path_segment(token: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(token.len());
    for &b in token.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(b >> 4)]));
                out.push(char::from(HEX[usize::from(b & 0x0F)]));
            }
        }
    }
    out
}

/// Enforce the pinned push-token contract (ADR-0037 §3): success is exactly
/// `204 No Content`. Anything else — including a 2xx variant — surfaces the
/// status + body so contract drift fails loudly on the SDK side.
async fn expect_204(response: reqwest::Response, operation: &str) -> Result<(), String> {
    let status = response.status();
    if status.as_u16() == 204 {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(format!(
        "push-token {operation} failed: HTTP {}: {body}",
        status.as_u16()
    ))
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
/// Every arg is optional and falls back to the `plugins.cairn` config block
/// (A2): with a populated `tauri.conf.json`, `connect()` needs no args.
#[tauri::command]
async fn connect(
    state: State<'_, NostosState>,
    url: Option<String>,
    token: Option<String>,
    db_path: Option<String>,
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

/// Reactive watch: push the full per-table snapshot to `on_event` immediately,
/// and again after every change tick (ADR-0024). `on_event` is a Tauri
/// `ipc::Channel<NostosSnapshot>` the JS frontend constructs via
/// `new Channel()`; the pump sends snapshots from a tokio worker on the owned
/// runtime. Drop the channel (or call `nostos unsubscribe`) to stop the pump.
#[tauri::command]
async fn watch(
    state: State<'_, NostosState>,
    table: String,
    on_event: tauri::ipc::Channel<NostosSnapshot>,
) -> Result<(), String> {
    state.watch(table, on_event).await
}

/// ADR-0029 sign-out: stop sync, close the socket, wipe local rows + outbox,
/// and clear the token so the next principal sees a blank device. Idempotent.
#[tauri::command]
async fn sign_out(state: State<'_, NostosState>) -> Result<(), String> {
    state.sign_out().await
}

/// ADR-0029: swap the auth token on the live client (a refresh self-heals
/// within one backoff window). Requires `connect()` to have run.
#[tauri::command]
async fn set_token(state: State<'_, NostosState>, token: Option<String>) -> Result<(), String> {
    state.set_token(token).await
}

/// ADR-0037 §3: register this device's push token (`POST /push-tokens`)
/// with the same auth the sync connection uses. On iOS/Android the token
/// comes from the Tauri mobile shell's APNs/FCM native hooks; desktop apps
/// usually do not register (no OS rail — an online session gets WS delivery).
#[tauri::command]
async fn register_push_token(
    state: State<'_, NostosState>,
    platform: String,
    token: String,
) -> Result<(), String> {
    state.register_push_token(platform, token).await
}

/// ADR-0037 §3: deregister a push token (`DELETE /push-tokens/{token}`);
/// `sign_out` does this automatically for session-registered tokens.
#[tauri::command]
async fn deregister_push_token(state: State<'_, NostosState>, token: String) -> Result<(), String> {
    state.deregister_push_token(token).await
}

/// ADR-0030 add-wins OR-set add (see [`NostosState::or_set_add`]).
#[tauri::command]
async fn or_set_add(
    state: State<'_, NostosState>,
    table: String,
    pk: String,
    element: String,
) -> Result<u64, String> {
    state.or_set_add(table, pk, element).await
}

/// ADR-0030 OR-set remove (tombstone at a fresh HLC; add-wins).
#[tauri::command]
async fn or_set_remove(
    state: State<'_, NostosState>,
    table: String,
    pk: String,
    element: String,
) -> Result<u64, String> {
    state.or_set_remove(table, pk, element).await
}

/// ADR-0030 PN-Counter increment by `delta`.
#[tauri::command]
async fn counter_increment(
    state: State<'_, NostosState>,
    table: String,
    pk: String,
    delta: i64,
) -> Result<u64, String> {
    state.counter_increment(table, pk, delta).await
}

/// ADR-0030 PN-Counter decrement by `delta`.
#[tauri::command]
async fn counter_decrement(
    state: State<'_, NostosState>,
    table: String,
    pk: String,
    delta: u64,
) -> Result<u64, String> {
    state.counter_decrement(table, pk, delta).await
}

/// ADR-0027 outbox status — pending/dead-letter counts + the last
/// dead-letter error (the unified-verb `deadLetters` surface).
#[tauri::command]
async fn dead_letters(state: State<'_, NostosState>) -> Result<NostosWriteStatus, String> {
    state.write_queue_status().await
}

/// True once the session has proven a subscription — the unified-verb
/// `connectionState` surface.
#[tauri::command]
async fn connection_state(state: State<'_, NostosState>) -> Result<bool, String> {
    state.is_subscribed().await
}

/// Build the `nostos` Tauri plugin. Generic over `R: Runtime` so a Tauri app
/// using any runtime (the default `Wry`, or a custom one) can register it via
/// `tauri::Builder::default().plugin(tauri_plugin_cairn::init())`.
pub fn init<R: Runtime>() -> TauriPlugin<R, NostosPluginConfig> {
    // A2 config story: the second Builder type parameter is the
    // plugins.cairn block of tauri.conf.json — Tauri deserializes it in
    // TauriPlugin::initialize (erroring loudly on a malformed block) and
    // hands it to setup via api.config(). Absent block == empty object ==
    // all-None defaults, so the plugin also works with zero config.
    Builder::<R, NostosPluginConfig>::new("cairn")
        .setup(|app, api| {
            app.manage(NostosState::with_config(api.config().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect,
            subscribe,
            write,
            query,
            checkpoint,
            watch,
            set_token,
            sign_out,
            register_push_token,
            deregister_push_token,
            or_set_add,
            or_set_remove,
            counter_increment,
            counter_decrement,
            dead_letters,
            connection_state,
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
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect("connect");

        // Round-trip an aliased `SELECT 1 AS one` through
        // `SyncClient::with_storage` → `SqliteStorage::query` — the same path
        // `nostos_node`'s `query()` takes. (Aliased so the column key is stable;
        // `nostos_node`'s smoke uses the same `AS one` shape.)
        let rows_json = state.query("SELECT 1 AS one".into()).await.expect("query");
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

    /// `plugins.cairn.whereSql` deserializes camelCase and a key outside the
    /// table set is refused at `connect` (not silently dropped).
    #[tokio::test]
    async fn where_sql_config_is_validated_against_tables() {
        let cfg: NostosPluginConfig = serde_json::from_str(
            r#"{"tables":["tasks","notes"],"whereSql":{"notes":"owner_id = 'u1'","ghost":"1=1"}}"#,
        )
        .expect("config parses");
        assert_eq!(cfg.where_sql.as_ref().unwrap()["notes"], "owner_id = 'u1'");
        let err = NostosState::with_config(cfg)
            .connect(
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect_err("stray whereSql key must be refused");
        assert!(err.contains("\"ghost\""), "got: {err}");

        // A predicate on a listed table connects fine.
        let cfg: NostosPluginConfig = serde_json::from_str(
            r#"{"tables":["tasks","notes"],"whereSql":{"notes":"owner_id = 'u1'"}}"#,
        )
        .expect("config parses");
        NostosState::with_config(cfg)
            .connect(
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect("connect with a valid whereSql");
    }

    /// `write()` before `connect()` surfaces a clear error rather than
    /// panicking — the same contract `nostos_node` enforces.
    #[tokio::test]
    async fn write_before_connect_is_an_error() {
        let state = NostosState::new();
        let err = state
            .write("tasks".into(), "upsert".into(), "pk1".into(), None)
            .await
            .expect_err("write before connect should error");
        assert!(
            err.contains("before connect"),
            "expected a before-connect error, got: {err}"
        );
    }

    /// ADR-0029 sign_out: a row written before sign_out is gone after AND the
    /// session is torn down (a later `query()` is a "before connect" error).
    /// Uses a temp FILE (not `:memory:`) so re-`connect()` reopens the SAME
    /// store and the wipe is observable on disk — `:memory:` would hide it
    /// behind a fresh DB on every open. Also pins idempotency (a second
    /// sign_out with no session is `Ok`).
    #[tokio::test(flavor = "multi_thread")]
    async fn sign_out_wipes_rows_and_drops_session() {
        let db =
            std::env::temp_dir().join(format!("nostos-tauri-signout-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let path = db.to_str().expect("utf8 db path").to_owned();

        let state = NostosState::new();
        state
            .connect(Some("ws://localhost:0".into()), None, Some(path.clone()))
            .await
            .expect("connect");
        state
            .write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                Some(r#"{"id":"pk1","title":"before-signout"}"#.into()),
            )
            .await
            .expect("write");

        // Sanity: the write landed in cairn_data (instant-local-apply, WS2).
        let before = state
            .query("SELECT pk FROM cairn_data WHERE table_name = 'tasks'".into())
            .await
            .expect("query before sign_out");
        assert!(
            before.contains("pk1"),
            "write should land in cairn_data before sign_out, got: {before}"
        );

        // Sign out: abort + quiesce + clear + clear-token + drop session.
        state.sign_out().await.expect("sign_out");

        // (1) Session is gone — query() is now a "before connect" error.
        let err = state
            .query("SELECT 1 AS one".into())
            .await
            .expect_err("query after sign_out should error (no session)");
        assert!(
            err.contains("before connect"),
            "expected before-connect error after sign_out, got: {err}"
        );

        // (2) The wipe persisted to disk — reopen the SAME file and the row is
        // gone. This is the cross-user-leak guard from ADR-0029.
        state
            .connect(Some("ws://localhost:0".into()), None, Some(path.clone()))
            .await
            .expect("reconnect");
        let rows_json = state
            .query("SELECT pk FROM cairn_data WHERE table_name = 'tasks'".into())
            .await
            .expect("query after reconnect");
        let rows: serde_json::Value = serde_json::from_str(&rows_json).expect("parse rows json");
        assert!(
            rows.as_array().is_some_and(|a| a.is_empty()),
            "tasks table should be empty after sign_out, got: {rows_json}"
        );

        // Idempotent: a second sign_out is a no-op (Ok).
        state.sign_out().await.expect("sign_out idempotent");
        let _ = std::fs::remove_file(&db);
    }

    // -------------------------------------------------------------------------
    // Reactive watch (ADR-0024) — host reactivity proof, mirroring
    // `sdk/nostos_node`'s `watch_emits_initial_snapshot_then_refires_on_local_write`.
    // No Tauri app env (a `ipc::Channel` can't be built in a unit test), so the
    // test drives `watch_internal` with a recording `SnapshotEmitter` leaf — the
    // production leaf differs only in `Channel::send` vs `mpsc::send`.
    // -------------------------------------------------------------------------

    /// REACTIVITY PROOF (host, no Tauri env, no live server): `watch_internal()`
    /// emits the initial snapshot, and a local `write()` — which applies a row
    /// to `cairn_data` AND fires the change broadcast (nostos-client invariant
    /// `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot`,
    /// `rows_applied == 1`) — causes the pump to emit a NEW snapshot, WITHOUT
    /// the test polling a timer. The `mpsc` receiver blocks on the pump's emit
    /// (an event wait), so this is reactive-by-callback, not reactive-by-poll.
    ///
    /// Also pins the subscribe-before-snapshot ordering: initial snapshot first
    /// (empty), then the post-write snapshot (contains the row).
    #[tokio::test(flavor = "multi_thread")]
    async fn watch_emits_initial_snapshot_then_refires_on_local_write() {
        use std::sync::mpsc;
        use std::sync::Mutex as StdMutex;

        struct RecordingEmitter(StdMutex<mpsc::Sender<NostosSnapshot>>);
        impl SnapshotEmitter for RecordingEmitter {
            fn emit(&self, snapshot: NostosSnapshot) -> bool {
                self.0.lock().expect("recorder lock").send(snapshot).is_ok()
            }
        }

        let state = NostosState::new();
        state
            .connect(
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect("connect");

        // watch_internal subscribes (broadcast receiver created BEFORE the
        // initial snapshot read — the load-bearing invariant) and emits the
        // initial snapshot before returning.
        let (tx, rx) = mpsc::channel::<NostosSnapshot>();
        let emitter = Arc::new(RecordingEmitter(StdMutex::new(tx))) as Arc<dyn SnapshotEmitter>;
        state
            .watch_internal("tasks".into(), Arc::clone(&emitter))
            .await
            .expect("watch");

        // (1) Initial snapshot delivered — empty store → rows == []. Blocking
        // event wait, 5s ceiling (no wall-clock polling).
        let initial = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("initial snapshot should arrive immediately");
        assert!(
            initial.rows.is_empty(),
            "fresh store tasks snapshot should be empty, got: {:?}",
            initial.rows
        );
        assert_eq!(initial.table, "tasks");

        // (2) Local write applies a row to cairn_data AND fires the change tick.
        // The pump (on the owned runtime) wakes, re-snapshots, emits AGAIN.
        state
            .write(
                "tasks".into(),
                "upsert".into(),
                "pk1".into(),
                Some(r#"{"id":"pk1","title":"reactive"}"#.into()),
            )
            .await
            .expect("write");

        // (3) The post-write snapshot arrives — the reactive proof.
        let after = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("post-write snapshot should arrive");
        assert!(
            after
                .rows
                .iter()
                .any(|r| r.get("pk").and_then(|v| v.as_str()) == Some("pk1")),
            "post-write snapshot should contain pk1, got: {:?}",
            after.rows
        );

        // Clean teardown: abort the session's background pumps.
        state.abort_subscribe().await;
    }

    // ─────────── push-token REST (ADR-0037 §3 / Track A3) ───────────
    // Mirrors sdk/nostos_node's pinned-contract tests byte-for-byte: the
    // server routes (crates/nostos-server/src/push_api.rs) are built against
    // the same pins, so drift fails here first.

    /// Build a full HTTP/1.1 reply with an exact Content-Length (no
    /// hand-counted lengths to drift).
    fn reply(status_line: &str, body: &str) -> String {
        format!(
            "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Spawn a local HTTP server on 127.0.0.1:0 that accepts `count`
    /// connections, replies `response` to each, and forwards each raw request
    /// (start line + headers + body, verbatim) over the channel. Hand-rolled
    /// on std::net so the pinned-contract tests add no dev-dependencies (an
    /// axum/hyper dev-dep would outweigh the scaffold SDK itself).
    /// `Connection: close` in the canned reply keeps reqwest from reusing a
    /// connection, so one accept == one request. Returns the host:port
    /// authority.
    fn spawn_capture_server(
        count: usize,
        response: String,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut raw = Vec::<u8>::new();
                let mut buf = [0u8; 4096];
                // Read until the headers end AND any Content-Length body is
                // fully received — one complete HTTP/1.1 request.
                loop {
                    let complete = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|pos| {
                        let headers = String::from_utf8_lossy(&raw[..pos]).to_ascii_lowercase();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        raw.len() >= pos + 4 + len
                    });
                    if complete == Some(true) {
                        break;
                    }
                    let n = match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                }
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                if tx.send(String::from_utf8_lossy(&raw).into_owned()).is_err() {
                    break;
                }
            }
        });
        (format!("127.0.0.1:{}", addr.port()), rx)
    }

    /// A NostosState pointed at the capture server (WS URL derived from the
    /// HTTP authority the way a real app's ws://…/sync URL would be).
    async fn push_state(authority: &str, token: Option<&str>) -> NostosState {
        let state = NostosState::new();
        state
            .connect(
                Some(format!("ws://{authority}/sync")),
                token.map(|t| t.to_owned()),
                Some(":memory:".into()),
            )
            .await
            .expect("connect");
        state
    }

    /// PINNED CONTRACT: register_push_token sends `POST /push-tokens` with the
    /// exact JSON body and the sync token as a Bearer header. The server
    /// routes are built against this same pin; drift fails here first.
    /// tenant/account are never sent — the server stamps them (ADR-0018
    /// discipline).
    #[tokio::test]
    async fn register_push_token_posts_exact_json_with_bearer() {
        let (authority, rx) = spawn_capture_server(1, reply("HTTP/1.1 204 No Content", ""));
        let state = push_state(&authority, Some("tauri-jwt")).await;
        state
            .register_push_token("fcm".into(), "tok-1".into())
            .await
            .expect("register should succeed on 204");

        let raw = rx.recv_timeout(Duration::from_secs(5)).expect("request");
        assert!(
            raw.starts_with("POST /push-tokens HTTP/1.1"),
            "expected POST /push-tokens, got: {raw}"
        );
        let lower = raw.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer tauri-jwt"),
            "expected the sync token as Bearer, got: {raw}"
        );
        assert!(
            lower.contains("content-type: application/json"),
            "expected a JSON content-type, got: {raw}"
        );
        assert!(
            raw.contains(r#"{"platform":"fcm","token":"tok-1"}"#),
            "expected the exact pinned JSON body, got: {raw}"
        );
    }

    /// PINNED CONTRACT: deregister_push_token sends
    /// `DELETE /push-tokens/{token}` with the same auth (register first so
    /// the happy path is also real).
    #[tokio::test]
    async fn deregister_push_token_deletes_the_token_path() {
        let (authority, rx) = spawn_capture_server(2, reply("HTTP/1.1 204 No Content", ""));
        let state = push_state(&authority, Some("tauri-jwt")).await;
        state
            .register_push_token("apns".into(), "tok-1".into())
            .await
            .expect("register");
        state
            .deregister_push_token("tok-1".into())
            .await
            .expect("deregister should succeed on 204");

        let _post = rx.recv_timeout(Duration::from_secs(5)).expect("POST");
        let raw = rx.recv_timeout(Duration::from_secs(5)).expect("DELETE");
        assert!(
            raw.starts_with("DELETE /push-tokens/tok-1 HTTP/1.1"),
            "expected DELETE /push-tokens/tok-1, got: {raw}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains("authorization: bearer tauri-jwt"),
            "expected the sync token as Bearer, got: {raw}"
        );
    }

    /// M1: a token containing reserved characters — a webpush token IS the
    /// full `pushSubscription` JSON, so it contains `/` — must ride the path
    /// percent-encoded as ONE segment; un-encoded it splits the path and the
    /// DELETE 404s (mirrors the Flutter push_token_test.dart pin).
    #[tokio::test]
    async fn deregister_push_token_percent_encodes_url_unsafe_token() {
        let (authority, rx) = spawn_capture_server(1, reply("HTTP/1.1 204 No Content", ""));
        let state = push_state(&authority, Some("tauri-jwt")).await;
        state
            .deregister_push_token("tok with spaces/+".into())
            .await
            .expect("deregister should succeed on 204");

        let raw = rx.recv_timeout(Duration::from_secs(5)).expect("DELETE");
        assert!(
            raw.starts_with("DELETE /push-tokens/tok%20with%20spaces%2F%2B HTTP/1.1"),
            "expected the token percent-encoded as one path segment, got: {raw}"
        );
    }

    /// Anything other than the pinned 204 surfaces the status + body in the
    /// error message (this SDK's error style is String, matching every other
    /// method here).
    #[tokio::test]
    async fn register_push_token_errors_on_non_204() {
        let (authority, _rx) = spawn_capture_server(
            1,
            reply("HTTP/1.1 401 Unauthorized", r#"{"error":"unauthorized"}"#),
        );
        let state = push_state(&authority, Some("stale-jwt")).await;
        let err = state
            .register_push_token("fcm".into(), "tok-1".into())
            .await
            .expect_err("non-204 must error");
        assert!(
            err.contains("401") && err.contains("unauthorized"),
            "expected status + body in the error, got: {err}"
        );
    }

    /// An unknown platform fails before the wire (no request reaches the
    /// server — it is spawned with zero accepts, so any request would hang
    /// the test).
    #[tokio::test]
    async fn register_push_token_unknown_platform_is_an_error() {
        let (authority, _rx) = spawn_capture_server(0, String::new());
        let state = push_state(&authority, Some("tauri-jwt")).await;
        let err = state
            .register_push_token("gcm".into(), "tok-1".into())
            .await
            .expect_err("unknown platform must error");
        assert!(
            err.contains("unknown push platform"),
            "expected a platform error, got: {err}"
        );
    }

    /// No connect() means no URL to derive the REST base from — a clear
    /// error, not a panic (the same contract the sync commands enforce).
    #[tokio::test]
    async fn register_push_token_before_connect_is_an_error() {
        let state = NostosState::new();
        let err = state
            .register_push_token("fcm".into(), "tok-1".into())
            .await
            .expect_err("register before connect should error");
        assert!(
            err.contains("before connect"),
            "expected a before-connect error, got: {err}"
        );
    }

    /// ADR-0029 + ADR-0037 §3: set_token swaps the credential on the LIVE
    /// client — the push cache must follow, so a post-refresh registration
    /// sends the SAME JWT the next WS open will (the refresh self-heal).
    #[tokio::test]
    async fn set_token_updates_the_push_credential_cache() {
        let (authority, rx) = spawn_capture_server(1, reply("HTTP/1.1 204 No Content", ""));
        let state = push_state(&authority, Some("stale-jwt")).await;
        state
            .set_token(Some("fresh-jwt".into()))
            .await
            .expect("set_token");
        state
            .register_push_token("fcm".into(), "tok-1".into())
            .await
            .expect("register");

        let raw = rx.recv_timeout(Duration::from_secs(5)).expect("request");
        assert!(
            raw.to_ascii_lowercase()
                .contains("authorization: bearer fresh-jwt"),
            "expected the refreshed token as Bearer, got: {raw}"
        );
    }

    /// ADR-0037 §3: sign_out deregisters session-registered tokens. The DELETE
    /// must carry the JWT captured BEFORE sign_out clears the token cache
    /// (step 4) — this test pins that ordering.
    #[tokio::test]
    async fn sign_out_deregisters_session_registered_tokens() {
        let (authority, rx) = spawn_capture_server(2, reply("HTTP/1.1 204 No Content", ""));
        let state = push_state(&authority, Some("tauri-jwt")).await;
        state
            .register_push_token("webpush".into(), "tok-1".into())
            .await
            .expect("register");
        state.sign_out().await.expect("sign_out");

        let _post = rx.recv_timeout(Duration::from_secs(5)).expect("POST");
        let raw = rx.recv_timeout(Duration::from_secs(5)).expect("DELETE");
        assert!(
            raw.starts_with("DELETE /push-tokens/tok-1 HTTP/1.1"),
            "sign_out should deregister the session token, got: {raw}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains("authorization: bearer tauri-jwt"),
            "the deregister must use the pre-clear JWT, got: {raw}"
        );
    }

    // ─────────── plugin config (Track A2) ───────────

    /// A2 precedence: with a populated plugins.cairn config, connect() takes
    /// no args — url/table/dbPath all fall through from the config block, and
    /// the session table is honored by write()'s table guard.
    #[tokio::test]
    async fn connect_falls_back_to_plugin_config() {
        let config = NostosPluginConfig {
            sync_url: Some("ws://localhost:0/sync".into()),
            token: None,
            table: Some("notes".into()),
            db_path: Some(":memory:".into()),
            ..NostosPluginConfig::default()
        };
        let state = NostosState::with_config(config);
        state.connect(None, None, None).await.expect("connect");

        // The session table came from config — "notes" writes, "tasks"
        // (the old hard-coded default) mismatches.
        state
            .write("notes".into(), "upsert".into(), "n1".into(), None)
            .await
            .expect("write to the config table");
        let err = state
            .write("tasks".into(), "upsert".into(), "t1".into(), None)
            .await
            .expect_err("tasks should no longer be the session table");
        assert!(
            err.contains("is not in the session tables"),
            "expected a table-mismatch error, got: {err}"
        );
    }

    /// A2 floor: no args AND no config syncUrl is the one hard error — every
    /// other field has a default, but the SDK refuses to guess an endpoint.
    #[tokio::test]
    async fn connect_without_url_or_config_is_an_error() {
        let state = NostosState::new();
        let err = state
            .connect(None, None, None)
            .await
            .expect_err("connect with no url anywhere should error");
        assert!(
            err.contains("no url") && err.contains("syncUrl"),
            "expected the missing-URL error naming the config key, got: {err}"
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
    const PUSH_BODY: &str =
        r#"{"pk":"tauri-push","payload":{"title":"from-server","status":"open","priority":"5"}}"#;

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
        let db_path =
            std::env::temp_dir().join(format!("nostos-tauri-e2e-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        let state = NostosState::new();
        let url = format!("ws://127.0.0.1:{port}/sync");
        state
            .connect(
                Some(url),
                None,
                Some(db_path.to_str().expect("utf8 db path").to_owned()),
            )
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
                Some(r#"{"title":"from-client","status":"open","priority":"5"}"#.into()),
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
        let sql = format!("SELECT pk FROM cairn_data WHERE table_name = 'tasks' AND pk = '{pk}'");
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

    // ---------------------------------------- unified verbs (Track A1)

    /// plugins.cairn parses the CRDT table declarations camelCase (the
    /// guest's spelling) and feeds them into the client config at connect.
    #[test]
    fn plugin_config_parses_crdt_tables_camel_case() {
        let raw = serde_json::json!({
            "syncUrl": "ws://localhost:9/sync",
            "orSetTables": ["tags"],
            "counterTables": ["scores"],
        });
        let config: NostosPluginConfig = serde_json::from_value(raw).expect("parse");
        assert_eq!(config.or_set_tables, Some(vec!["tags".to_owned()]));
        assert_eq!(config.counter_tables, Some(vec!["scores".to_owned()]));
        // Unknown keys still fail loudly (deny_unknown_fields discipline).
        assert!(
            serde_json::from_value::<NostosPluginConfig>(serde_json::json!({
                "orsettables": [],
            }))
            .is_err()
        );
    }

    /// The CRDT verbs + observability commands fail honestly before
    /// connect() — the same message shape write() uses.
    #[tokio::test]
    async fn unified_verbs_before_connect_fail_honestly() {
        let state = NostosState::new();
        let err = state
            .or_set_add("tasks".into(), "t1".into(), "x".into())
            .await
            .expect_err("no session");
        assert!(err.contains("before connect()"), "names the fix: {err}");
        let err = state.write_queue_status().await.expect_err("no session");
        assert!(err.contains("before connect()"), "names the fix: {err}");
        let err = state.is_subscribed().await.expect_err("no session");
        assert!(err.contains("before connect()"), "names the fix: {err}");
    }

    /// Offline CRDT round-trip: a tagged table's orSetAdd enqueues a
    /// durable merge-upsert (the outbox id returns; pending ticks up in
    /// the ADR-0027 status), and an UNTAGGED table is refused by the
    /// client gate with the three-views-of-one-truth error. The table
    /// membership guard fires first: a CRDT table must also be in
    /// `plugins.cairn.tables`.
    #[tokio::test]
    async fn crdt_verbs_offline_round_trip_and_gate() {
        let config = NostosPluginConfig {
            or_set_tables: Some(vec!["tasks".to_owned()]),
            counter_tables: Some(vec!["tasks".to_owned()]),
            ..NostosPluginConfig::default()
        };
        let state = NostosState::with_config(config);
        state
            .connect(
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect("connect");

        let id = state
            .or_set_add("tasks".into(), "t1".into(), "hello".into())
            .await
            .expect("tagged table enqueues");
        assert!(id > 0, "outbox ids start at 1");
        let cid = state
            .counter_increment("tasks".into(), "c1".into(), 3)
            .await
            .expect("tagged counter enqueues");
        assert!(cid > 0);

        let status = state.write_queue_status().await.expect("status");
        assert!(status.pending >= 2, "both CRDT writes sit in the outbox");
        assert_eq!(status.dead_lettered, 0, "nothing has been rejected yet");
        assert_eq!(status.last_error, None);
        assert!(!state.is_subscribed().await.expect("subscribed"));

        // The gate: a session WITHOUT the table tag refuses the verb
        // client-side (the three-views rule) before any outbox entry exists.
        // The table-membership guard fires first for OTHER tables, so this
        // needs its own state whose session table is simply untagged.
        let untagged = NostosState::new();
        untagged
            .connect(
                Some("ws://localhost:0".into()),
                None,
                Some(":memory:".into()),
            )
            .await
            .expect("connect");
        let err = untagged
            .or_set_add("tasks".into(), "n1".into(), "x".into())
            .await
            .expect_err("untagged table");
        assert!(
            err.to_lowercase().contains("tagged"),
            "names the three-views rule: {err}"
        );
    }
    /// Multi-table lift (2026-09-21): `plugins.cairn.tables` feeds
    /// `SyncClient.extra_tables`; every listed table passes the guard, an
    /// unlisted one is refused naming the set; `tables` wins over `table`.
    #[tokio::test]
    async fn plugin_config_tables_lifts_the_one_table_ceiling() {
        let config: NostosPluginConfig = serde_json::from_value(serde_json::json!({
            "syncUrl": "ws://localhost:0/sync",
            "dbPath": ":memory:",
            "table": "ignored",
            "tables": ["tasks", "notes"]
        }))
        .expect("camelCase block parses");
        assert_eq!(
            config.tables(),
            vec!["tasks".to_owned(), "notes".to_owned()]
        );
        let state = NostosState::with_config(config);
        state.connect(None, None, None).await.expect("connect");
        for t in ["tasks", "notes"] {
            state
                .write(t.into(), "upsert".into(), "pk".into(), None)
                .await
                .unwrap_or_else(|e| panic!("write to configured table {t}: {e}"));
        }
        state
            .subscribe("notes".into())
            .await
            .expect("subscribe any configured table");
        let err = state
            .write("ignored".into(), "upsert".into(), "x".into(), None)
            .await
            .expect_err("`table` is superseded by `tables`");
        assert!(
            err.contains("is not in the session tables [\"tasks\", \"notes\"]"),
            "{err}"
        );
        let err = state.subscribe("other".into()).await.expect_err("unlisted");
        assert!(
            err.starts_with("subscribe() table \"other\" is not in the session tables"),
            "{err}"
        );
    }
}
