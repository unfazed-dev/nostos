//! ADR-0041: client-side iroh dial-by-scheme.
//!
//! run_once's session loop is generic over the tungstenite message halves;
//! this module produces EXACTLY those halves over an iroh bidirectional
//! stream: dial iroh://<node><path>?ticket=..., connect, open_bi, wrap the
//! halves as one AsyncRead+AsyncWrite (`BiStream`, shared with the server's
//! accept loop via nostos-infra), then run the standard WebSocket client
//! handshake over it (tokio-tungstenite works over any stream). The server's
//! iroh accept loop runs the matching server handshake in place and drives
//! the same session core the TCP/WS path drives (D6) — no loopback hop.
//!
//! The wire contract is untouched: the token query param rides the HTTP
//! request exactly as the TCP path does (JWT on connect frame, ADR-0018).

#![cfg(feature = "iroh")]

use std::pin::Pin;

use iroh::endpoint::{presets, Endpoint};
use iroh_tickets::endpoint::EndpointTicket;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::WebSocketStream;

use nostos_infra::iroh_sync::{BiStream, NOSTOS_SYNC_ALPN};

/// One process-wide client endpoint (rebinding per reconnect churns the node
/// id and re-runs relay discovery; a stable endpoint is also what a future
/// direct-connection cache wants). Binding is lazy + once.
fn client_endpoint() -> &'static tokio::sync::OnceCell<Endpoint> {
    static EP: tokio::sync::OnceCell<Endpoint> = tokio::sync::OnceCell::const_new();
    &EP
}

async fn endpoint() -> Result<&'static Endpoint, String> {
    client_endpoint()
        .get_or_try_init(|| async {
            Endpoint::bind(presets::N0)
                .await
                .map_err(|e| format!("iroh endpoint bind failed: {e}"))
        })
        .await
        .map_err(|e| e.clone())
}

/// Dial an iroh:// sync URL and complete the WebSocket handshake over the
/// stream. Returns the same stream/sink shape a TCP dial produces.
pub async fn dial_sync_ws(url: &str) -> Result<WebSocketStream<BiStream>, String> {
    let parsed = parse(url)?;

    // The HTTP request the server's axum router sees: path + query WITHOUT
    // the ticket param (the ticket is addressing, not request state).
    let mut query: Vec<String> = parsed
        .others
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    query.sort();
    let qs = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let path = if parsed.path.is_empty() {
        "/sync".to_string()
    } else {
        parsed.path.clone()
    };
    let req = format!("ws://iroh.internal{path}{qs}")
        .into_client_request()
        .map_err(|e| format!("handshake request build failed: {e}"))?;

    let ticket_raw = parsed.ticket.ok_or_else(|| {
        "iroh:// URL carries no ticket param — cannot resolve the peer's          relay/direct hints (the server prints the full URL; use it verbatim)"
            .to_string()
    })?;
    let ticket: EndpointTicket = ticket_raw
        .parse()
        .map_err(|e| format!("invalid iroh ticket in URL: {e}"))?;
    let addr = ticket.endpoint_addr().clone();

    let ep = endpoint().await?;
    let conn = ep
        .connect(addr, NOSTOS_SYNC_ALPN)
        .await
        .map_err(|e| format!("iroh connect failed: {e}"))?;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("iroh open_bi failed: {e}"))?;

    let (ws, _resp) = tokio_tungstenite::client_async(req, BiStream { send, recv })
        .await
        .map_err(|e| format!("websocket handshake over iroh failed: {e}"))?;
    Ok(ws)
}

/// The parsed pieces of an iroh:// sync URL.
struct ParsedUrl {
    path: String,
    ticket: Option<String>,
    others: Vec<(String, String)>,
}

fn parse(url: &str) -> Result<ParsedUrl, String> {
    let rest = url
        .strip_prefix("iroh://")
        .ok_or_else(|| format!("not an iroh:// URL: {url}"))?;
    let (auth_path, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let path = match auth_path.find('/') {
        Some(i) => auth_path[i..].to_string(),
        None => String::new(),
    };
    let mut ticket = None;
    let mut others = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // The ticket is percent-encoded by the server (iroh_sync::urlencode);
        // everything else is plain.
        if k == "ticket" {
            ticket = Some(percent_decode(v));
        } else {
            others.push((k.to_string(), v.to_string()));
        }
    }
    Ok(ParsedUrl {
        path,
        ticket,
        others,
    })
}

/// Decode the server's minimal percent-encoding (see iroh_sync::urlencode).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'A'..=b'F' => Some(b - b'A' + 10),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    };
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The unified socket the session loop dials: whichever transport won.
/// Implements the exact Stream+Sink bounds the loop (and its `split()`)
/// already required of a tungstenite WebSocket, delegating per variant —
/// the loop stays byte-for-byte transport-agnostic.
pub enum SyncWs {
    Tcp(
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ),
    Iroh(WebSocketStream<BiStream>),
}

impl futures_util::Stream for SyncWs {
    type Item =
        Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match &mut *self {
            SyncWs::Tcp(ws) => Pin::new(ws).poll_next(cx),
            SyncWs::Iroh(ws) => Pin::new(ws).poll_next(cx),
        }
    }
}

impl futures_util::Sink<tokio_tungstenite::tungstenite::Message> for SyncWs {
    type Error = tokio_tungstenite::tungstenite::Error;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match &mut *self {
            SyncWs::Tcp(ws) => Pin::new(ws).poll_ready(cx),
            SyncWs::Iroh(ws) => Pin::new(ws).poll_ready(cx),
        }
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: tokio_tungstenite::tungstenite::Message,
    ) -> Result<(), Self::Error> {
        match &mut *self {
            SyncWs::Tcp(ws) => Pin::new(ws).start_send(item),
            SyncWs::Iroh(ws) => Pin::new(ws).start_send(item),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match &mut *self {
            SyncWs::Tcp(ws) => Pin::new(ws).poll_flush(cx),
            SyncWs::Iroh(ws) => Pin::new(ws).poll_flush(cx),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match &mut *self {
            SyncWs::Tcp(ws) => Pin::new(ws).poll_close(cx),
            SyncWs::Iroh(ws) => Pin::new(ws).poll_close(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_path_ticket_and_others() {
        let p = parse("iroh://abc123/sync?ticket=a%20b&token=tok").unwrap();
        assert_eq!(p.path, "/sync");
        assert_eq!(p.ticket.as_deref(), Some("a b"));
        assert_eq!(p.others, vec![("token".to_string(), "tok".to_string())]);
    }

    #[test]
    fn parse_defaults_empty_path_and_rejects_non_iroh() {
        let p = parse("iroh://node").unwrap();
        assert_eq!(p.path, "");
        assert!(parse("ws://host/sync").is_err());
    }
}
