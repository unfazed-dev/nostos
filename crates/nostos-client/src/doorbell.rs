//! Direct mode's doorbell — a Supabase Realtime private channel over a raw
//! WebSocket.
//!
//! A ring means **"call `rpc/pull`"** and nothing else. That is the load-bearing
//! design choice, not a simplification: because the channel carries no rows,
//! native (this module) and web (`realtime-js` in the Worker, ADR-0033) never
//! have to agree on payload decoding, and a lost ring costs staleness until the
//! next pull rather than a missing row. Same position as ADR-0037 and
//! `docs/STRATEGY.md:214` — push is a wake-up trigger, not a data channel.
//!
//! ## Why a trigger-side `realtime.send()` and not `postgres_changes`
//!
//! Realtime reads broadcast messages out of the WAL, so a `realtime.send()`
//! fired from the change-log trigger cannot be delivered before the writing
//! transaction commits — post-commit by construction, no outbox poller, no 2PC.
//! Supabase's own guidance is to prefer broadcast-from-trigger because
//! `postgres_changes` authorizes once per subscriber on a single ordering
//! thread. See `docs/plans/direct-mode-push-and-presence.md`.
//!
//! ## Protocol
//!
//! Wire format is Realtime protocol `vsn=1.0.0`: JSON text frames with
//! `topic` / `event` / `payload` / `ref` / `join_ref`. `2.0.0` exists and uses
//! arrays plus binary frames; `1.0.0` is the server default and keeps the
//! debuggable-JSON convention this project holds to everywhere else.
//!
//! Facts below are from the Realtime Protocol reference, not from memory:
//!
//! - Connect to `…/realtime/v1/websocket?apikey=<key>` (hosted) or
//!   `…/socket/websocket?apikey=<key>` (self-hosted).
//! - `heartbeat` "should be sent at least every 25 seconds to avoid a
//!   connection timeout", on the special topic `phoenix`, payload `{}`.
//! - `access_token` refreshes the JWT in-band with no rejoin. **There is no
//!   reply on success**, and **tokens with an `sb_*` prefix are silently
//!   ignored by the server** — that one is a trap worth naming: the refresh
//!   appears to work and the channel dies at the old token's expiry.
//! - A rejected `phx_join` replies with `response.reason` as
//!   `"<ErrorCode>: <message>"`, after a deliberate server-side backoff delay,
//!   so a tight retry loop is worse than useless.
//! - Channel-level `system` errors have **no machine-readable code** — the docs
//!   say to match on the `message` text — and are always followed by
//!   `phx_close`.
//!
//! ## Private channels need the project setting, not just the policy
//!
//! `private: true` is authorized by RLS policies on `realtime.messages`
//! discriminating on the `extension` column. It only *enforces* anything once
//! "Allow public access" is off in Realtime Settings; with it on, anyone
//! holding the anon key can join any public topic with no policy check.
//! `nostos doctor` checks the setting — a policy alone is not a boundary.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// How often the heartbeat goes out. The documented ceiling is 25 s; 20 s
/// leaves room for a slow radio without doubling the traffic.
const HEARTBEAT: Duration = Duration::from_secs(20);

/// Why the doorbell stopped listening.
#[derive(Debug, thiserror::Error)]
pub enum DoorbellError {
    /// The URL could not be turned into a Realtime WebSocket endpoint.
    #[error("invalid Realtime base url {0:?}")]
    BadUrl(String),

    /// Socket-level failure: dial, TLS, or a dropped connection. Reconnect.
    #[error("doorbell socket error: {0}")]
    Socket(String),

    /// The server rejected the join or killed the channel. `fatal` follows the
    /// documented error tables: auth-invalid and config errors are
    /// do-not-retry, rate limits and database errors are backoff-and-retry.
    #[error("doorbell rejected: {reason}")]
    Rejected {
        /// The server's `reason` / `message` text, verbatim.
        reason: String,
        /// `true` when retrying cannot help — surface it to the caller instead.
        fatal: bool,
    },

    /// The channel closed cleanly (`phx_close` with no preceding error).
    #[error("doorbell channel closed")]
    Closed,
}

impl DoorbellError {
    /// Whether reconnecting can plausibly help.
    ///
    /// Note that even a retryable join error must be backed off: the server
    /// already delays its rejection reply on purpose.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::BadUrl(_) => false,
            Self::Socket(_) | Self::Closed => true,
            Self::Rejected { fatal, .. } => !fatal,
        }
    }
}

/// Where the doorbell listens, and as whom.
#[derive(Debug, Clone)]
pub struct DoorbellConfig {
    /// The full WebSocket URL including `apikey` and `vsn`.
    url: String,
    /// The Realtime topic, already prefixed with `realtime:`.
    topic: String,
    /// The JWT presented for authorization. The anon key works and gets
    /// whatever RLS grants `anon` on `realtime.messages`, usually nothing.
    token: String,
}

impl DoorbellConfig {
    /// Build a config from a Supabase project URL
    /// (`https://<ref>.supabase.co`) and a scope.
    ///
    /// `scope` is the same tenant/owner value the change-log trigger stamps, so
    /// the channel a device listens on is the set of rows it can pull. The
    /// `nostos:` prefix keeps Nostos's topics from colliding with the app's own.
    pub fn new(
        base_url: &str,
        apikey: &str,
        scope: &str,
        token: impl Into<String>,
    ) -> Result<Self, DoorbellError> {
        let base = base_url.trim_end_matches('/');
        let ws_base = match base.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some(("http", rest)) => format!("ws://{rest}"),
            Some(("wss" | "ws", _)) => base.to_string(),
            _ => return Err(DoorbellError::BadUrl(base_url.to_string())),
        };
        Ok(Self {
            // vsn is pinned rather than defaulted: 1.0.0 is today's server
            // default, but relying on a default to stay put is how a wire
            // format changes under you.
            url: format!("{ws_base}/realtime/v1/websocket?apikey={apikey}&vsn=1.0.0"),
            topic: format!("realtime:nostos:{scope}"),
            token: token.into(),
        })
    }

    /// The topic being listened on (for logs and `nostos doctor`).
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The `phx_join` frame: a private channel, presence off, no echo of our
    /// own sends (we never send any).
    #[must_use]
    pub fn join_frame(&self) -> String {
        serde_json::json!({
            "topic": self.topic,
            "event": "phx_join",
            "ref": "1",
            "join_ref": "1",
            "payload": {
                "config": {
                    "broadcast": { "ack": false, "self": false },
                    "presence": { "enabled": false },
                    // Without this the channel is public and the RLS policies
                    // on realtime.messages are never consulted.
                    "private": true,
                },
                "access_token": self.token,
            }
        })
        .to_string()
    }

    /// The in-band JWT refresh frame. No reply arrives on success.
    #[must_use]
    pub fn access_token_frame(&self, token: &str, seq: u64) -> String {
        serde_json::json!({
            "topic": self.topic,
            "event": "access_token",
            "ref": seq.to_string(),
            "join_ref": "1",
            "payload": { "access_token": token }
        })
        .to_string()
    }
}

/// The heartbeat frame. Topic `phoenix`, empty payload, no `join_ref`.
#[must_use]
fn heartbeat_frame(seq: u64) -> String {
    serde_json::json!({
        "topic": "phoenix",
        "event": "heartbeat",
        "ref": seq.to_string(),
        "payload": {}
    })
    .to_string()
}

/// What one inbound frame means to us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// A broadcast arrived: call `rpc/pull`. The payload is deliberately
    /// ignored — the channel carries no rows.
    Ring,
    /// The join succeeded (or a heartbeat was acknowledged).
    Ok,
    /// The server rejected or closed the channel.
    Rejected {
        /// Verbatim server text.
        reason: String,
        /// Retrying cannot help.
        fatal: bool,
    },
    /// `phx_close` with no error before it.
    Closed,
    /// Something we do not act on (`presence_*`, unknown events). Ignored
    /// rather than treated as an error — an unrecognised frame is not a fault.
    Ignored,
}

/// Classify one inbound Realtime text frame (`vsn=1.0.0`).
///
/// Pure, so the whole error matrix is host-testable without a socket — the same
/// split `nostos-ffi-wasm`'s `transport.rs` uses (frame logic pure, socket owned
/// by the platform).
#[must_use]
pub fn classify(text: &str) -> Inbound {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        // Undecodable frame. Not fatal, not a ring: a doorbell we cannot read
        // is indistinguishable from one that never came, and the next pull
        // catches up regardless.
        return Inbound::Ignored;
    };
    let event = v
        .get("event")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let payload = v.get("payload");
    match event {
        "broadcast" => Inbound::Ring,
        "phx_reply" => {
            let status = payload
                .and_then(|p| p.get("status"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if status == "ok" {
                return Inbound::Ok;
            }
            let reason = payload
                .and_then(|p| p.get("response"))
                .and_then(|r| r.get("reason"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown join error");
            Inbound::Rejected {
                reason: reason.to_string(),
                fatal: join_error_is_fatal(reason),
            }
        }
        "system" => {
            let status = payload
                .and_then(|p| p.get("status"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if status != "error" {
                return Inbound::Ok;
            }
            let message = payload
                .and_then(|p| p.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown system error");
            Inbound::Rejected {
                reason: message.to_string(),
                fatal: system_error_is_fatal(message),
            }
        }
        "phx_error" => Inbound::Rejected {
            reason: "phx_error".to_string(),
            fatal: false,
        },
        "phx_close" => Inbound::Closed,
        _ => Inbound::Ignored,
    }
}

/// Join errors that no retry can fix, per the protocol reference's own
/// "Action" column: auth-invalid and config are do-not-retry; expired tokens,
/// rate limits, database and transient errors are retryable.
fn join_error_is_fatal(reason: &str) -> bool {
    const FATAL: [&str; 7] = [
        "MalformedJWT",
        "JwtSignatureError",
        "Unauthorized",
        "TopicNameRequired",
        "TenantNotFound",
        "RealtimeDisabledForTenant",
        "RealtimeDisabledForConfiguration",
    ];
    // The reason is "<ErrorCode>: <message>"; `UnknownErrorOnChannel` is
    // documented to arrive without the prefix, and it is retryable anyway.
    let code = reason.split_once(':').map_or(reason, |(c, _)| c).trim();
    FATAL.contains(&code)
}

/// Channel-level `system` errors carry no code field — the docs say to match on
/// the message text. Only a malformed token is unfixable by retrying; an
/// expired one is fixed by refreshing and rejoining.
fn system_error_is_fatal(message: &str) -> bool {
    message.contains("required in JWT")
}

/// Listen on the private channel, sending `()` on `rings` for every broadcast.
///
/// Returns when the channel closes or errors — reconnect policy belongs to the
/// caller, which also owns the backoff. Every reconnect should pull
/// unconditionally: a ring missed while disconnected is invisible, and RxDB
/// names the same rule ("`pullStream$` should also emit a RESYNC event each
/// time the client reconnects").
///
/// Rings are coalesced by the channel's capacity rather than queued: a full
/// `rings` buffer drops the ring, which is correct, because N rings and one
/// ring produce the same pull from the same horizon.
pub async fn listen(
    config: &DoorbellConfig,
    rings: mpsc::Sender<()>,
) -> Result<std::convert::Infallible, DoorbellError> {
    // tungstenite uses the process-level provider; with both `ring` and
    // `aws-lc-rs` compiled in, rustls refuses to guess and panics. Installing
    // is once-per-process; a second call returns Err, which is the same thing.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ws, _) = tokio_tungstenite::connect_async(&config.url)
        .await
        .map_err(|e| DoorbellError::Socket(e.to_string()))?;
    let (mut write, mut read) = ws.split();

    write
        .send(Message::Text(config.join_frame()))
        .await
        .map_err(|e| DoorbellError::Socket(e.to_string()))?;

    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.tick().await; // the first tick is immediate; the join just went out
    let mut seq: u64 = 1;

    loop {
        tokio::select! {
            _ = beat.tick() => {
                seq += 1;
                write
                    .send(Message::Text(heartbeat_frame(seq)))
                    .await
                    .map_err(|e| DoorbellError::Socket(e.to_string()))?;
            }
            frame = read.next() => {
                let Some(frame) = frame else {
                    return Err(DoorbellError::Closed);
                };
                let frame = frame.map_err(|e| DoorbellError::Socket(e.to_string()))?;
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(_) => return Err(DoorbellError::Closed),
                    // Realtime's own ping/pong is handled by tungstenite; any
                    // other frame type is not part of vsn 1.0.0.
                    _ => continue,
                };
                match classify(&text) {
                    // `try_send` deliberately: a full buffer means a pull is
                    // already pending, and a second one would find nothing.
                    Inbound::Ring => { let _ = rings.try_send(()); }
                    Inbound::Ok | Inbound::Ignored => {}
                    Inbound::Closed => return Err(DoorbellError::Closed),
                    Inbound::Rejected { reason, fatal } => {
                        return Err(DoorbellError::Rejected { reason, fatal });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DoorbellConfig {
        DoorbellConfig::new("https://ref.supabase.co", "anon-key", "tenant-7", "jwt").unwrap()
    }

    #[test]
    fn the_url_becomes_wss_and_pins_the_protocol_version() {
        let c = cfg();
        assert_eq!(
            c.url,
            "wss://ref.supabase.co/realtime/v1/websocket?apikey=anon-key&vsn=1.0.0"
        );
        assert_eq!(c.topic(), "realtime:nostos:tenant-7");
        assert!(matches!(
            DoorbellConfig::new("ftp://nope", "k", "s", "t"),
            Err(DoorbellError::BadUrl(_))
        ));
    }

    #[test]
    fn the_join_frame_is_private_with_presence_off() {
        let v: serde_json::Value = serde_json::from_str(&cfg().join_frame()).unwrap();
        assert_eq!(v["event"], "phx_join");
        assert_eq!(v["topic"], "realtime:nostos:tenant-7");
        // Without `private: true` the RLS policies are never consulted.
        assert_eq!(v["payload"]["config"]["private"], true);
        assert_eq!(v["payload"]["config"]["presence"]["enabled"], false);
        assert_eq!(v["payload"]["config"]["broadcast"]["self"], false);
        assert_eq!(v["payload"]["access_token"], "jwt");
        // phx_join requires both ref and join_ref.
        assert!(v["ref"].is_string() && v["join_ref"].is_string());
    }

    #[test]
    fn the_heartbeat_goes_to_phoenix_with_a_ref_and_no_join_ref() {
        let v: serde_json::Value = serde_json::from_str(&heartbeat_frame(7)).unwrap();
        assert_eq!(v["topic"], "phoenix");
        assert_eq!(v["event"], "heartbeat");
        assert_eq!(v["ref"], "7");
        assert_eq!(v["payload"], serde_json::json!({}));
        assert!(v.get("join_ref").is_none(), "heartbeat takes no join_ref");
    }

    #[test]
    fn the_access_token_frame_carries_ref_and_join_ref() {
        let v: serde_json::Value =
            serde_json::from_str(&cfg().access_token_frame("fresher.jwt", 9)).unwrap();
        assert_eq!(v["event"], "access_token");
        assert_eq!(v["payload"]["access_token"], "fresher.jwt");
        assert_eq!(v["join_ref"], "1");
        assert_eq!(v["ref"], "9");
    }

    #[test]
    fn any_broadcast_is_a_ring_whatever_it_carries() {
        // The channel is contentless by design, so the payload must not matter.
        for body in [
            r#"{"topic":"realtime:nostos:t","event":"broadcast","payload":{"event":"nostos","type":"broadcast","payload":{}}}"#,
            r#"{"topic":"realtime:nostos:t","event":"broadcast","payload":{"event":"whatever","type":"broadcast","payload":{"table":"orders"}}}"#,
        ] {
            assert_eq!(classify(body), Inbound::Ring);
        }
    }

    #[test]
    fn a_successful_join_reply_is_ok() {
        let body = r#"{"topic":"realtime:nostos:t","event":"phx_reply","payload":{"status":"ok","response":{}},"ref":"1"}"#;
        assert_eq!(classify(body), Inbound::Ok);
    }

    #[test]
    fn join_errors_split_into_retryable_and_fatal_per_the_docs() {
        let cases = [
            (
                "InvalidJWTExpiration: Token has expired 300 seconds ago",
                false,
            ),
            ("ChannelRateLimitReached: slow down", false),
            ("IncreaseConnectionPool: no db connections", false),
            ("RealtimeRestarting: try later", false),
            ("Unauthorized: no policy match", true),
            ("MalformedJWT: bad token", true),
            ("JwtSignatureError: bad signature", true),
            ("TenantNotFound: nope", true),
            ("RealtimeDisabledForTenant: off", true),
            // Documented to arrive with no "<Code>: " prefix.
            ("Unknown Error on Channel", false),
        ];
        for (reason, expect_fatal) in cases {
            let body = serde_json::json!({
                "event": "phx_reply",
                "payload": { "status": "error", "response": { "reason": reason } }
            })
            .to_string();
            match classify(&body) {
                Inbound::Rejected { fatal, .. } => {
                    assert_eq!(fatal, expect_fatal, "{reason}");
                }
                other => panic!("{reason} → {other:?}"),
            }
        }
    }

    #[test]
    fn system_errors_are_matched_on_message_text_because_there_is_no_code() {
        // Expired: refresh and rejoin, so retryable.
        let expired = serde_json::json!({
            "event": "system",
            "payload": { "status": "error", "extension": "system",
                         "message": "Token has expired", "channel": "realtime:nostos:t" }
        })
        .to_string();
        assert_eq!(
            classify(&expired),
            Inbound::Rejected {
                reason: "Token has expired".to_string(),
                fatal: false
            }
        );

        // Claims missing: the token issuer is wrong; retrying cannot fix it.
        let claims = serde_json::json!({
            "event": "system",
            "payload": { "status": "error",
                         "message": "Fields `role` and `exp` are required in JWT" }
        })
        .to_string();
        assert!(matches!(
            classify(&claims),
            Inbound::Rejected { fatal: true, .. }
        ));
    }

    #[test]
    fn close_and_junk_are_distinguished() {
        assert_eq!(
            classify(r#"{"event":"phx_close","payload":{}}"#),
            Inbound::Closed
        );
        assert_eq!(classify("not json at all"), Inbound::Ignored);
        assert_eq!(classify(r#"{"event":"presence_diff"}"#), Inbound::Ignored);
    }

    #[test]
    fn retryability_matches_the_error_kind() {
        assert!(!DoorbellError::BadUrl("x".into()).is_retryable());
        assert!(DoorbellError::Socket("reset".into()).is_retryable());
        assert!(DoorbellError::Closed.is_retryable());
        assert!(!DoorbellError::Rejected {
            reason: "Unauthorized: no".into(),
            fatal: true
        }
        .is_retryable());
    }

    /// Every Supabase Realtime URL is `wss://`. Without a TLS feature on
    /// tokio-tungstenite the TCP dial succeeds, the TLS wrap fails with
    /// "TLS support not compiled in", `listen` returns a retryable `Socket`
    /// error, and `run` degrades to the 30 s backoff resync with no visible
    /// symptom (measured on atlet iOS 2026-09-25: pull cadence matched the
    /// backoff exactly and no `/realtime/v1/websocket` hit ever landed).
    #[tokio::test]
    async fn wss_is_refused_by_the_peer_not_by_a_missing_tls_feature() {
        // A loopback listener that drops every connection: a TLS-capable
        // build gets as far as the handshake and fails on the closed stream.
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = server.accept().await {
                drop(stream);
            }
        });
        let config =
            DoorbellConfig::new(&format!("https://127.0.0.1:{port}"), "anon", "sub:x", "jwt")
                .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let err = tokio::time::timeout(Duration::from_secs(10), listen(&config, tx))
            .await
            .expect("listen hung on a dropped stream")
            .unwrap_err();
        let DoorbellError::Socket(msg) = err else {
            panic!("expected Socket, got {err:?}");
        };
        assert!(
            !msg.contains("TLS support not compiled in"),
            "tokio-tungstenite has no TLS feature: {msg}"
        );
    }
}
