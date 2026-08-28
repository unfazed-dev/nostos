//! ADR-0041: the iroh transport — endpoint, QR-native ticket URL, and the
//! native accept loop.
//!
//! One client session = one QUIC connection = one bidirectional stream
//! (ADR-0041 §2). Each accepted stream runs the standard WebSocket server
//! handshake IN PLACE (tungstenite works over any `AsyncRead + AsyncWrite`),
//! then drives the SAME session core the axum `/sync` handler drives —
//! `crate::transport::run_session` is generic over the frame `Stream`/`Sink`
//! (D6). No loopback hop, no HTTP in the session path; the HTTP listener
//! keeps only the ops surface (healthz/metrics/schema/push REST).
//!
//! Auth parity with `sync_handler`: the handshake request is captured via
//! `accept_hdr_async` and the token check is the same policy (Authorization
//! header, else `?token=`, else `""` → the anonymous principal). The verifier
//! is async, so a rejected token is answered with a `4401` close frame right
//! after the handshake — where the axum path answers a pre-upgrade 401 —
//! and no session state is ever created.
//!
//! ## Addressing (ADR-0041 §4)
//!
//! The server prints an iroh:// URL carrying the node id plus a
//! urlencoded EndpointTicket (relay + direct-address hints). This is
//! QR-native: the arxa pairing QR already carries exactly this ticket.

#![cfg(feature = "iroh")]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::extract::ws::{CloseFrame as WsClose, Message as WsMessage};
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use iroh::endpoint::{presets, Endpoint, RecvStream, SendStream};
use iroh::{RelayMode, RelayUrl};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, warn};

use crate::transport::{bearer_token, run_session, AuthQuery, SyncRouterState};

/// The ALPN every nostos sync participant registers/dials (ADR-0041 §2).
pub const NOSTOS_SYNC_ALPN: &[u8] = b"cairn/sync/1";

/// Parse the optional `NOSTOS_IROH_RELAY_URL` value (ADR-0041 D8): the URL of
/// a self-hosted iroh relay that REPLACES the n0 default relay fleet for the
/// sync endpoint (`RelayMode::Custom`). Unset/empty/whitespace → `None` (keep
/// the n0 defaults). An unparseable value is a startup-fatal config error
/// (`Err`), never silently ignored — the operator runbook pattern
/// (docs/OPERATING.md §1).
pub fn parse_relay_url(value: Option<&str>) -> Result<Option<RelayUrl>, String> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    raw.parse::<RelayUrl>()
        .map(Some)
        .map_err(|e| format!("invalid NOSTOS_IROH_RELAY_URL {raw:?}: {e}"))
}

/// Bind a sync endpoint with the default n0 preset (relay + discovery) and
/// register the nostos sync ALPN. The returned handle exposes the dial URL.
///
/// `relay_url`: `Some(url)` swaps the n0 default relay fleet for that
/// self-hosted relay (`RelayMode::Custom`); the printed dial URL's ticket
/// then carries it, and stock clients dial straight through (iroh's relay
/// transport treats any peer-address relay URL as dialable).
// ponytail: `presets::N0` keeps n0's iroh.link DNS/pkarr discovery
// (publish + resolve) even when a custom relay replaces the fleet — the QR
// ticket carries the relay/addr hints, so pairing never depends on
// discovery. A discovery cut-off knob (`clear_address_lookup`) lands only if
// an operator asks for zero third-party contact; docs/OPERATING.md §9.
pub async fn bind_sync_endpoint(
    relay_url: Option<RelayUrl>,
) -> Result<SyncEndpoint, iroh::endpoint::BindError> {
    let builder = Endpoint::builder(presets::N0).alpns(vec![NOSTOS_SYNC_ALPN.to_vec()]);
    let builder = match relay_url {
        Some(url) => builder.relay_mode(RelayMode::custom([url])),
        None => builder,
    };
    let endpoint = builder.bind().await?;
    Ok(SyncEndpoint { endpoint })
}

/// A bound iroh sync endpoint.
#[derive(Clone, Debug)]
pub struct SyncEndpoint {
    endpoint: Endpoint,
}

impl SyncEndpoint {
    /// The dial URL: iroh://<node-id>?ticket=<urlencoded EndpointTicket>.
    /// The ticket carries the relay + direct-address hints; the node id in
    /// the authority is for humans (logs, QR labels) — the ticket is what
    /// the client resolves.
    pub fn url(&self, ws_path: &str) -> String {
        let addr = self.endpoint.addr();
        let node = addr.id.to_string();
        let ticket = EndpointTicket::new(addr).to_string();
        format!("iroh://{node}{ws_path}?ticket={}", urlencode(&ticket))
    }

    /// Serve until the endpoint errors: accept connections, accept every
    /// bidirectional stream on each, and drive one sync session per stream.
    pub async fn serve_sessions(&self, state: SyncRouterState) {
        loop {
            let Some(incoming) = self.endpoint.accept().await else {
                debug!("iroh accept loop ended");
                break;
            };
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "iroh incoming connection handshake failed");
                    continue;
                }
            };
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    let state = state.clone();
                    tokio::spawn(async move {
                        handle_stream(state, send, recv).await;
                    });
                }
            });
        }
    }
}

/// One accepted bi-stream: WebSocket server handshake in place (capturing
/// the request so the token check sees the same inputs the axum handler
/// sees), authenticate, then hand the socket to the shared session core.
// result_large_err: tungstenite's accept-callback signature is fixed
// (Result<Response, ErrorResponse>) — not ours to slim.
#[allow(clippy::result_large_err)]
async fn handle_stream(state: SyncRouterState, send: SendStream, recv: RecvStream) {
    let captured: Arc<Mutex<Option<tungstenite::handshake::server::Request>>> =
        Arc::new(Mutex::new(None));
    let slot = Arc::clone(&captured);
    let ws = match tokio_tungstenite::accept_hdr_async(
        BiStream { send, recv },
        move |req: &tungstenite::handshake::server::Request,
              resp: tungstenite::handshake::server::Response| {
            *slot.lock().expect("handshake capture mutex poisoned") = Some(req.clone());
            Ok(resp)
        },
    )
    .await
    {
        Ok(ws) => ws,
        Err(e) => {
            debug!(error = %e, "iroh sync websocket handshake failed");
            return;
        }
    };

    // Token policy identical to transport::sync_handler. The captured request
    // is always Some after a successful accept_hdr_async; the `Option` chain
    // keeps a hypothetical miss on the anonymous path rather than panicking.
    let req = captured
        .lock()
        .expect("handshake capture mutex poisoned")
        .take();
    let token = req
        .as_ref()
        .and_then(|r| bearer_token(r.headers()))
        .or_else(|| {
            req.as_ref().and_then(|r| {
                axum::extract::Query::<AuthQuery>::try_from_uri(r.uri())
                    .ok()
                    .and_then(|axum::extract::Query(q)| q.token)
            })
        })
        .unwrap_or_default();
    let Some(principal) = state.auth.authenticate(&token).await else {
        debug!("iroh sync session rejected: authentication failed");
        let mut sock = IrohSessionSocket { inner: ws };
        let frame = WsClose {
            code: 4401,
            reason: "nostos: authentication required for /sync".into(),
        };
        let _ = sock.send(WsMessage::Close(Some(frame))).await;
        return;
    };
    let exp = crate::auth::token_exp(&token);
    run_session(IrohSessionSocket { inner: ws }, state, principal, exp).await;
}

/// A server-side WebSocket over one iroh bi-stream, adapted to the axum
/// frame type `run_session` is typed on. The ONLY translation in the iroh
/// path — framing, ping/pong, and close semantics are tungstenite's either
/// way (axum's `WebSocket` is itself tungstenite underneath).
pub struct IrohSessionSocket {
    inner: WebSocketStream<BiStream>,
}

impl Stream for IrohSessionSocket {
    type Item = Result<WsMessage, tungstenite::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some(to_axum(msg))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<WsMessage> for IrohSessionSocket {
    type Error = tungstenite::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
        Pin::new(&mut self.inner).start_send(to_tungstenite(item))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// tungstenite → axum frame translation (the adapter's read half).
///
/// Both enums hold String/Vec<u8>/Cow payloads at these versions (axum's ws
/// module is itself derived from tungstenite), so only the close CODE needs
/// converting (tungstenite's `CloseCode` enum ↔ axum's u16).
// result_large_err: the error type is tungstenite's own — dictated by its
// API, not ours to box.
#[allow(clippy::result_large_err)]
fn to_axum(msg: tungstenite::Message) -> Result<WsMessage, tungstenite::Error> {
    Ok(match msg {
        tungstenite::Message::Text(t) => WsMessage::Text(t),
        tungstenite::Message::Binary(b) => WsMessage::Binary(b),
        tungstenite::Message::Ping(p) => WsMessage::Ping(p),
        tungstenite::Message::Pong(p) => WsMessage::Pong(p),
        tungstenite::Message::Close(cf) => WsMessage::Close(cf.map(|f| WsClose {
            code: u16::from(f.code),
            reason: f.reason,
        })),
        // Read-side never yields raw frames per tungstenite docs; be loud if
        // that ever changes rather than silently dropping session data.
        tungstenite::Message::Frame(_) => {
            return Err(tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected raw frame on the server read side",
            )));
        }
    })
}

/// axum → tungstenite frame translation (the adapter's write half).
fn to_tungstenite(msg: WsMessage) -> tungstenite::Message {
    match msg {
        WsMessage::Text(t) => tungstenite::Message::Text(t),
        WsMessage::Binary(b) => tungstenite::Message::Binary(b),
        WsMessage::Ping(p) => tungstenite::Message::Ping(p),
        WsMessage::Pong(p) => tungstenite::Message::Pong(p),
        WsMessage::Close(cf) => {
            tungstenite::Message::Close(cf.map(|f| tungstenite::protocol::CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::from(f.code),
                reason: f.reason,
            }))
        }
    }
}

/// One iroh bidirectional stream as a single `AsyncRead + AsyncWrite` object
/// (tungstenite wants one stream; iroh hands out halves). Shared by the
/// client dial (`nostos-client::iroh_dial`) and the server accept loop here.
#[derive(Debug)]
pub struct BiStream {
    /// The write half.
    pub send: SendStream,
    /// The read half.
    pub recv: RecvStream,
}

impl AsyncRead for BiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for BiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // iroh's SendStream exposes inherent poll methods typed with its
        // own WriteError; tokio's AsyncWrite wants io::Error — map across.
        Pin::new(&mut self.send)
            .poll_write(cx, buf)
            .map_err(std::io::Error::other)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send)
            .poll_shutdown(cx)
            .map_err(std::io::Error::other)
    }
}

/// Minimal percent-encoding for the ticket inside a URL query param: escape
/// everything outside unreserved [A-Za-z0-9-_.~] so no ticket byte can
/// terminate or re-shape the URL.
fn urlencode(s: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_relay_url_none_or_empty_keeps_n0_defaults() {
        assert_eq!(parse_relay_url(None).unwrap(), None);
        assert_eq!(parse_relay_url(Some("")).unwrap(), None);
        assert_eq!(parse_relay_url(Some("   ")).unwrap(), None);
    }

    #[test]
    fn parse_relay_url_valid_url_round_trips() {
        let parsed = parse_relay_url(Some("https://relay.example.com"))
            .unwrap()
            .expect("a valid URL must parse");
        assert_eq!(parsed.to_string(), "https://relay.example.com/");
        // Whitespace-padded values trim to the same relay.
        let padded = parse_relay_url(Some("  https://relay.example.com "))
            .unwrap()
            .expect("padded valid URL must parse");
        assert_eq!(padded, parsed);
    }

    #[test]
    fn parse_relay_url_garbage_is_a_named_startup_error() {
        let err = parse_relay_url(Some("not a url")).expect_err("garbage must not parse");
        assert!(
            err.contains("NOSTOS_IROH_RELAY_URL"),
            "error must name the var: {err}"
        );
        assert!(
            err.contains("not a url"),
            "error must echo the value: {err}"
        );
        // A bare host without scheme is a relative URL — also rejected.
        assert!(parse_relay_url(Some("relay.example.com")).is_err());
    }

    #[test]
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(urlencode("plain-AZ_09.~"), "plain-AZ_09.~");
    }

    #[test]
    fn message_translation_round_trips_close_codes() {
        // The writer's contract-critical closes (4401 expiry, 1007 rejects)
        // must survive the axum → tungstenite hop exactly.
        let msg = to_tungstenite(WsMessage::Close(Some(WsClose {
            code: 4401,
            reason: "nostos: token expired".into(),
        })));
        let tungstenite::Message::Close(Some(frame)) = msg else {
            panic!("close frame lost in translation");
        };
        assert_eq!(u16::from(frame.code), 4401);
        assert_eq!(&*frame.reason, "nostos: token expired");
    }
}
