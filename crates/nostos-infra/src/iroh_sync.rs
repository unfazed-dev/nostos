//! ADR-0041 SPIKE: the iroh transport — accept loop + ticket URL.
//!
//! Scope of the spike (per ADR-0041 D4): prove the transport seam end to end
//! — a sync client dials an iroh:// URL and completes a full session against
//! a nostos server — while the session core stays byte-identical.
//!
//! ## How the spike bridges (and what the native end-state replaces)
//!
//! The sync session core (run_session in transport.rs) is typed on axum's
//! WebSocket, which can only be produced by an HTTP upgrade. Rather than
//! refactor the core onto a transport trait inside the spike, each accepted
//! iroh bidirectional stream is BRIDGED to the server's own loopback HTTP
//! listener as raw bytes: the client runs the full WebSocket handshake over
//! the iroh stream (tokio-tungstenite over any AsyncRead+AsyncWrite), those
//! bytes surface on the loopback TCP connection, and axum upgrades it exactly
//! as if the client had dialed TCP directly. This is the same bridging shape
//! arxa studio's desktop pairing uses in production today (one iroh stream
//! per HTTP exchange), so the pattern has field mileage.
//!
//! ponytail: the bridge costs one loopback TCP hop per connection — fine for
//! the spike's proof, but the native end-state (if the ADR is accepted) is a
//! run_session refactor onto a small frame-io trait so iroh streams drive the
//! session core directly, deleting the hop. The conformance legs in
//! nostos-client pin the BEHAVIOR so that refactor is a pure internal change.
//!
//! ## Addressing (ADR-0041 D4 §4)
//!
//! The server prints an iroh:// URL carrying the node id plus a
//! urlencoded EndpointTicket (relay + direct-address hints). This is
//! QR-native: the arxa pairing QR already carries exactly this ticket.

#![cfg(feature = "iroh")]

use iroh::endpoint::{presets, Endpoint};
use iroh_tickets::endpoint::EndpointTicket;
use tracing::{debug, warn};

/// The ALPN every nostos sync participant registers/dials (ADR-0041 §2).
pub const NOSTOS_SYNC_ALPN: &[u8] = b"cairn/sync/1";

/// Bind a sync endpoint with the default n0 preset (relay + discovery) and
/// register the nostos sync ALPN. The returned handle exposes the dial URL.
pub async fn bind_sync_endpoint() -> Result<SyncEndpoint, iroh::endpoint::BindError> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![NOSTOS_SYNC_ALPN.to_vec()])
        .bind()
        .await?;
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

    /// Serve until the endpoint errors: accept connections, one bridge task
    /// per bidirectional stream, piping raw bytes to the loopback HTTP
    /// listener at http_addr (where the sync route is mounted).
    pub async fn serve_bridge(&self, http_addr: std::net::SocketAddr) {
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
            let http = http_addr;
            tokio::spawn(async move {
                while let Ok((send, recv)) = conn.accept_bi().await {
                    tokio::spawn(async move {
                        if let Err(e) = bridge_stream(send, recv, http).await {
                            debug!(error = %e, "iroh sync stream bridge ended");
                        }
                    });
                }
            });
        }
    }
}

/// Pipe one iroh bidirectional stream against one fresh loopback TCP
/// connection. Byte-opaque in both directions; the WebSocket (and its
/// upgrade) rides inside, terminated by axum on the TCP side.
async fn bridge_stream(
    send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    http_addr: std::net::SocketAddr,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut send = send;
    let mut tcp = tokio::net::TcpStream::connect(http_addr).await?;
    let (mut tcp_read, mut tcp_write) = tcp.split();

    let uplink = async {
        tokio::io::copy(&mut tcp_read, &mut send).await?;
        let _ = send.finish();
        Ok::<(), std::io::Error>(())
    };
    let downlink = async {
        tokio::io::copy(&mut recv, &mut tcp_write).await?;
        tcp_write.shutdown().await?;
        Ok::<(), std::io::Error>(())
    };
    tokio::try_join!(uplink, downlink).map(|_| ())
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
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(urlencode("plain-AZ_09.~"), "plain-AZ_09.~");
    }
}
