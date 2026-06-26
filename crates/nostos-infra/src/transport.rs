//! WebSocket transport adapter — axum route that upgrades a connection, spawns
//! a session, and drains its bounded sink onto the wire.
//!
//! Flow on a new connection:
//! 1. Read the first text frame as a `Subscribe { predicate }` (JSON).
//! 2. Allocate a `TokioEventSink` (bounded channel).
//! 3. Register the session with the `SessionManager` (which adds it to the
//!    store, indexed by `predicate.table`).
//! 4. Spawn a drain task that pulls events off the channel and writes each as
//!    a `WireFrame` to the socket.
//! 5. On disconnect, close the sink + unregister.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use tokio::sync::Notify;

use nostos_application::SessionManager;
use nostos_domain::{Predicate, SyncSession};

use crate::router::TokioEventSink;
use crate::wire::WireCodec;

/// Default per-session bounded-buffer depth. Slow clients that fall this far
/// behind are dropped (an explicit, observable choice — never silent OOM).
const DEFAULT_SESSION_BUFFER: usize = 1024;

/// Shared state injected into the axum router.
#[derive(Clone)]
pub struct SyncRouterState {
    pub manager: Arc<SessionManager>,
    pub codec: Arc<WireCodec>,
    pub session_buffer: usize,
}

impl SyncRouterState {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            manager,
            codec: Arc::new(WireCodec),
            session_buffer: DEFAULT_SESSION_BUFFER,
        }
    }

    #[must_use]
    pub fn with_buffer(mut self, buffer: usize) -> Self {
        self.session_buffer = buffer.max(1);
        self
    }
}

/// The subscription frame a client sends right after connecting.
#[derive(Debug, Deserialize)]
pub struct Subscribe {
    pub table: String,
    /// Optional column-equality filters (Week 1: list of {column, value}).
    #[serde(default)]
    pub filters: Vec<FilterClause>,
}

#[derive(Debug, Deserialize)]
pub struct FilterClause {
    pub column: String,
    pub value: String,
}

impl Subscribe {
    fn to_predicate(&self) -> Predicate {
        if self.filters.is_empty() {
            return Predicate::all(&self.table);
        }
        let mut p = Predicate::eq(
            &self.table,
            &self.filters[0].column,
            nostos_domain::ColumnValue::text(&self.filters[0].value),
        );
        for f in &self.filters[1..] {
            p = p.and_eq(&f.column, nostos_domain::ColumnValue::text(&f.value));
        }
        p
    }
}

/// Axum handler: `GET /sync` → WebSocket upgrade.
///
/// Marked `async` because axum's `Handler` trait requires an async fn (the body
/// doesn't await here, but the trait bound needs the async wrapper). The
/// `unused_async` lint is intentionally allowed for that reason.
#[allow(clippy::unused_async)]
pub async fn sync_handler(
    ws: WebSocketUpgrade,
    State(state): State<SyncRouterState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| run_session(socket, state))
}

/// Drive one WebSocket connection for its lifetime.
async fn run_session(mut socket: WebSocket, state: SyncRouterState) {
    // 1. Read the subscribe frame.
    let Some(predicate) = read_subscribe(&mut socket).await else {
        return; // client disconnected without subscribing
    };

    // 2. Allocate the bounded sink. We keep the *concrete* `Arc<TokioEventSink>`
    //    for the close() call, and a type-erased clone for the store.
    let (sink, mut rx) = TokioEventSink::channel(state.session_buffer);
    let sink_concrete = Arc::new(sink);
    let sink_dyn: Arc<dyn nostos_application::ports::EventSink> =
        Arc::clone(&sink_concrete) as Arc<dyn nostos_application::ports::EventSink>;
    let session = SyncSession::new(predicate);

    // 3. Register with the store via the manager.
    let manager = Arc::clone(&state.manager);
    let id = manager.connect(session, sink_dyn).await;

    // 4. Drain the sink onto the wire until the client goes away.
    let codec = Arc::clone(&state.codec);
    let closed = Arc::new(Notify::new());
    let closed_tx = Arc::clone(&closed);

    let write_loop = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let frame = codec.encode(&event);
            // `frame` is already `Vec<u8>`; `Binary::new` takes it directly.
            if socket.send(Message::Binary(frame)).await.is_err() {
                break; // client gone
            }
        }
        let _ = socket;
        closed_tx.notify_waiters();
    });

    // Keep the session alive until the writer ends, then clean up.
    closed.notified().await;
    // Mark the sink closed so the router stops delivering (concrete ref).
    sink_concrete.close();
    manager.disconnect(id).await;
    let _ = write_loop.await;
}

/// Await the first text frame and parse it as `Subscribe`.
async fn read_subscribe(socket: &mut WebSocket) -> Option<Predicate> {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(t) => {
                return serde_json::from_str::<Subscribe>(&t)
                    .ok()
                    .map(|s| s.to_predicate());
            }
            Message::Binary(b) => {
                return serde_json::from_slice::<Subscribe>(&b)
                    .ok()
                    .map(|s| s.to_predicate());
            }
            Message::Close(_) => return None,
            _ => {} // ping/pong — loop
        }
    }
    None
}

// (Transport-swap seam removed — axum 0.7's `WebSocket` works directly. If we
// later swap to WebTransport, this module is the single place that changes.)
