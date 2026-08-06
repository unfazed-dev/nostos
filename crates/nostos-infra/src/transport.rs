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
    EventSink, Metrics, OpLogSource, SchemaSource, SnapshotSource, SyncAuth, WriteBack,
    WriteBackError,
};
use nostos_application::{ActiveRuleset, RuleDecision, SessionManager};
use nostos_domain::{
    compose_sync_epoch, ColumnValue, Predicate, Principal, ReplicationEvent, SessionId, SyncMode,
    SyncSession,
};

use crate::router::TokioEventSink;
use crate::wire::{
    decode_client_message, encode_event, encode_events, encode_resume_info,
    encode_snapshot_boundary, encode_write_result, ClientMessage,
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

/// Close reason a live session receives when a sync-rules reload (ADR-0031
/// D3) changes the rule decision for one of its subscribed tables. Swap
/// verification is per-table and coarse — see the `ponytail:` at the
/// `run_session` call site — so this fires on a real narrowing AND on any
/// widen the verification can't prove safe in place; either way the client's
/// reconnect (Task 11's checksum/epoch path) re-scopes it into the current
/// ruleset.
pub(crate) const RULES_CHANGED_CLOSE_REASON: &str = "rules changed; reconnect to re-scope";

/// Shared state injected into the axum router.
#[derive(Clone)]
pub struct SyncRouterState {
    pub manager: Arc<SessionManager>,
    pub session_buffer: usize,
    /// The server-wide metrics handle (ADR-0025 slice 4b). Read for
    /// `slot_epoch` — the reconnect-resume gate compares the client's epoch to
    /// it. Defaults to a throwaway `Metrics::new()` in [`Self::new`]; the
    /// composition root injects the real shared handle via [`Self::with_metrics`].
    pub metrics: Arc<Metrics>,
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
    /// The op-log replay port (ADR-0025 slice 4b). When set + the client's
    /// epoch matches + its `resume_lsn` is in-window, `register_subscribe`
    /// replays the offline gap from `cairn_oplog` instead of full-snapshotting.
    /// `None` (fake mode, or a binary built without feature `pg`) → always
    /// snapshot. Injected under `NOSTOS_REPLICATOR=pg`.
    pub oplog_reader: Option<Arc<dyn OpLogSource>>,
    /// The active ruleset (ADR-0031). Swapped at runtime by the reload watcher
    /// (Task 14); read at SUBSCRIBE time only — never per delivered event.
    /// Defaults to `ActiveRuleset::all_mode()`.
    pub rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
    /// Broadcasts the active ruleset's checksum. A per-connection
    /// `Receiver::changed()` arm is free while unchanged, so live-session
    /// invalidation (Task 14) costs nothing on the delivery path.
    /// Defaults to a channel seeded with `ActiveRuleset::all_mode().checksum()`.
    pub rules_changed: tokio::sync::watch::Receiver<u64>,
}

impl SyncRouterState {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, auth: Arc<dyn SyncAuth>) -> Self {
        let rules = ActiveRuleset::all_mode();
        // No reload watcher wired by default — the sender has no consumer
        // until the composition root creates one (main.rs) and passes the
        // matching `Receiver` via `with_rules`.
        let (_rules_tx, rules_changed) = tokio::sync::watch::channel(rules.checksum());
        Self {
            manager,
            session_buffer: DEFAULT_SESSION_BUFFER,
            metrics: Arc::new(Metrics::new()),
            auth,
            tenant_column: None,
            write_back: Arc::new(crate::write_back::NoWriteBack::new()),
            write_tables: Arc::new(HashSet::new()),
            snapshotter: None,
            schema_source: None,
            oplog_reader: None,
            rules: Arc::new(tokio::sync::RwLock::new(rules)),
            rules_changed,
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

    /// Inject the server-wide metrics handle (ADR-0025 slice 4b). The
    /// composition root passes the same `Arc<Metrics>` the replicator bumps
    /// `slot_epoch` into, so `register_subscribe` reads the live epoch. The
    /// default in [`Self::new`] is a throwaway (slot_epoch stays 0 → the gate
    /// forces snapshot, which is correct for tests / fake mode).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Inject the op-log replay adapter (ADR-0025 slice 4b). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgOpLogReader`; otherwise leave it `None`
    /// (the default) so reconnecting clients always take the snapshot path.
    #[must_use]
    pub fn with_oplog_reader(mut self, reader: Arc<dyn OpLogSource>) -> Self {
        self.oplog_reader = Some(reader);
        self
    }

    /// Inject the active ruleset (ADR-0031) and its checksum-change receiver.
    /// The composition root loads `nostos_rules.toml` (or falls back to
    /// [`ActiveRuleset::all_mode`]) and creates the matching
    /// `tokio::sync::watch::channel` before calling this; the default in
    /// [`Self::new`] is a standalone all-mode ruleset with no live sender.
    #[must_use]
    pub fn with_rules(
        mut self,
        rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
        rules_changed: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        self.rules = rules;
        self.rules_changed = rules_changed;
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
    // ADR-0029 §Decision-4 (live-socket): arm the close-on-expiry deadline from
    // the handshake token's `exp`. `None` ⇒ no deadline — the OSS `sync_auth:
    // none` default and Phase-0 no-`exp` tokens stay open, exactly as before.
    let exp = crate::auth::token_exp(&token);
    ws.on_upgrade(move |socket| run_session(socket, state, principal, exp))
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
async fn run_session(
    mut socket: WebSocket,
    state: SyncRouterState,
    principal: Principal,
    exp: Option<i64>,
) {
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
    // ADR-0031: snapshot the active ruleset once per socket. Cloned (cheap —
    // wraps a BTreeMap) rather than holding the RwLock read guard across the
    // awaits inside register_subscribe. This snapshot becomes `old_ruleset`
    // in the write_loop below (D3, Task 14) so a live rules reload can be
    // verified against exactly what this socket's already-registered
    // subscriptions were granted.
    let ruleset = state.rules.read().await.clone();

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

    // ADR-0025 F2: advertise the server's current slot epoch ONCE at subscribe
    // (before snapshot/replay frames) on BOTH paths, so the client can persist
    // + resend it on reconnect (the resume gate compares client vs server
    // epoch). Read fresh here — register_subscribe reads the same raw value
    // below for the gate, so the client persists exactly what its resume will
    // be judged against.
    let server_epoch = state
        .metrics
        .slot_epoch
        .load(std::sync::atomic::Ordering::Relaxed);
    // ADR-0031 D2: a client that sent `rules_checksum` on its Subscribe frame
    // gets the raw epoch + the raw checksum advertised back, so its logs can
    // tell "slot recreated" from "rules changed". A pre-D2 client (no
    // `rules_checksum` in its Subscribe) gets the old composed value — its
    // frame is byte-identical to today's.
    let has_checksum = subscribe.client_rules_checksum.is_some();
    if !has_checksum {
        debug!("client omitted rules_checksum; using composed-epoch fallback");
    }
    let (advertised_epoch, advertised_checksum) =
        resume_advertisement(has_checksum, server_epoch, ruleset.checksum());
    let _ = server_frames_tx
        .send(encode_resume_info(advertised_epoch, advertised_checksum))
        .await;

    // 5. Register the FIRST table. A where_sql rejection or the global device
    //    cap is FATAL here (close the socket with a reason before any event
    //    flows, same as the single-table path); subsequent rejects are
    //    non-fatal (the reader logs + keeps serving existing subscriptions).
    if let Err(reject) = register_subscribe(
        &subscribe,
        &subs,
        &manager,
        snapshotter.as_ref(),
        server_epoch,
        state.oplog_reader.as_ref(),
        &sink_concrete,
        &principal,
        state.tenant_column.as_deref(),
        &ruleset,
    )
    .await
    {
        match reject {
            SubscribeReject::Rejected(reason) => {
                debug!(%reason, "closing socket: first subscribe rejected");
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

    // ADR-0029 §Decision-4 (live-socket): a live socket must not outlive its
    // JWT. Arm a one-shot deadline at `exp + leeway`; when it fires the writer
    // (below) sends Close(4401) and ends the session — the client's reconnect
    // loop then reconnects with the refreshed token held via `set_token`. Armed
    // only when the handshake token carried an `exp` (None ⇒ no-op: the OSS
    // `sync_auth: none` default and no-`exp` tokens behave exactly as before).
    let exp_fired = Arc::new(Notify::new());
    let exp_task = exp.map(|exp_secs| {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(i64::MAX, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let remaining = u64::try_from(
            (exp_secs + crate::auth::JWT_LEEWAY_SECS)
                .saturating_sub(now_secs)
                .max(0),
        )
        .unwrap_or(0);
        let notify = Arc::clone(&exp_fired);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
            notify.notify_one();
        })
    });
    let exp_for_writer = Arc::clone(&exp_fired);

    // ADR-0031 D3: live-session re-scoping on a rules reload. `rules_rx` only
    // wakes when the watcher (crates/nostos-server/src/main.rs::watch_rules)
    // observes an actual checksum change on the rules file — a
    // `watch::Receiver` that has already seen the current value never fires,
    // so this arm costs nothing on the per-event delivery path below until an
    // operator actually edits `nostos_rules.toml`.
    let mut rules_rx = state.rules_changed.clone();
    let rules_shared = Arc::clone(&state.rules);
    let subs_for_reload = Arc::clone(&subs);
    let principal_for_reload = principal.clone();

    let write_loop = tokio::spawn(async move {
        use futures_util::sink::SinkExt as _;
        let mut writer = writer;
        let mut old_ruleset = ruleset;
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
                // ADR-0029 §Decision-4 (live-socket): token-expiry deadline.
                // Inert (never notified) for no-`exp`/anonymous sessions.
                () = exp_for_writer.notified() => {
                    debug!("closing socket: token expired (ADR-0029 §Decision-4)");
                    let frame = axum::extract::ws::CloseFrame {
                        code: 4401,
                        reason: "nostos: token expired".into(),
                    };
                    let _ = writer
                        .send(axum::extract::ws::Message::Close(Some(frame)))
                        .await;
                    break;
                }
                // ADR-0031 D3: sync-rules reload. Verification is per-table:
                // only a subscribed table whose rule decision changed at all
                // trips this — an edit that never touches this socket's
                // tables is free.
                res = rules_rx.changed() => {
                    if res.is_err() {
                        continue; // sender dropped (server shutting down)
                    }
                    let new_ruleset = rules_shared.read().await.clone();
                    let narrowed = {
                        let s = subs_for_reload.lock().await;
                        s.tables.iter().any(|table| {
                            old_ruleset.decide(table, &principal_for_reload)
                                != new_ruleset.decide(table, &principal_for_reload)
                        })
                    };
                    if narrowed {
                        debug!("closing socket: sync rules changed under a live session (ADR-0031 D3)");
                        // ponytail: swap verification is coarse — ANY per-table
                        // rule-decision change (a real narrow, or a widen this
                        // code can't prove safe) disconnects the whole socket
                        // rather than re-scoping just the affected subscription
                        // in place. Ceiling: one reconnect + resnapshot per
                        // connected client per rules edit that touches one of
                        // its subscribed tables, including edits that only
                        // widened. Upgrade path: a real subset/implication
                        // check on `PredicateExpr` for the Allow-to-Allow case
                        // so a genuine widen can keep running in place, plus
                        // per-subscription differential resync instead of
                        // closing the whole socket.
                        let frame = axum::extract::ws::CloseFrame {
                            code: axum::extract::ws::close_code::INVALID,
                            reason: RULES_CHANGED_CLOSE_REASON.into(),
                        };
                        let _ = writer
                            .send(axum::extract::ws::Message::Close(Some(frame)))
                            .await;
                        break;
                    }
                    old_ruleset = new_ruleset;
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
    // ADR-0025 slice 4b: read slot_epoch fresh per mid-session subscribe (it
    // bumps on slot recreate) + the op-log reader for the replay branch.
    let metrics_reader = Arc::clone(&state.metrics);
    let oplog_reader = state.oplog_reader.clone();
    // ADR-0031 D3: read fresh (not a connection-start snapshot) so a
    // mid-session subscribe issued after a live rules reload is decided
    // against the CURRENT ruleset, not a stale one — otherwise a table just
    // denied by the reload could still be granted to a newly-subscribed
    // table on an existing socket. Cold path (bounded by MAX_TABLES_PER_SOCKET
    // Subscribe frames per socket, ever), so the extra read costs nothing
    // that matters.
    let rules_for_reader = Arc::clone(&state.rules);
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
                    epoch,
                    rules_checksum,
                }) => {
                    let req = SubscribeRequest {
                        table,
                        filters,
                        where_sql,
                        resume_lsn,
                        client_epoch: epoch,
                        client_rules_checksum: rules_checksum,
                    };
                    let current_ruleset = rules_for_reader.read().await.clone();
                    if let Err(e) = register_subscribe(
                        &req,
                        &subs_reader,
                        &manager_reader,
                        snapshotter.as_ref(),
                        metrics_reader
                            .slot_epoch
                            .load(std::sync::atomic::Ordering::Relaxed),
                        oplog_reader.as_ref(),
                        &ack_sink,
                        &write_principal,
                        tenant_column_for_writes.as_deref(),
                        &current_ruleset,
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
    // ADR-0029 §Decision-4: cancel any pending token-expiry deadline so a
    // short-lived session doesn't leave a sleeping timer task behind.
    if let Some(task) = exp_task {
        task.abort();
    }
}

/// Why a subscribe was rejected. Non-fatal for mid-session subscribes (the
/// socket keeps serving its existing tables); FATAL for the first subscribe
/// (the socket is closed — see `run_session`).
#[derive(Debug)]
enum SubscribeReject {
    /// The predicate could not be built — rules denial (`NotSynced`/
    /// `MissingClaim`, ADR-0031) or a `where_sql` compile failure (ADR-0012).
    /// Carries [`SubscribeRejection`]'s rendered message.
    Rejected(String),
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

/// The `(epoch, rules_checksum)` pair to advertise on `resume_info` for a
/// subscribe (ADR-0031 D2). Shared by the advertise site (`run_session`) and
/// `register_subscribe`'s resume gate so the two can never drift apart: what
/// a client is told to persist is byte-for-byte what its next resume is
/// judged against.
fn resume_advertisement(
    client_sent_checksum: bool,
    server_epoch: u64,
    rules_checksum: u64,
) -> (u64, Option<u64>) {
    if client_sent_checksum {
        (server_epoch, Some(rules_checksum))
    } else {
        (compose_sync_epoch(server_epoch, rules_checksum), None)
    }
}

/// Register one table subscription on the socket's shared sink: predicate
/// build, per-socket cap + idempotency checks, `SessionManager::connect`, and
/// snapshot-on-subscribe. Called for the first subscribe (pre-split, in
/// `run_session`) and every subsequent one (post-split, in the reader task).
/// Returns `Err` WITHOUT registering on any rejection. Critical sections on
/// `subs` are short and never span an `.await`; access is serialized anyway
/// (one reader task; the first subscribe runs before the reader is spawned).
#[allow(clippy::too_many_arguments)] // 10 params is the genuine subscribe surface; a param-struct would obscure the call sites.
async fn register_subscribe(
    req: &SubscribeRequest,
    subs: &Arc<Mutex<SocketSubs>>,
    manager: &Arc<SessionManager>,
    snapshotter: Option<&Arc<dyn SnapshotSource>>,
    server_epoch: u64,
    oplog_reader: Option<&Arc<dyn OpLogSource>>,
    sink_concrete: &Arc<TokioEventSink>,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
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

    let predicate = build_predicate(req, principal, tenant_column, ruleset)
        .map_err(|rejection| SubscribeReject::Rejected(rejection.to_string()))?;
    let session = SyncSession::new_authenticated(predicate, principal.clone());
    // Derive the type-erased clone the store holds; `sink_concrete` stays the
    // concrete handle for snapshot delivery below.
    let sink_dyn: Arc<dyn EventSink> = Arc::clone(sink_concrete) as Arc<dyn EventSink>;
    let id = manager
        .connect(session, sink_dyn)
        .await
        .map_err(|_| SubscribeReject::DeviceCapReached)?;

    // ── Op-log replay-on-reconnect (ADR-0025 slice 4b). When the client's
    //    epoch matches the server's current slot epoch AND its resume_lsn is
    //    within the retained op-log window, replay the offline gap from
    //    `cairn_oplog` to the fresh sink and SKIP the snapshot. The client
    //    dedups per-row by lsn (slice 4a), so the concurrent live fan-out +
    //    replay overlap is safe. Live fan-out started at `manager.connect`
    //    above. Any decline (epoch mismatch, aged-out resume, empty/failed
    //    replay, no reader) falls through to the snapshot path below — slice-1
    //    reconcile is the correctness floor.
    let client_epoch = req.client_epoch.unwrap_or(0);
    // ADR-0031 D2: a D2 client sent `rules_checksum` on Subscribe, so its
    // epoch and checksum are compared independently against the raw
    // `server_epoch` and the active ruleset's checksum — same slot epoch but
    // a rules edit still forces a snapshot.
    //
    // ponytail: pre-D2 clients get the rules checksum folded into the
    // advertised epoch, so their logs cannot distinguish a slot recreate from
    // a rules edit. Ceiling: log-level attribution only for old clients.
    // Upgrade path: drop the fallback once the SDK floor is D2-or-newer.
    let (want_epoch, want_checksum) = resume_advertisement(
        req.client_rules_checksum.is_some(),
        server_epoch,
        ruleset.checksum(),
    );
    let epoch_and_checksum_match =
        client_epoch == want_epoch && req.client_rules_checksum == want_checksum;
    if epoch_and_checksum_match && !req.table.is_empty() {
        if let (Some(reader), Some(resume)) = (oplog_reader, req.resume_lsn) {
            let in_window = matches!(reader.window_tail().await, Ok(tail) if resume >= tail);
            if in_window {
                match reader
                    .replay_after(principal.tenant_id.as_str(), resume)
                    .await
                {
                    Ok(events) if !events.is_empty() => {
                        let count = events.len();
                        for ev in events {
                            // Backpressure-aware (slice-1): the bounded sink
                            // mustn't truncate the replay. Live `deliver` +
                            // replay `deliver_awaiting` share the FIFO channel;
                            // slice-4a's per-row lsn gate dedups the overlap.
                            let _ = sink_concrete.deliver_awaiting(ev).await;
                        }
                        debug!(
                            table = %req.table, resume, count,
                            "op-log replay delivered (epoch match, in-window); skipping snapshot"
                        );
                        // Record the session on the socket (same bookkeeping as
                        // the snapshot path's tail, minus the synthetic-cursor
                        // advance — replay events carry REAL lsns, not synthetic
                        // ones, so they don't consume the cursor's space).
                        {
                            let mut s = subs.lock().await;
                            s.ids.push(id);
                            s.tables.insert(req.table.clone());
                        }
                        return Ok(());
                    }
                    Ok(_) => debug!(
                        table = %req.table, resume,
                        "op-log replay empty; falling back to snapshot"
                    ),
                    Err(e) => warn!(
                        table = %req.table, error = %e,
                        "op-log replay failed; falling back to snapshot"
                    ),
                }
            } else {
                debug!(
                    table = %req.table, resume,
                    "resume_lsn aged out of op-log window; snapshot"
                );
            }
        }
    }

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
/// string is `"upsert" | "delete" | "patch" | "increment"`; anything else is an
/// `InvalidPayload`. The payload (a `serde_json::Value`) is rendered back to
/// JSON text for the upsert/patch/increment paths (the port takes `&str`).
/// `tenant` (ADR-0018) is forwarded verbatim to the adapter — `dispatch_write`
/// doesn't interpret it, just relays the scope the caller already computed
/// from the principal.
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
        "increment" => {
            // ADR-0030 Decision 1: server-authoritative counter delta. Payload
            // is `{"field","delta"}`; PgWriteBack emits SET col = col + ?. Same
            // object-ness guard — the adapter re-validates field/delta.
            let value = payload.ok_or_else(|| {
                WriteBackError::InvalidPayload("payload required for increment".into())
            })?;
            if !value.is_object() {
                return Err(WriteBackError::InvalidPayload(
                    "payload must be a JSON object".into(),
                ));
            }
            let json = value.to_string();
            write_back.increment(table, pk, &json, tenant).await
        }
        "delete" => write_back.delete(table, pk, tenant).await,
        other => Err(WriteBackError::InvalidPayload(format!(
            "unknown op: {other} (expected 'upsert', 'delete', 'patch', or 'increment')"
        ))),
    }
}

/// Why a subscribe was refused. Rendered into the close reason via `Display`
/// (both `run_session` fatal paths and `register_subscribe`'s
/// `SubscribeReject::Rejected` wrap the rendered string — the SDKs surface it
/// verbatim to app developers, ADR-0031).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubscribeRejection {
    /// The table is unlisted, or listed with `sync = false` (`toggles` mode).
    /// Fail-closed (ADR-0031 Global Constraint 10): an unlisted table is never
    /// treated as "everything", only ever as "nothing".
    NotSynced { table: String, mode: SyncMode },
    /// The connecting principal lacks a claim the rules' scope for this table
    /// references (e.g. `org_id = claims.org_id` with no `org_id` claim).
    MissingClaim { table: String, claim: String },
    /// `where_sql` failed to compile (ADR-0012) — folded into this enum so
    /// every subscribe refusal shares one path.
    InvalidWhereSql(String),
}

impl std::fmt::Display for SubscribeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSynced { table, mode } => write!(
                f,
                "table `{table}` is not synced by the active rules (sync_mode={})",
                mode.as_str()
            ),
            Self::MissingClaim { table, claim } => write!(
                f,
                "missing claim `{claim}` required by the rules for table `{table}`"
            ),
            Self::InvalidWhereSql(reason) => write!(f, "invalid where_sql: {reason}"),
        }
    }
}

/// Build the server-enforced predicate from the client's subscribe + principal.
///
/// Composition order (ADR-0031), all three ANDed, none skippable:
/// **rules scope** (this table's compiled scope from the active ruleset — deny
/// closes the socket before anything else runs) **AND** **tenant scope**
/// (ADR-0011) **AND** **client filters + `where_sql`** (ADR-0012).
///
/// The rules decision runs FIRST and fail-closed: an unlisted/toggled-off
/// table or a missing scope claim rejects the subscribe outright, never
/// falling back to match-all. `ActiveRuleset::all_mode()` always allows
/// (`RuleDecision::Allow(PredicateExpr::any())`) — but tenant scoping below
/// still applies unconditionally in every mode (ADR-0031 Global Constraint
/// 11: `all` mode disables rules, not tenancy).
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
/// wraps the client expression and a `where_sql` can never shed it or the rules
/// scope seeded below. A parse failure is `Err(InvalidWhereSql)`; the caller
/// closes the socket with that reason before any event flows.
///
/// The IF-to-enforce-tenant decision is [`Principal::tenant_scope`] — the same
/// seam the write path (`dispatch_write`, ADR-0018) calls, so the read and
/// write enforcement conditions cannot drift apart.
fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<Predicate, SubscribeRejection> {
    // Rules scope FIRST, fail-closed (ADR-0031). No filter/tenant work happens
    // until the rules allow this table for this principal.
    let rules_expr = match ruleset.decide(&subscribe.table, principal) {
        RuleDecision::Allow(expr) => expr,
        RuleDecision::DeniedTable => {
            return Err(SubscribeRejection::NotSynced {
                table: subscribe.table.clone(),
                mode: ruleset.mode(),
            });
        }
        RuleDecision::DeniedClaim(claim) => {
            return Err(SubscribeRejection::MissingClaim {
                table: subscribe.table.clone(),
                claim,
            });
        }
    };

    let scope = principal.tenant_scope(tenant_column);

    // Seed the predicate from the rules scope (replaces the historical
    // match-all start), then fold in the client's own filters — EXCLUDING any
    // on the tenant column, which the server overrides with the principal's
    // real value (never client-attested). The `and_eq` combinator collapses an
    // `Any` root down to a bare `Eq` leaf (all-mode, no rules scope) and
    // otherwise ANDs onto whatever the rules already seeded — same combinator,
    // no special-casing needed here.
    let mut p = Predicate {
        table: subscribe.table.clone(),
        expr: rules_expr,
    };
    for f in &subscribe.filters {
        if scope.is_some_and(|s| f.column == s.column) {
            continue; // server injects the real tenant value below
        }
        p = p.and_eq(&f.column, ColumnValue::text(&f.value));
    }

    // Compile the optional safe-SQL-subset expression (ADR-0012) and AND it in.
    // Done BEFORE tenant enforcement so the server-injected tenant clause wraps
    // the client expression — a where_sql can never widen scope past its tenant
    // OR past the rules scope already folded in above.
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
            Err(e) => return Err(SubscribeRejection::InvalidWhereSql(e.to_string())),
        }
    }

    // Server-enforced tenant scoping (ADR-0011). Always injected for an
    // authenticated principal when a tenant column is configured — in every
    // rules mode, including `all` (ADR-0031 Global Constraint 11). This stays
    // LAST so it wraps everything above (rules scope + filters + where_sql).
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
    /// The client's last-seen server slot epoch (ADR-0025 slice 4b). `None` on
    /// old clients → the gate treats it as a mismatch → snapshot (safe default).
    client_epoch: Option<u64>,
    /// The client's last-synced rules checksum (ADR-0031, D2). `Some` marks a
    /// D2-or-newer client: the gate compares epoch and checksum independently.
    /// `None` marks a pre-D2 client: the gate falls back to the composed
    /// epoch (see `register_subscribe`).
    client_rules_checksum: Option<u64>,
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
                epoch,
                rules_checksum,
            } => Some(SubscribeRequest {
                table,
                filters,
                where_sql,
                resume_lsn,
                client_epoch: epoch,
                client_rules_checksum: rules_checksum,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_application::ports::SessionStore;
    use nostos_domain::{Lsn, RowOp};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    /// A canned op-log reader for the reconnect-resume branch tests (ADR-0025
    /// slice 4b). `replay_calls` distinguishes "replay attempted + empty"
    /// (case e) from "replay never reached" (b, c, d).
    struct MockOpLog {
        events: Vec<ReplicationEvent>,
        tail: u64,
        replay_calls: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl nostos_application::ports::OpLogSource for MockOpLog {
        async fn replay_after(
            &self,
            _tenant: &str,
            _after: u64,
        ) -> Result<Vec<ReplicationEvent>, nostos_application::ports::OpLogError> {
            self.replay_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.events.clone())
        }
        async fn window_tail(&self) -> Result<u64, nostos_application::ports::OpLogError> {
            Ok(self.tail)
        }
    }

    fn ev(lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    /// Build the register_subscribe harness. `snapshotter` is `None`, so the
    /// snapshot path delivers nothing — the replay-vs-snapshot observable is
    /// whether the sink received events (+ the replay-call counter).
    #[allow(clippy::unused_async)] // sync body; kept async so call sites read uniformly with the awaited setup.
    async fn harness() -> (
        Arc<Mutex<SocketSubs>>,
        Arc<SessionManager>,
        Arc<TokioEventSink>,
        mpsc::Receiver<crate::router::SinkMsg>,
    ) {
        let subs = Arc::new(Mutex::new(SocketSubs {
            ids: Vec::new(),
            tables: HashSet::new(),
            synthetic_cursor: 0,
        }));
        let store: Arc<dyn SessionStore> = Arc::new(crate::store::InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let (sink, rx) = TokioEventSink::channel(16);
        (subs, manager, Arc::new(sink), rx)
    }

    // Legacy (pre-D2) request: no `rules_checksum` on the wire, so the gate
    // takes the composed-epoch fallback. D2 tests build `SubscribeRequest`
    // directly with `client_rules_checksum: Some(_)`.
    fn req(table: &str, client_epoch: Option<u64>, resume: Option<u64>) -> SubscribeRequest {
        SubscribeRequest {
            table: table.into(),
            filters: Vec::new(),
            where_sql: None,
            resume_lsn: resume,
            client_epoch,
            client_rules_checksum: None,
        }
    }

    // (a) epoch match + resume ≥ tail + non-empty replay → replay delivers.
    #[tokio::test]
    async fn replay_delivers_on_epoch_match_in_window() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        // Legacy (no rules_checksum) client: the gate compares the composed
        // fallback, so the request must carry the composed value to match.
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert!(rx.try_recv().is_err(), "replay delivered exactly one event");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // (b) epoch mismatch → snapshot (replay never reached).
    #[tokio::test]
    async fn snapshot_on_epoch_mismatch() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req("tasks", Some(1), Some(5)),
            &subs,
            &manager,
            None,
            2,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "snapshot path (snapshotter None) delivers nothing"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // (c) resume < tail (aged out of the op-log window) → snapshot.
    #[tokio::test]
    async fn snapshot_when_resume_aged_out() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 100, // resume 5 < tail 100 → aged out
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    // (d) no oplog reader wired → snapshot.
    #[tokio::test]
    async fn snapshot_when_no_reader() {
        let (subs, manager, sink, mut rx) = harness().await;
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            None,
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // (e) replay returns empty → fall back to snapshot (replay WAS attempted).
    #[tokio::test]
    async fn snapshot_when_replay_empty() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: Vec::new(),
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "empty replay → snapshot, nothing delivered"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "replay was attempted, just empty"
        );
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // ADR-0031 D2 — the resume gate compares epoch and checksum
    // independently for a client that sent `rules_checksum` on Subscribe.

    // (f) D2 client, epoch AND checksum both match current server state →
    // replay (not snapshot).
    #[tokio::test]
    async fn d2_client_replays_when_epoch_and_checksum_match() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let ruleset = ActiveRuleset::all_mode();
        let req = SubscribeRequest {
            table: "tasks".into(),
            filters: Vec::new(),
            where_sql: None,
            resume_lsn: Some(5),
            client_epoch: Some(7),
            client_rules_checksum: Some(ruleset.checksum()),
        };
        register_subscribe(
            &req,
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // (g) D2 client, same slot epoch but the active ruleset checksum has
    // moved on (a rules edit, no slot recreate) → snapshot. This is the
    // entire point of D2: pre-D2 epoch-only comparison would have replayed.
    #[tokio::test]
    async fn d2_client_snapshots_when_only_checksum_differs() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let old_checksum = ActiveRuleset::all_mode().checksum();
        let current_ruleset = ActiveRuleset::compile(&toggles_rules("tasks", true, None)).unwrap();
        assert_ne!(
            old_checksum,
            current_ruleset.checksum(),
            "test needs two distinguishable rulesets"
        );
        let req = SubscribeRequest {
            table: "tasks".into(),
            filters: Vec::new(),
            where_sql: None,
            resume_lsn: Some(5),
            client_epoch: Some(7),
            client_rules_checksum: Some(old_checksum),
        };
        register_subscribe(
            &req,
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &current_ruleset,
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "checksum mismatch → snapshot, nothing delivered"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0, "replay never attempted");
    }

    // (h) legacy (pre-D2) client, no `rules_checksum` on the wire — the
    // composed-epoch fallback still forces a snapshot when the rules changed,
    // exactly as it would have folded a slot recreate in before D2.
    #[tokio::test]
    async fn legacy_client_snapshots_on_rules_change() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let current_ruleset = ActiveRuleset::compile(&toggles_rules("tasks", true, None)).unwrap();
        let stale_composed = compose_sync_epoch(7, ActiveRuleset::all_mode().checksum());
        register_subscribe(
            &req("tasks", Some(stale_composed), Some(5)),
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &current_ruleset,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    // (i) legacy client, nothing changed (same slot epoch, same rules) →
    // still replays — the fallback must not regress ADR-0025 for clients that
    // never adopt D2.
    #[tokio::test]
    async fn legacy_client_replays_when_nothing_changed() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let ruleset = ActiveRuleset::all_mode();
        let composed = compose_sync_epoch(7, ruleset.checksum());
        register_subscribe(
            &req("tasks", Some(composed), Some(5)),
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // (j) what `resume_info` advertises is exactly what the gate accepts on
    // the client's next reconnect — both the D2 and the legacy path. Exercises
    // `resume_advertisement` directly (the single source of truth for both
    // sites) rather than re-deriving the formula by hand, so this test only
    // fails if the two call sites actually diverge.
    #[tokio::test]
    async fn advertised_values_match_gate_values() {
        let ruleset = ActiveRuleset::all_mode();
        let principal = Principal::new("acct", "tenant-acme");

        // D2 path.
        {
            let (subs, manager, sink, mut rx) = harness().await;
            let calls = Arc::new(AtomicU64::new(0));
            let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
                events: vec![ev(10)],
                tail: 0,
                replay_calls: Arc::clone(&calls),
            });
            let (adv_epoch, adv_checksum) = resume_advertisement(true, 7, ruleset.checksum());
            let req = SubscribeRequest {
                table: "tasks".into(),
                filters: Vec::new(),
                where_sql: None,
                resume_lsn: Some(5),
                client_epoch: Some(adv_epoch),
                client_rules_checksum: adv_checksum,
            };
            register_subscribe(
                &req,
                &subs,
                &manager,
                None,
                7,
                Some(&reader),
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
            assert!(matches!(
                rx.recv().await,
                Some(crate::router::SinkMsg::Event(_))
            ));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }

        // Legacy path.
        {
            let (subs, manager, sink, mut rx) = harness().await;
            let calls = Arc::new(AtomicU64::new(0));
            let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
                events: vec![ev(10)],
                tail: 0,
                replay_calls: Arc::clone(&calls),
            });
            let (adv_epoch, adv_checksum) = resume_advertisement(false, 7, ruleset.checksum());
            assert_eq!(adv_checksum, None, "legacy path omits the checksum key");
            register_subscribe(
                &req("tasks", Some(adv_epoch), Some(5)),
                &subs,
                &manager,
                None,
                7,
                Some(&reader),
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
            assert!(matches!(
                rx.recv().await,
                Some(crate::router::SinkMsg::Event(_))
            ));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }

    // (ADR-0031) SyncRouterState::new defaults to the permissive zero-config
    // ruleset so no existing construction site (none of which pass rules)
    // breaks.
    #[tokio::test]
    async fn router_state_defaults_to_all_mode() {
        let store: Arc<dyn SessionStore> = Arc::new(crate::store::InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn SyncAuth> = Arc::new(crate::auth::AllowAnonymous::new());
        let state = SyncRouterState::new(manager, auth);
        assert_eq!(state.rules.read().await.mode(), nostos_domain::SyncMode::All);
    }

    fn toggles_rules(table: &str, sync: bool, scope: Option<&str>) -> nostos_domain::SyncRules {
        nostos_domain::SyncRules {
            version: nostos_domain::RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![nostos_domain::TableRule {
                table: table.into(),
                sync,
                scope: scope.map(str::to_string),
            }],
            hand: Vec::new(),
        }
    }

    // Task 10: toggles mode, `notes` sync=false → the rules decision is
    // fail-closed, not "everything" — `build_predicate` refuses before any
    // filter/tenant work runs.
    #[test]
    fn subscribe_to_unsynced_table_is_rejected() {
        let ruleset = ActiveRuleset::compile(&toggles_rules("notes", false, None)).unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let err =
            build_predicate(&req("notes", None, None), &principal, None, &ruleset).unwrap_err();
        assert_eq!(
            err,
            SubscribeRejection::NotSynced {
                table: "notes".into(),
                mode: SyncMode::Toggles,
            }
        );
    }

    // Task 10: rules scope (`status = 'open'`) AND tenant scope (`org_id`) —
    // a row must satisfy both, neither alone is enough.
    #[test]
    fn rules_scope_is_anded_with_tenant_scope() {
        let ruleset =
            ActiveRuleset::compile(&toggles_rules("tasks", true, Some("status = 'open'"))).unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let predicate = build_predicate(
            &req("tasks", None, None),
            &principal,
            Some("org_id"),
            &ruleset,
        )
        .unwrap();

        let row = |status: &'static str, org: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                match col {
                    "status" => Some(ColumnValue::text(status)),
                    "org_id" => Some(ColumnValue::text(org)),
                    _ => None,
                }
            }
        };
        assert!(predicate.expr.matches(row("open", "tenant-acme")));
        assert!(
            !predicate.expr.matches(row("closed", "tenant-acme")),
            "rules scope must hold"
        );
        assert!(
            !predicate.expr.matches(row("open", "someone-else")),
            "tenant scope must hold"
        );
    }

    // Task 10 / ADR-0031 Global Constraint 11: `all` mode disables rules but
    // never tenancy — a foreign-tenant row must not match even with no rules
    // scope in play.
    #[test]
    fn all_mode_still_applies_tenant_scope() {
        let ruleset = ActiveRuleset::all_mode();
        let principal = Principal::new("acct", "tenant-acme");
        let predicate = build_predicate(
            &req("tasks", None, None),
            &principal,
            Some("org_id"),
            &ruleset,
        )
        .unwrap();

        let with_org = |org: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "org_id").then(|| ColumnValue::text(org))
            }
        };
        assert!(predicate.expr.matches(with_org("tenant-acme")));
        assert!(!predicate.expr.matches(with_org("someone-else")));
    }

    // Task 10: a scope claim the principal doesn't carry is a denial, not a
    // silent widen — `claims.sub` (no explicit claim needed, resolves to the
    // account id) is present, but a table gated on `claims.org_id` with no
    // `org_id` claim on the principal must reject.
    #[test]
    fn missing_claim_rejects_subscribe() {
        let ruleset = ActiveRuleset::compile(&toggles_rules(
            "tasks",
            true,
            Some("org_id = claims.org_id"),
        ))
        .unwrap();
        let principal = Principal::new("acct", "tenant-acme"); // no org_id claim
        let err =
            build_predicate(&req("tasks", None, None), &principal, None, &ruleset).unwrap_err();
        assert_eq!(
            err,
            SubscribeRejection::MissingClaim {
                table: "tasks".into(),
                claim: "org_id".into(),
            }
        );
    }

    // Task 10: client `where_sql` is ANDed onto the rules scope, never
    // substituted for it — `owner_id = claims.sub` (→ "owner1") AND
    // `owner_id = 'someone_else'` is unsatisfiable, so the composed predicate
    // matches nothing, proving where_sql cannot widen past the rules scope.
    #[test]
    fn where_sql_cannot_widen_past_rules() {
        let ruleset =
            ActiveRuleset::compile(&toggles_rules("tasks", true, Some("owner_id = claims.sub")))
                .unwrap();
        let principal = Principal::new("owner1", "tenant-acme");
        let mut subscribe = req("tasks", None, None);
        subscribe.where_sql = Some("owner_id = 'someone_else'".into());
        let predicate = build_predicate(&subscribe, &principal, None, &ruleset).unwrap();

        let with_owner = |owner: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "owner_id").then(|| ColumnValue::text(owner))
            }
        };
        assert!(!predicate.expr.matches(with_owner("someone_else")));
        assert!(!predicate.expr.matches(with_owner("owner1")));
    }
}
