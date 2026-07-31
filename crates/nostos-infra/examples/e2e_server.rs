//! # e2e_server — the SDK live-E2E spine.
//!
//! A connectable no-docker server binary that serves the REAL Nostos WS sync
//! contract (the production `sync_handler`), backed by an in-process injection
//! channel (no Postgres, no docker, no network egress — `127.0.0.1` only). An
//! echo `WriteBack` re-emits every accepted client write through
//! `FanOutService` so the writer receives its own write over the same WS
//! session — the 2-way round-trip every SDK E2E proves against this binary.
//!
//! ## Discovery (the contract with every SDK E2E harness)
//! - Binds `127.0.0.1:0`, prints `NOSTOS_E2E_PORT=<port>` then `NOSTOS_E2E_READY`
//!   to stdout and flushes. SDK tests read these lines to learn the port.
//! - `GET /sync` (WS upgrade) is the production handler — subscribe, ack, write
//!   all flow through the real wire codec (`nostos_infra::wire`).
//! - `POST /push` with JSON `{"pk":"...","payload":{...}}` injects one `tasks`
//!   row that flows to every live subscriber via the real `FanOutService`.
//!
//! ## Wire shapes (what an SDK client sends / receives)
//! - **Subscribe** (client → server, first frame): `{"type":"subscribe",
//!   "table":"tasks"}` — optional `where_sql`/`resume_lsn`/`filters` fields.
//! - **Write** (client → server): `{"type":"write","table":"tasks","op":
//!   "upsert","pk":"<id>","payload":{...},"client_write_id":"<id>"}`.
//! - **WireFrame** (server → client, replicated row):
//!   `{"lsn":<u64>,"op":"insert","table":"tasks","pk":"<id>","payload":
//!   "<hex-bytes>"}` (server may batch as `[{...},{...}]`).
//! - **WriteResult** (server → client, write ack):
//!   `{"type":"write_result","client_write_id":"<id>","ok":true}`.
//!
//! See the "spine" section of `docs/plans/sdk-live-e2e-consolidation.md`.

// Presentation/formatting lints trip on incidental shape in a throwaway dev
// fixture (uninlined format args, etc.) — mirroring nostos-bench's allow for
// the same reason.
#![allow(clippy::uninlined_format_args)]

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

use nostos_application::ports::{SessionStore, SyncAuth, WriteBack, WriteBackError};
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp, Tier};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::AllowAnonymous;

/// mpsc channel carrying replication events to the fan-out pump. Both `/push`
/// and the echo `WriteBack` produce into it; the background pump drains it and
/// fans every event out to live subscribers through the real `FanOutService`.
type EventTx = mpsc::UnboundedSender<ReplicationEvent>;

/// The echo `WriteBack` (lifted from `reactive_scroll`'s `RecordingWriteBack`):
/// every accepted client write is re-emitted as a `ReplicationEvent` through
/// the shared channel, so the pump feeds it back to subscribers — including
/// the writer — over the same replication path. This stands in for
/// `PgWriteBack → Postgres → logical replication` without a database.
struct EchoWriteBack {
    tx: EventTx,
    /// Write-echo LSNs live above the `/push` LSN space (which starts at 100)
    /// so the two streams never collide.
    next_lsn: Arc<AtomicU64>,
}

impl EchoWriteBack {
    fn new(tx: EventTx) -> Self {
        Self {
            tx,
            next_lsn: Arc::new(AtomicU64::new(1_000_000)),
        }
    }

    /// Forward one event to the fan-out pump. Fire-and-forget: the pump holds
    /// the receiver live for the lifetime of the server; a send error means
    /// the pump is gone, which only happens during shutdown — the write
    /// already semantically succeeded, so it isn't surfaced as a WriteBackError.
    fn emit(&self, ev: ReplicationEvent) {
        let _ = self.tx.send(ev);
    }
}

#[async_trait]
impl WriteBack for EchoWriteBack {
    async fn upsert(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::copy_from_slice(payload_json.as_bytes()),
            },
        );
        self.emit(ev);
        Ok(())
    }

    async fn delete(
        &self,
        table: &str,
        pk: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Delete {
                table: table.to_string(),
                pk: pk.to_string(),
                old_payload: None,
            },
        );
        self.emit(ev);
        Ok(())
    }

    async fn patch(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        // P3 PowerSync PATCH parity: record the patch as an Update carrying
        // the partial tuple image.
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Update {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::copy_from_slice(payload_json.as_bytes()),
            },
        );
        self.emit(ev);
        Ok(())
    }

    async fn increment(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        _tenant: Option<nostos_domain::TenantScope<'_>>,
    ) -> Result<(), WriteBackError> {
        // ADR-0030 Decision 1: the real PgWriteBack applies col = col + delta
        // and replicates the full new row; this echo mock forwards the delta-op
        // as an Update carrying the {field,delta} payload (no row state in the
        // mock — plumbing round-trip only).
        let lsn = self.next_lsn.fetch_add(10, Ordering::Relaxed);
        let ev = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Update {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::copy_from_slice(payload_json.as_bytes()),
            },
        );
        self.emit(ev);
        Ok(())
    }
}

/// The `/push` request body: inject one `tasks` row with the given primary key
/// and JSON payload. The payload is JSON-serialized to bytes and carried as the
/// row's tuple image — the same shape `PgReplicator::tuple_to_json_payload`
/// emits and the FanOutService's column extractor parses for predicates.
#[derive(Debug, Deserialize)]
struct PushBody {
    pk: String,
    payload: serde_json::Value,
}

/// Shared state for the `/push` handler: the injection channel + a per-event
/// LSN counter (so each injected row carries a unique, monotonically
/// increasing LSN — FanOutService requires monotonicity).
#[derive(Clone)]
struct PushCtx {
    tx: EventTx,
    next_lsn: Arc<AtomicU64>,
}

/// `POST /push`: build one `tasks` Insert event and forward it to the fan-out
/// pump. The next live subscriber receives it via the real `FanOutService` —
/// identical in shape to a Postgres WAL event.
async fn push_handler(
    State(ctx): State<PushCtx>,
    Json(body): Json<PushBody>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let payload_bytes = serde_json::to_vec(&body.payload)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("payload encode: {e}")))?;
    let lsn = ctx.next_lsn.fetch_add(10, Ordering::Relaxed);
    let ev = ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".to_string(),
            pk: body.pk,
            payload: Bytes::from(payload_bytes),
        },
    );
    let _ = ctx.tx.send(ev);
    Ok(StatusCode::OK)
}

/// Column extractor for the `FanOutService` — the production
/// `extract_json_column` shape (parse the payload JSON once per call, look up
/// the requested column, return it as a `ColumnValue::text`). Lifted verbatim
/// from `reactive_scroll` so predicates (`where_sql`) work end-to-end against
/// injected rows. SDK clients that subscribe WITHOUT a `where_sql` never
/// trigger this path; SDK clients that DO supply one get the same
/// typed-comparison engine a real Pg-backed server provides.
fn extract_json(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    let map = parse_flat_json(s)?;
    map.get(col).map(ColumnValue::text)
}

/// Minimal flat-JSON-object parser for `{"k":"v",...}` — lifted verbatim from
/// `reactive_scroll`. Production uses `serde_json::Value` (extract_json_column
/// in nostos-infra); this avoids building the per-event Value tree.
fn parse_flat_json(s: &str) -> Option<HashMap<String, String>> {
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    let mut map = HashMap::new();
    for pair in s.split(',') {
        let mut kv = pair.splitn(2, ':');
        let k = kv.next()?.trim().trim_matches('"');
        let v = kv.next()?.trim().trim_matches('"');
        map.insert(k.to_string(), v.to_string());
    }
    Some(map)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,nostos_infra=info")),
        )
        .try_init()
        .ok();

    // ---- shared store + auth + session manager (the real production wiring) ----
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let manager = Arc::new(SessionManager::new(Arc::clone(&store), Tier::Enterprise));

    // The injection channel: `/push` and `EchoWriteBack` produce, the pump
    // consumes. Unbounded so a burst of writes never blocks the WriteBack path
    // (a slow fan-out must never reject a client write).
    let (tx, mut rx) = mpsc::unbounded_channel::<ReplicationEvent>();
    let echo_wb: Arc<dyn WriteBack> = Arc::new(EchoWriteBack::new(tx.clone()));
    let push_ctx = PushCtx {
        tx,
        next_lsn: Arc::new(AtomicU64::new(100)),
    };

    // ---- router: real /sync + control /push ----
    // Each sub-router bakes in its own state type (SyncRouterState vs PushCtx)
    // via `.with_state(...)`, producing two `Router<()>` instances that merge
    // cleanly — the production `sync_handler` requires `State<SyncRouterState>`
    // exactly, so we cannot collapse them into one shared AppState.
    let mut tables = HashSet::new();
    tables.insert("tasks".to_string());
    let sync_state = SyncRouterState::new(Arc::clone(&manager), Arc::clone(&auth))
        .with_buffer(1024)
        .with_write_back(echo_wb)
        .with_write_tables(tables);
    let sync_router = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(sync_state);
    let push_router = axum::Router::new()
        .route("/push", post(push_handler))
        .with_state(push_ctx);
    let app = axum::Router::new().merge(sync_router).merge(push_router);

    // ---- bind 127.0.0.1:0 + announce to stdout ----
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    println!("NOSTOS_E2E_PORT={}", addr.port());
    println!("NOSTOS_E2E_READY");
    let _ = std::io::stdout().flush();

    // ---- fan-out pump: drain the injection channel, fan each event out ----
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));
    let pump = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let _ = fanout.fan_out(&ev, extract_json).await;
        }
    });

    // ---- serve with graceful shutdown on Ctrl-C / SIGTERM ----
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sig_term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => info!("ctrl-c received, shutting down"),
            _ = sig_term.recv() => info!("SIGTERM received, shutting down"),
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    // Drop the pump: closes the channel sender clones (EchoWriteBack's + the
    // PushCtx's) are still alive in `app`, but once `axum::serve` returns the
    // routers are dropped too, dropping the last senders, so `rx.recv()`
    // returns None and the pump task ends.
    pump.abort();
    // ponytail:fixed sleep instead of a join-with-timeout — a dev fixture can
    // afford the 50ms of cleanliness; production would use `JoinSet`.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}
