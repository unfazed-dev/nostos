//! WebSocket transport adapter — axum route that authenticates a connection,
//! upgrades it, spawns a session, drains its bounded sink onto the wire, and
//! reads client ACKs concurrently.
//!
//! Flow on a new connection:
//! 1. **Authenticate** the bearer token (Authorization header OR `?token=` —
//!    browsers can't set headers on a WS handshake) via the `SyncAuth` port.
//!    Reject with HTTP 401 before upgrade on failure (ADR-0010). This closes
//!    the data-exfiltration hole the unauthenticated `/sync` had.
//! 2. Read the first frame as a `ClientMessage::Subscribe { table, filters,
//!    resume_lsn }`.
//! 3. **Inject the tenant filter** from the principal into the predicate (the
//!    client's own filters are intersected, never allowed to widen scope —
//!    ADR-0011). Anonymous principals get no injection.
//! 4. Allocate a `TokioEventSink` (bounded channel); seed its ack cursor from
//!    `resume_lsn` if present.
//! 5. Register the authenticated session with the `SessionManager`.
//! 6. **Split** the socket: a writer task drains the sink onto the wire; a
//!    reader task parses `ClientMessage::Ack` frames and stamps the sink's ack
//!    cursor (driving the ack-driven slot advance, ADR-0009).
//! 7. On disconnect, close the sink + unregister.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::stream::StreamExt as _;
use serde::Deserialize;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};

use nostos_application::ports::{
    EventSink, SchemaSource, SnapshotSource, SyncAuth, WriteBack, WriteBackError,
};
use nostos_application::SessionManager;
use nostos_domain::{ColumnValue, Predicate, Principal, ReplicationEvent, SessionId, SyncSession};

use crate::router::TokioEventSink;
use crate::wire::{
    decode_client_message, encode_event, encode_events, encode_snapshot_boundary,
    encode_write_result, ClientMessage,
};

/// Default per-session bounded-buffer depth. Slow clients that fall this far
/// behind are dropped (an explicit, observable choice — never silent OOM).
const DEFAULT_SESSION_BUFFER: usize = 1024;

/// Max frames coalesced into one WebSocket message under backlog (C3
/// batched-writes). The write task drains up to this many *immediately
/// available* frames after the first; the first always arrives via an
/// `await`, so there is **zero latency tax at low rates** — batching only
/// kicks in when the channel already has a backlog (≥2 pending frames). The
/// receiver decodes both the batched array and the legacy single-object form,
/// so no wire-version bump is needed.
const MAX_BATCH_FRAMES: usize = 64;

/// Per-socket table-subscription cap (D1/ADR-0022). Bounds snapshot-on-
/// subscribe cost (each subscribe triggers a full-table SELECT in
/// `PgSnapshotter`) so one client cannot DoS the snapshotter by subscribing to
/// thousands of tables on one socket. A `Subscribe` beyond this cap is
/// rejected (non-fatal — the socket keeps serving its existing subscriptions);
/// 32 is generous for real apps (the provider dashboard uses 5) and small
/// enough that 32 × device_cap snapshots is a bounded worst case.
const MAX_TABLES_PER_SOCKET: usize = 32;

/// Shared state injected into the axum router.
#[derive(Clone)]
pub struct SyncRouterState {
    pub manager: Arc<SessionManager>,
    pub session_buffer: usize,
    pub auth: Arc<dyn SyncAuth>,
    /// When set, every predicate is AND-constrained to `tenant_column =
    /// principal.tenant_id` (server-enforced, never client-attested). `None`
    /// means no tenant enforcement (single-tenant / anonymous deploys).
    pub tenant_column: Option<String>,
    /// The write-back port (ADR-0013). Defaults to [`NoWriteBack`] (the
    /// fake-mode stub that refuses every call); the composition root injects
    /// `PgWriteBack` under `NOSTOS_REPLICATOR=pg` with feature `pg`.
    pub write_back: Arc<dyn WriteBack>,
    /// The set of tables clients may write to (ADR-0013). Enforced by the
    /// transport FIRST — before the adapter is called — so the allowlist is a
    /// single trust-boundary check that holds regardless of which adapter is
    /// injected (the `PgWriteBack` adapter re-validates it as
    /// defense-in-depth). Empty = no tables writable. Defaults empty.
    pub write_tables: Arc<HashSet<String>>,
    /// The snapshot-on-subscribe port (ADR-0014). When set, a freshly-
    /// subscribing session receives the table's pre-existing rows as `Insert`
    /// events BEFORE live fan-out. `None` means snapshot-on-subscribe is off
    /// (the `FakeReplicator` path, or a binary built without feature `pg`).
    /// The composition root injects `PgSnapshotter` under
    /// `NOSTOS_REPLICATOR=pg`.
    pub snapshotter: Option<Arc<dyn SnapshotSource>>,
    /// The typed-schema port (WS1). When set, `GET /schema` serves the
    /// publication's tables/columns/affinities for client auto-schema. `None`
    /// means the endpoint returns 404 (the `FakeReplicator` path, or a binary
    /// built without feature `pg`). Injected under `NOSTOS_REPLICATOR=pg`.
    pub schema_source: Option<Arc<dyn SchemaSource>>,
}

impl SyncRouterState {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, auth: Arc<dyn SyncAuth>) -> Self {
        Self {
            manager,
            session_buffer: DEFAULT_SESSION_BUFFER,
            auth,
            tenant_column: None,
            write_back: Arc::new(crate::write_back::NoWriteBack::new()),
            write_tables: Arc::new(HashSet::new()),
            snapshotter: None,
            schema_source: None,
        }
    }

    /// Set the per-session bounded buffer depth.
    #[must_use]
    pub fn with_buffer(mut self, buffer: usize) -> Self {
        self.session_buffer = buffer.max(1);
        self
    }

    /// Set the tenant column used to inject server-enforced predicates.
    #[must_use]
    pub fn with_tenant_column(mut self, column: impl Into<String>) -> Self {
        self.tenant_column = Some(column.into());
        self
    }

    /// Inject the write-back adapter (ADR-0013). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgWriteBack`; otherwise the default
    /// `NoWriteBack` stub surfaces a clear "write-back requires pg replicator"
    /// error to any client attempting a write.
    #[must_use]
    pub fn with_write_back(mut self, wb: Arc<dyn WriteBack>) -> Self {
        self.write_back = wb;
        self
    }

    /// Set the writable-table allowlist (ADR-0013). Enforced by the transport
    /// before the adapter is called. Build from `NOSTOS_WRITE_TABLES` via
    /// [`crate::parse_allowlist`].
    #[must_use]
    pub fn with_write_tables(mut self, tables: HashSet<String>) -> Self {
        self.write_tables = Arc::new(tables);
        self
    }

    /// Inject the snapshot-on-subscribe adapter (ADR-0014). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgSnapshotter`; otherwise leave it `None`
    /// (the default) so subscribe-time snapshots are skipped and clients rely
    /// on live fan-out alone.
    #[must_use]
    pub fn with_snapshotter(mut self, snap: Arc<dyn SnapshotSource>) -> Self {
        self.snapshotter = Some(snap);
        self
    }

    /// Inject the typed-schema adapter (WS1). Call under `NOSTOS_REPLICATOR=pg`
    /// with a `PgSchemaSource`; otherwise leave it `None` (the default) so
    /// `GET /schema` returns 404.
    #[must_use]
    pub fn with_schema_source(mut self, src: Arc<dyn SchemaSource>) -> Self {
        self.schema_source = Some(src);
        self
    }
}

/// Query-string auth fallback — browsers can't set Authorization on a WS
/// handshake, so `?token=` is the supported path for web clients.
#[derive(Debug, Deserialize)]
pub struct AuthQuery {
    #[serde(default)]
    pub token: Option<String>,
}

/// Axum handler: `GET /sync` → authenticate → WebSocket upgrade.
///
/// Reads the bearer token from the `Authorization` header or `?token=` query
/// param, resolves it to a [`Principal`] via [`SyncAuth`], and rejects with
/// HTTP 401 (no upgrade) on failure. The principal is threaded into the
/// upgraded session so predicates can be server-enforced.
///
/// Marked `async` because axum's `Handler` trait requires it; the body doesn't
/// await before upgrade (the auth check is synchronous-ish), so the lint is
/// allowed.
#[allow(clippy::unused_async)]
pub async fn sync_handler(
    ws: WebSocketUpgrade,
    State(state): State<SyncRouterState>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
) -> Response {
    // Token from header OR `?token=` (browsers can't set WS handshake headers).
    // An empty/missing token is passed to the adapter as "" — `AllowAnonymous`
    // accepts it (returns the anonymous principal), real verifiers reject it.
    let token = bearer_token(&headers).or(query.token).unwrap_or_default();
    let principal = state.auth.authenticate(&token).await;
    let Some(principal) = principal else {
        return unauthorized();
    };
    ws.on_upgrade(move |socket| run_session(socket, state, principal))
}

/// Pull a bearer token off the Authorization header (`Bearer <token>`).
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    let t = s
        .strip_prefix("Bearer ")
        .or_else(|| s.strip_prefix("bearer "))?;
    Some(t.to_string())
}

fn unauthorized() -> Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        "nostos: authentication required for /sync",
    )
        .into_response()
}

/// Drive one WebSocket connection for its lifetime.
async fn run_session(mut socket: WebSocket, state: SyncRouterState, principal: Principal) {
    // 1. Read the subscribe frame.
    let Some(subscribe) = read_subscribe(&mut socket).await else {
        return; // client disconnected without subscribing
    };

    // 2. Allocate the ONE shared sink for this socket. N tables deliver into
    //    this one bounded channel; a single writer task drains it onto the wire.
    //    We keep the *concrete* `Arc<TokioEventSink>` for close()/record_ack()
    //    + snapshot delivery; `register_subscribe` derives the type-erased
    //    `Arc<dyn EventSink>` clone the store holds per registered session.
    let (sink, mut rx) = TokioEventSink::channel(state.session_buffer);
    let sink_concrete = Arc::new(sink);

    // 3. Seed the resume cursor ONCE from the first subscribe (the client's
    //    global checkpoint). The socket's `synthetic_cursor` (the snapshot LSN
    //    allocator) derives from the same value; per-frame resume_lsn on later
    //    subscribes is ignored, so a mid-stream snapshot can't be dropped past
    //    an already-advanced checkpoint.
    if let Some(resume) = subscribe.resume_lsn {
        sink_concrete.seed_acked_lsn(nostos_domain::Lsn::new(resume));
        debug!(
            resume_lsn = resume,
            "session resuming from client checkpoint"
        );
    }

    // Clone the principal + tenant for BOTH the write path and the read-side
    // subscribe path (ADR-0018): the read path (predicate injection) and the
    // write path (tenant-scoped stamping/guards) share one authenticated
    // identity. `principal` is borrowed for the first register_subscribe
    // below; the clones live on in the reader task.
    let write_principal = principal.clone();
    let tenant_column_for_writes = state.tenant_column.clone();

    let manager = Arc::clone(&state.manager);
    let snapshotter = state.snapshotter.clone();

    // 4. Per-socket multi-table state. `synthetic_cursor` is seeded from the
    //    first subscribe's resume_lsn (0 for a fresh client) and advanced by
    //    each snapshot's row count — the load-bearing fix that keeps multi-
    //    table snapshot-on-subscribe correct on a shared sink (see
    //    `register_subscribe` + ADR-0022).
    let subs = Arc::new(Mutex::new(SocketSubs {
        ids: Vec::new(),
        tables: HashSet::new(),
        synthetic_cursor: subscribe.resume_lsn.unwrap_or(0),
    }));

    // Pre-encoded control frame channel (write_result acks + snapshot-reconcile
    // boundaries — ADR-0013 v2 + ADR-0014). Created BEFORE the first
    // `register_subscribe` so the first-table snapshot can emit a begin/end
    // pair through it; the writer task (split below) drains the rx half.
    let (server_frames_tx, mut server_frames_rx) =
        tokio::sync::mpsc::channel::<Vec<u8>>(DEFAULT_SESSION_BUFFER);

    // 5. Register the FIRST table. A where_sql rejection or the global device
    //    cap is FATAL here (close the socket with a reason before any event
    //    flows, same as the single-table path); subsequent rejects are
    //    non-fatal (the reader logs + keeps serving existing subscriptions).
    if let Err(reject) = register_subscribe(
        &subscribe,
        &subs,
        &manager,
        snapshotter.as_ref(),
        &sink_concrete,
        &principal,
        state.tenant_column.as_deref(),
    )
    .await
    {
        match reject {
            SubscribeReject::WhereSqlRejected(reason) => {
                debug!(%reason, "closing socket: first subscribe where_sql rejected");
                let frame = axum::extract::ws::CloseFrame {
                    code: axum::extract::ws::close_code::INVALID,
                    reason: reason.into(),
                };
                let _ = socket
                    .send(axum::extract::ws::Message::Close(Some(frame)))
                    .await;
                return;
            }
            // Device cap or (impossible here) per-socket cap: close cleanly.
            SubscribeReject::DeviceCapReached | SubscribeReject::CapExceeded => {
                let _ = socket.close().await;
                return;
            }
        }
    }

    // 6. Split the socket: writer drains the shared sink, reader parses ACK/
    //    Write frames AND handles additional Subscribe frames (registering more
    //    tables on the SAME sink). Same single-writer serialization as before:
    //    events AND write-acks share one socket sink, no interleaving race.
    let (writer, mut reader) = socket.split();

    let closed = Arc::new(Notify::new());
    let closed_tx = Arc::clone(&closed);
    let ack_sink = Arc::clone(&sink_concrete);

    let write_loop = tokio::spawn(async move {
        use futures_util::sink::SinkExt as _;
        let mut writer = writer;
        // C3 batched-writes: the first frame is awaited (no busy-spin, no
        // latency tax when idle). Once one is in hand, drain up to
        // `MAX_BATCH_FRAMES - 1` MORE frames that are *immediately available*
        // (non-blocking `try_recv`). If only the awaited frame is available,
        // send it as a single object — byte-identical to the pre-batching wire
        // (so the low-rate path adds zero cost and stays wire-compatible with
        // any client that only understands single-object messages). Only when
        // the channel has a backlog (≥2 pending) do we coalesce into one JSON
        // array message, amortizing N frame-encode + socket-send costs into
        // one wire write. The receiver decodes both forms (`decode_frames`).
        //
        // D2: the writer ALSO drains `server_frames_rx` (the reader's
        // WriteResult acks). We `select!` over both sources so neither starves
        // the other; a pending write-ack goes out promptly even under event
        // backlog (it's a single small frame, never batched with events —
        // `WriteResult` is its own wire shape, not a replication frame).
        loop {
            // Await the next thing to send: an event batch OR a write-ack frame.
            tokio::select! {
                // Replication events from the fan-out sink.
                maybe_first = rx.recv() => {
                    let Some(first) = maybe_first else { break; };
                    // The sink channel carries Events AND Control frames (snapshot
                    // boundaries) on one FIFO queue (ADR-0025 hole #2). A Control
                    // goes out immediately, alone (a different wire shape — can't
                    // batch with events); an Event starts a batch. Draining stops
                    // at a Control so it keeps its FIFO slot, sent right after the
                    // batch it followed — that is what guarantees begin → rows →
                    // end on the wire.
                    match first {
                        crate::router::SinkMsg::Control(bytes) => {
                            if writer.send(Message::Binary(bytes)).await.is_err() {
                                break; // client gone
                            }
                        }
                        crate::router::SinkMsg::Event(first_ev) => {
                            let mut batch: Vec<ReplicationEvent> =
                                Vec::with_capacity(MAX_BATCH_FRAMES);
                            batch.push(first_ev);
                            let mut pending_control: Option<Vec<u8>> = None;
                            while batch.len() < MAX_BATCH_FRAMES {
                                match rx.try_recv() {
                                    Ok(crate::router::SinkMsg::Event(ev)) => batch.push(ev),
                                    Ok(crate::router::SinkMsg::Control(bytes)) => {
                                        pending_control = Some(bytes);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let msg = if batch.len() == 1 {
                                Message::Binary(encode_event(&batch[0]))
                            } else {
                                let refs: Vec<&ReplicationEvent> = batch.iter().collect();
                                Message::Binary(encode_events(&refs))
                            };
                            if writer.send(msg).await.is_err() {
                                break; // client gone
                            }
                            if let Some(bytes) = pending_control {
                                if writer.send(Message::Binary(bytes)).await.is_err() {
                                    break; // client gone
                                }
                            }
                        }
                    }
                }
                // WriteResult acks from the reader task (D2). Never batched —
                // a write-ack is its own wire shape, sent immediately.
                maybe_ack = server_frames_rx.recv() => {
                    let Some(bytes) = maybe_ack else { break; };
                    if writer.send(Message::Binary(bytes)).await.is_err() {
                        break; // client gone
                    }
                }
            }
        }
        let _ = writer;
        closed_tx.notify_waiters();
    });

    // Reader: decode each inbound frame ONCE, then route:
    //   Subscribe → register_subscribe (register another table on the shared
    //     sink — multi-table-per-handle, D1/ADR-0022).
    //   Ack/Write → handle_decoded_message (ack cursor / allowlist + write-back).
    // A rejected mid-session subscribe (per-socket cap, where_sql, or global
    // device cap) is NON-fatal: warn and keep serving existing subscriptions.
    // The reader cannot cleanly force-close the writer half after split, and
    // reject-and-continue is already bounded — `register_subscribe` returns
    // before `connect` on cap-exceed, so no session registers past
    // MAX_TABLES_PER_SOCKET (≤32 × device_cap worst case). Architecture advisor
    // (HIGH, 2026-07-15) recommended close-on-cap; this deviates because the
    // no-leak property makes close's teardown wiring unjustified — ADR-0022.
    let write_back = Arc::clone(&state.write_back);
    let write_tables = Arc::clone(&state.write_tables);
    let subs_reader = Arc::clone(&subs);
    let manager_reader = Arc::clone(&manager);
    let read_loop = tokio::spawn(async move {
        while let Some(Ok(msg)) = reader.next().await {
            let data: Option<Vec<u8>> = match msg {
                Message::Text(t) => Some(t.into_bytes()),
                Message::Binary(b) => Some(b),
                Message::Close(_) => break,
                _ => None, // ping/pong
            };
            let Some(data) = data else { continue };
            match decode_client_message(&data) {
                Some(ClientMessage::Subscribe {
                    table,
                    filters,
                    where_sql,
                    resume_lsn,
                }) => {
                    let req = SubscribeRequest {
                        table,
                        filters,
                        where_sql,
                        resume_lsn,
                    };
                    if let Err(e) = register_subscribe(
                        &req,
                        &subs_reader,
                        &manager_reader,
                        snapshotter.as_ref(),
                        &ack_sink,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                    )
                    .await
                    {
                        warn!(reject = ?e, table = %req.table, "mid-session subscribe rejected; socket continues");
                    }
                }
                Some(other) => {
                    handle_decoded_message(
                        other,
                        &ack_sink,
                        &write_back,
                        &write_tables,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                        &server_frames_tx,
                    )
                    .await;
                }
                None => warn!("dropping malformed client message"),
            }
        }
    });

    // Keep the socket alive until the writer ends, then disconnect ALL sessions
    // registered on the shared sink (one per subscribed table) + close it.
    closed.notified().await;
    sink_concrete.close();
    let ids: Vec<SessionId> = {
        let mut s = subs.lock().await;
        std::mem::take(&mut s.ids)
    };
    for id in ids {
        manager.disconnect(id).await;
    }
    let _ = write_loop.await;
    // The reader may still be blocked on recv; abort it so the task reaps.
    read_loop.abort();
}

/// Why a subscribe was rejected. Non-fatal for mid-session subscribes (the
/// socket keeps serving its existing tables); FATAL for the first subscribe
/// (the socket is closed — see `run_session`).
#[derive(Debug)]
enum SubscribeReject {
    /// `where_sql` failed to compile (ADR-0012). Carries the reason string.
    WhereSqlRejected(String),
    /// Per-socket table cap exceeded (`MAX_TABLES_PER_SOCKET`) — DoS guard.
    CapExceeded,
    /// Global concurrent-device cap reached (`SessionManager`).
    DeviceCapReached,
}

/// Per-socket multi-table subscription state (D1/ADR-0022). One socket owns
/// ONE shared `TokioEventSink` (one channel, one `acked_lsn`, one writer task —
/// ADR-0009's single global checkpoint) and N single-predicate `SyncSession`s
/// registered against it. `candidates_for` is table-indexed, so each session
/// receives only its own table's events; `min_acked_lsn` folds the shared
/// sink's single `last_acked_lsn` across the N sessions (= the same value N
/// times = the socket's checkpoint).
///
/// `synthetic_cursor` is the load-bearing correctness fix for multi-table
/// snapshot-on-subscribe: `PgSnapshotter` stamps snapshot LSNs as `base+1+i`
/// PER TABLE (snapshot_source.rs), so on a shared sink table B's snapshot LSN
/// range collides with table A's and the sink's dedup ring (router.rs) drops it
/// as duplicates. The cursor is seeded from `resume_lsn` and advanced by each
/// snapshot's row count, so every event across all tables gets a distinct LSN.
struct SocketSubs {
    /// Every registered session id (one per subscribed table) — disconnected
    /// en masse when the socket closes.
    ids: Vec<SessionId>,
    /// Subscribed table names — drives the per-socket cap + idempotent repeat.
    tables: HashSet<String>,
    /// Monotonic snapshot-LSN allocator; passed as `base_lsn` to each snapshot.
    synthetic_cursor: u64,
}

/// Register one table subscription on the socket's shared sink: predicate
/// build, per-socket cap + idempotency checks, `SessionManager::connect`, and
/// snapshot-on-subscribe. Called for the first subscribe (pre-split, in
/// `run_session`) and every subsequent one (post-split, in the reader task).
/// Returns `Err` WITHOUT registering on any rejection. Critical sections on
/// `subs` are short and never span an `.await`; access is serialized anyway
/// (one reader task; the first subscribe runs before the reader is spawned).
async fn register_subscribe(
    req: &SubscribeRequest,
    subs: &Arc<Mutex<SocketSubs>>,
    manager: &Arc<SessionManager>,
    snapshotter: Option<&Arc<dyn SnapshotSource>>,
    sink_concrete: &Arc<TokioEventSink>,
    principal: &Principal,
    tenant_column: Option<&str>,
) -> Result<(), SubscribeReject> {
    // Cap + idempotent-repeat check (short lock, no await).
    {
        let s = subs.lock().await;
        if s.tables.contains(&req.table) {
            debug!(table = %req.table, "subscribe for already-subscribed table: no-op");
            return Ok(());
        }
        if s.tables.len() >= MAX_TABLES_PER_SOCKET {
            return Err(SubscribeReject::CapExceeded);
        }
    }

    let predicate = build_predicate(req, principal, tenant_column)
        .map_err(SubscribeReject::WhereSqlRejected)?;
    let session = SyncSession::new_authenticated(predicate, principal.clone());
    // Derive the type-erased clone the store holds; `sink_concrete` stays the
    // concrete handle for snapshot delivery below.
    let sink_dyn: Arc<dyn EventSink> = Arc::clone(sink_concrete) as Arc<dyn EventSink>;
    let id = manager
        .connect(session, sink_dyn)
        .await
        .map_err(|_| SubscribeReject::DeviceCapReached)?;

    // Snapshot-on-subscribe for THIS table only. base_lsn is the socket's
    // monotonic synthetic cursor (NOT the frame's resume_lsn) so cross-table
    // snapshot LSN ranges never collide on the shared sink's dedup ring. A
    // failed snapshot is non-fatal: the client still gets live fan-out.
    let snapshot_base = { subs.lock().await.synthetic_cursor };
    let delivered = if let Some(snap) = snapshotter {
        match snap
            .snapshot(&req.table, nostos_domain::Lsn::new(snapshot_base))
            .await
        {
            Ok(events) => {
                // Snapshot-reconcile boundary (ADR-0014 offline-delete fix):
                // bracket the snapshot's rows with begin/end control frames so
                // the client can reap local PKs absent from the snapshot (rows
                // hard-deleted server-side while the client was offline). The
                // boundaries travel the SAME FIFO channel as the rows
                // (`sink_concrete` → writer → WS) so the writer can't reorder
                // them relative to the rows (ADR-0025 hole #2: two channels let
                // the writer's `select!` land begin after early rows → those
                // rows never drain → reaped at end). A full sink buffer drops
                // the boundary (best-effort, like a write-ack): the client keeps
                // its stale rows but stays consistent (no partial reconcile).
                let _ = sink_concrete.deliver_control(encode_snapshot_boundary(&req.table, true));
                let count = events.len();
                for ev in events {
                    // ADR-0025 residual: backpressure-aware delivery so the
                    // snapshot is never truncated by sink backpressure (a
                    // dropped row would let `end` reap a pk the server still
                    // has). Live fan-out keeps best-effort `deliver`.
                    let _ = sink_concrete.deliver_awaiting(ev).await;
                }
                let _ = sink_concrete.deliver_control(encode_snapshot_boundary(&req.table, false));
                debug!(table = %req.table, count, "snapshot-on-subscribe delivered");
                count
            }
            Err(e) => {
                warn!(
                    table = %req.table, error = %e,
                    "snapshot-on-subscribe failed; continuing with live fan-out"
                );
                0
            }
        }
    } else {
        0
    };

    // Advance the cursor by the rows we just delivered + record the session.
    {
        let mut s = subs.lock().await;
        s.synthetic_cursor = s.synthetic_cursor.saturating_add(delivered as u64);
        s.ids.push(id);
        s.tables.insert(req.table.clone());
    }
    Ok(())
}

/// Apply a decoded inbound Ack/Write client message. `Subscribe` is routed by
/// the reader task to `register_subscribe` (this handler never sees it in the
/// current flow, but stays defensive). The Write body is the ADR-0013 trust
/// boundary: allowlist FIRST, then tenant-scoped dispatch to the write-back
/// port, then a `WriteResult` ack queued to the writer task. The write call is
/// tenant-scoped exactly like the read path (ADR-0018) —
/// `principal.tenant_scope(tenant_column)` is the same seam `build_predicate`
/// uses, so read/write enforcement can't drift.
async fn handle_decoded_message(
    msg: ClientMessage,
    sink: &TokioEventSink,
    write_back: &Arc<dyn WriteBack>,
    allowlist: &HashSet<String>,
    principal: &Principal,
    tenant_column: Option<&str>,
    server_frames_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    match msg {
        ClientMessage::Ack { lsn } => {
            sink.record_ack(nostos_domain::Lsn::new(lsn));
            debug!(ack_lsn = lsn, "client acknowledged progress");
        }
        ClientMessage::Write {
            table,
            op,
            pk,
            payload,
            client_write_id,
        } => {
            // ALLOWLIST FIRST (ADR-0013 trust boundary). The transport enforces
            // the table allowlist before the adapter is ever called, so this is
            // one uniform gate that holds regardless of adapter. The
            // `PgWriteBack` adapter re-validates it as defense-in-depth.
            if !allowlist.contains(&table) {
                // Actionable rejection (ADR-0013). The empty-default (no tables
                // writable) is deliberate — defense-in-depth at the SQL-injection
                // trust boundary — so name the table + the exact env var that
                // opens it, teaching the model instead of failing silently. The
                // `"table not writable"` prefix is asserted by ws_contract.rs.
                let msg = format!(
                    "table not writable: '{table}' — add it to NOSTOS_WRITE_TABLES \
                     (env, comma-separated; e.g. NOSTOS_WRITE_TABLES={table}). \
                     Empty by default = no tables writable (ADR-0013)."
                );
                let frame = encode_write_result(&client_write_id, false, Some(&msg));
                let _ = server_frames_tx.try_send(frame);
                debug!(table = %table, "write rejected: table not writable");
                return;
            }
            // ADR-0018: the tenant scope, if enforcement is active, travels
            // with the write so the adapter can force-stamp/guard by it —
            // never trust anything the client sent for the tenant column.
            let tenant = principal.tenant_scope(tenant_column);
            // Dispatch to the write-back port. The result is reported back to
            // the client as a WriteResult frame; the written row then flows
            // out through normal replication to every subscriber.
            let result =
                dispatch_write(write_back, &table, &op, &pk, payload.as_ref(), tenant).await;
            let (ok, error) = match result {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            };
            let frame = encode_write_result(&client_write_id, ok, error.as_deref());
            // If the channel is full (client disconnected / backpressure), the
            // ack is dropped — the writer loop will end on the next failed
            // send anyway. Best-effort; not fatal.
            let _ = server_frames_tx.try_send(frame);
            debug!(
                table = %table,
                op = %op,
                ok,
                "write applied (or rejected) — WriteResult queued"
            );
        }
        // Subscribe is routed by the reader to `register_subscribe`; reaching
        // here is impossible in the current flow, but stay defensive.
        ClientMessage::Subscribe { .. } => {
            debug!("subscribe reached decoded-message handler");
        }
    }
}

/// Translate a `Write` client message into a `WriteBack` port call. The op
/// string is `"upsert" | "delete" | "patch"`; anything else is an
/// `InvalidPayload`. The payload (a `serde_json::Value`) is rendered back to
/// JSON text for the upsert/patch paths (the port takes `&str`). `tenant`
/// (ADR-0018) is forwarded verbatim to the adapter — `dispatch_write` doesn't
/// interpret it, just relays the scope the caller already computed from the
/// principal.
async fn dispatch_write(
    write_back: &Arc<dyn WriteBack>,
    table: &str,
    op: &str,
    pk: &str,
    payload: Option<&serde_json::Value>,
    tenant: Option<nostos_domain::TenantScope<'_>>,
) -> Result<(), WriteBackError> {
    match op {
        "upsert" => {
            // The payload must be present and a JSON object for an upsert. A
            // missing/non-object payload is InvalidPayload. The adapter ALSO
            // validates the object-ness, but we catch it here too so the error
            // is surfaced uniformly.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for upsert".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.upsert(table, pk, &json, tenant).await
        }
        "patch" => {
            // A patch carries the partial column set (same object shape as an
            // upsert payload). Same object-ness guard as upsert — the adapter
            // re-validates too.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for patch".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.patch(table, pk, &json, tenant).await
        }
        "delete" => write_back.delete(table, pk, tenant).await,
        other => Err(WriteBackError::InvalidPayload(format!(
            "unknown op: {other} (expected 'upsert', 'delete', or 'patch')"
        ))),
    }
}

/// Build the server-enforced predicate from the client's subscribe + principal.
///
/// The client's filters are always intersected with the tenant filter when a
/// tenant column is configured AND the principal is authenticated — the client
/// cannot widen scope past its own tenant. A client that requests a *different*
/// tenant's value silently gets its own: the server **drops** any client filter
/// on the tenant column and injects the principal's real tenant value (so the
/// predicate is never the impossible `org=X AND org=Y`). Anonymous principals
/// get no injection (single-tenant dev mode).
///
/// The optional `where_sql` (ADR-0012 safe-SQL-subset compiler) is compiled and
/// ANDed in **before** the tenant clause — so the server-injected tenant scoping
/// wraps the client expression and a `where_sql` can never shed it. A parse
/// failure is returned as `Err(reason)`; the caller closes the socket with that
/// reason (prefixed `"invalid where_sql: "`) before any event flows.
///
/// The IF-to-enforce decision is [`Principal::tenant_scope`] — the same seam
/// the write path (`dispatch_write`, ADR-0018) calls, so the read and write
/// enforcement conditions cannot drift apart.
fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
) -> Result<Predicate, String> {
    let scope = principal.tenant_scope(tenant_column);

    // Start match-all, then fold in the client's own filters — EXCLUDING any on
    // the tenant column, which the server overrides with the principal's real
    // value (never client-attested). The `and_eq` combinator collapses the
    // initial match-all down to a bare `Eq` leaf, so a single-filter predicate
    // is structurally identical to the historical `Predicate::eq` form.
    let mut p = Predicate::all(&subscribe.table);
    for f in &subscribe.filters {
        if scope.is_some_and(|s| f.column == s.column) {
            continue; // server injects the real tenant value below
        }
        p = p.and_eq(&f.column, ColumnValue::text(&f.value));
    }

    // Compile the optional safe-SQL-subset expression (ADR-0012) and AND it in.
    // Done BEFORE tenant enforcement so the server-injected tenant clause wraps
    // the client expression — a where_sql can never widen scope past its tenant.
    if let Some(sql) = &subscribe.where_sql {
        match nostos_domain::parse_predicate_expr(sql) {
            // `Predicate` has no `and(PredicateExpr)` method (only `and_eq`),
            // so fold the parsed expression into the predicate's public `expr`
            // field via `PredicateExpr::and`. Keeps the table binding intact.
            Ok(expr) => {
                p = Predicate {
                    table: p.table,
                    expr: p.expr.and(expr),
                }
            }
            Err(e) => return Err(format!("invalid where_sql: {e}")),
        }
    }

    // Server-enforced tenant scoping (ADR-0011). Always injected for an
    // authenticated principal when a tenant column is configured. This stays
    // LAST so it wraps everything above (filters + where_sql).
    if let Some(s) = scope {
        p = p.and_eq(s.column, ColumnValue::text(s.value));
    }
    Ok(p)
}

/// The parsed first-frame subscribe request (internal shape; the wire type is
/// `ClientMessage::Subscribe`).
struct SubscribeRequest {
    table: String,
    filters: Vec<crate::wire::FilterClause>,
    /// Optional safe-SQL-subset expression — compiled in `build_predicate` and
    /// ANDed in BEFORE tenant enforcement (so it can never widen scope).
    where_sql: Option<String>,
    resume_lsn: Option<u64>,
}

/// Await the first frame, require it to be a `ClientMessage::Subscribe`, and
/// return its fields. Returns `None` if the client closes or sends a
/// non-subscribe first frame.
///
/// A leading `Write` (ADR-0013) or `Ack` is out of order — the session must
/// subscribe first so its predicate is registered before any event (or write
/// result) flows. Same discipline as an early ACK: the socket is closed
/// (caller drops it). A `ping`/`pong` is skipped, keeping the handshake alive.
async fn read_subscribe(socket: &mut WebSocket) -> Option<SubscribeRequest> {
    while let Some(Ok(msg)) = socket.recv().await {
        // Collect into owned bytes so the borrow outlives the match arms.
        let data: Vec<u8> = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Close(_) => return None,
            _ => continue, // ping/pong — keep waiting for the subscribe
        };
        return match decode_client_message(&data)? {
            ClientMessage::Subscribe {
                table,
                filters,
                where_sql,
                resume_lsn,
            } => Some(SubscribeRequest {
                table,
                filters,
                where_sql,
                resume_lsn,
            }),
            // An ACK or a Write before subscribing is out of order — reject by
            // closing the socket (same discipline as early ACK). The caller
            // returns from run_session, dropping the connection.
            ClientMessage::Ack { .. } | ClientMessage::Write { .. } => None,
        };
    }
    None
}

// (Transport-swap seam removed — axum 0.7's `WebSocket` works directly. If we
// later swap to WebTransport, this module is the single place that changes.)
