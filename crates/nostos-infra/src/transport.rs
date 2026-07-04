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

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::stream::StreamExt as _;
use serde::Deserialize;
use tokio::sync::Notify;
use tracing::{debug, warn};

use nostos_application::ports::SyncAuth;
use nostos_application::SessionManager;
use nostos_domain::{ColumnValue, Predicate, Principal, ReplicationEvent, SyncSession};

use crate::router::TokioEventSink;
use crate::wire::{decode_client_message, encode_event, encode_events, ClientMessage};

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
}

impl SyncRouterState {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, auth: Arc<dyn SyncAuth>) -> Self {
        Self {
            manager,
            session_buffer: DEFAULT_SESSION_BUFFER,
            auth,
            tenant_column: None,
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

    // 2. Build the predicate: the client's filters + optional safe-SQL
    //    `where_sql` (ADR-0012), intersected with the server-injected tenant
    //    filter (never client-attested). A where_sql parse failure closes the
    //    socket with a reason before any event flows (no session is registered,
    //    so nothing can leak).
    let predicate = match build_predicate(&subscribe, &principal, state.tenant_column.as_deref()) {
        Ok(p) => p,
        Err(reason) => {
            debug!(%reason, "closing socket: where_sql rejected");
            // Send an explicit close frame so the client sees the reason
            // (axum's `WebSocket::close()` would drop it). The reason already
            // contains the canonical "invalid where_sql: " prefix.
            let frame = axum::extract::ws::CloseFrame {
                code: axum::extract::ws::close_code::INVALID,
                reason: reason.into(),
            };
            let _ = socket
                .send(axum::extract::ws::Message::Close(Some(frame)))
                .await;
            return;
        }
    };

    // 3. Allocate the bounded sink. We keep the *concrete* `Arc<TokioEventSink>`
    //    for close()/record_ack(), and a type-erased clone for the store.
    let (sink, mut rx) = TokioEventSink::channel(state.session_buffer);
    let sink_concrete = Arc::new(sink);
    let sink_dyn: Arc<dyn nostos_application::ports::EventSink> =
        Arc::clone(&sink_concrete) as Arc<dyn nostos_application::ports::EventSink>;

    // 4. Seed the resume cursor if the client sent one.
    if let Some(resume) = subscribe.resume_lsn {
        sink_concrete.seed_acked_lsn(nostos_domain::Lsn::new(resume));
        debug!(
            resume_lsn = resume,
            "session resuming from client checkpoint"
        );
    }

    let session = SyncSession::new_authenticated(predicate, principal);

    // 5. Register with the store via the manager.
    let manager = Arc::clone(&state.manager);
    let id = match manager.connect(session, sink_dyn).await {
        Ok(id) => id,
        Err(_e) => {
            // Concurrent-device cap reached (or another connect failure). Close
            // the socket so the client sees a clean end rather than a hang.
            let _ = socket.close().await;
            return;
        }
    };

    // 6. Split the socket: writer drains the sink, reader parses ACKs.
    //    axum's `WebSocket` is `Stream + Sink`; `StreamExt::split` yields
    //    independent halves so ACK reads don't block frame writes.
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
        while let Some(first) = rx.recv().await {
            // Collect the awaited frame + any backlog, capped at MAX_BATCH_FRAMES.
            let mut batch: Vec<ReplicationEvent> = Vec::with_capacity(MAX_BATCH_FRAMES);
            batch.push(first);
            // Non-blocking drain of the backlog.
            while batch.len() < MAX_BATCH_FRAMES {
                match rx.try_recv() {
                    Ok(ev) => batch.push(ev),
                    // Empty or closed → stop draining. (Closed is fine: the
                    // outer `recv().await` will return None on the next loop
                    // iteration and we exit cleanly after flushing this batch.)
                    Err(_) => break,
                }
            }
            // Single frame → legacy single-object form (no array wrapper).
            // Multiple → one JSON-array message.
            let msg = if batch.len() == 1 {
                Message::Binary(encode_event(&batch[0]))
            } else {
                let refs: Vec<&ReplicationEvent> = batch.iter().collect();
                Message::Binary(encode_events(&refs))
            };
            // SplitSink::send owns the message; one wire write per batch. A
            // slow consumer's backpressure surfaces as a full buffer upstream
            // (the bounded sink drops rather than buffering unbounded here).
            if writer.send(msg).await.is_err() {
                break; // client gone
            }
        }
        let _ = writer;
        closed_tx.notify_waiters();
    });

    // Reader: parse inbound ACK frames and stamp the sink's ack cursor. Exits
    // when the socket closes (returns None) — that also ends the write loop
    // indirectly via the closed notify on the next rx exhaustion.
    let read_loop = tokio::spawn(async move {
        while let Some(Ok(msg)) = reader.next().await {
            match msg {
                Message::Text(t) => handle_client_message(t.as_bytes(), &ack_sink),
                Message::Binary(b) => handle_client_message(&b, &ack_sink),
                Message::Close(_) => break,
                _ => {} // ping/pong
            }
        }
    });

    // Keep the session alive until the writer ends, then clean up.
    closed.notified().await;
    sink_concrete.close();
    manager.disconnect(id).await;
    let _ = write_loop.await;
    // The reader may still be blocked on recv; abort it so the task reaps.
    read_loop.abort();
}

/// Parse an inbound client message and apply it (ACK → stamp cursor). Anything
/// else (a stray second subscribe, malformed) is ignored — the session is
/// already subscribed.
fn handle_client_message(data: &[u8], sink: &TokioEventSink) {
    match decode_client_message(data) {
        Some(ClientMessage::Ack { lsn }) => {
            sink.record_ack(nostos_domain::Lsn::new(lsn));
            debug!(ack_lsn = lsn, "client acknowledged progress");
        }
        Some(ClientMessage::Subscribe { .. }) => {
            // A second subscribe after the initial one — ignore (resubscribe
            // mid-session is a Phase-2 feature; for now one predicate per
            // connection). Don't error; just don't act.
            debug!("ignoring mid-session subscribe");
        }
        None => {
            warn!("dropping malformed client message");
        }
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
fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
) -> Result<Predicate, String> {
    let enforce_tenant = tenant_column.is_some() && !principal.is_anonymous();
    let tenant_col = tenant_column.unwrap_or("");

    // Start match-all, then fold in the client's own filters — EXCLUDING any on
    // the tenant column, which the server overrides with the principal's real
    // value (never client-attested). The `and_eq` combinator collapses the
    // initial match-all down to a bare `Eq` leaf, so a single-filter predicate
    // is structurally identical to the historical `Predicate::eq` form.
    let mut p = Predicate::all(&subscribe.table);
    for f in &subscribe.filters {
        if enforce_tenant && f.column == tenant_col {
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
    if enforce_tenant {
        p = p.and_eq(tenant_col, ColumnValue::text(&principal.tenant_id));
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
/// return its fields. Returns `None` if the client closes or sends a non-subscribe
/// first frame.
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
            ClientMessage::Ack { .. } => {
                // An ACK before subscribing is out of order — wait for the real
                // subscribe.
                None
            }
        };
    }
    None
}

// (Transport-swap seam removed — axum 0.7's `WebSocket` works directly. If we
// later swap to WebTransport, this module is the single place that changes.)
