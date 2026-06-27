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
use nostos_domain::{ColumnValue, Predicate, Principal, SyncSession};

use crate::router::TokioEventSink;
use crate::wire::{decode_client_message, encode_event, ClientMessage};

/// Default per-session bounded-buffer depth. Slow clients that fall this far
/// behind are dropped (an explicit, observable choice — never silent OOM).
const DEFAULT_SESSION_BUFFER: usize = 1024;

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

    // 2. Build the predicate: the client's filters, intersected with the
    //    server-injected tenant filter (never client-attested).
    let predicate = build_predicate(&subscribe, &principal, state.tenant_column.as_deref());

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
        while let Some(event) = rx.recv().await {
            let frame = encode_event(&event);
            // SplitSink::send owns the message; flush per frame so a slow
            // consumer's backpressure surfaces as a full buffer (not buffering
            // unbounded in the sink).
            if writer.send(Message::Binary(frame)).await.is_err() {
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
fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
) -> Predicate {
    let enforce_tenant = tenant_column.is_some() && !principal.is_anonymous();
    let tenant_col = tenant_column.unwrap_or("");

    // The client's own filters — EXCLUDING any on the tenant column, which the
    // server overrides with the principal's real value (never client-attested).
    let client_filters: Vec<&crate::wire::FilterClause> = subscribe
        .filters
        .iter()
        .filter(|f| !enforce_tenant || f.column != tenant_col)
        .collect();

    let mut p = if client_filters.is_empty() {
        Predicate::all(&subscribe.table)
    } else {
        let f = client_filters[0];
        Predicate::eq(&subscribe.table, &f.column, ColumnValue::text(&f.value))
    };
    for f in client_filters.get(1..).unwrap_or(&[]) {
        p = p.and_eq(&f.column, ColumnValue::text(&f.value));
    }

    // Server-enforced tenant scoping (ADR-0011). Always injected for an
    // authenticated principal when a tenant column is configured.
    if enforce_tenant {
        p = p.and_eq(tenant_col, ColumnValue::text(&principal.tenant_id));
    }
    p
}

/// The parsed first-frame subscribe request (internal shape; the wire type is
/// `ClientMessage::Subscribe`).
struct SubscribeRequest {
    table: String,
    filters: Vec<crate::wire::FilterClause>,
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
                resume_lsn,
            } => Some(SubscribeRequest {
                table,
                filters,
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
