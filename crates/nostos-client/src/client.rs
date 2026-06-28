//! `SyncClient` — the tokio orchestrator that closes the sync loop.
//!
//! Connects to `/sync`, subscribes with the durable `resume_lsn`, feeds every
//! received [`WireFrame`] through the apply engine, and `Ack`s each commit so the
//! server's ack-driven slot advance stays correct (ADR-0009). On disconnect it
//! reconnects with exponential backoff, re-seeding `resume_lsn` from the
//! checkpoint that was flushed to disk on the last commit.
//!
//! ## The receive → apply → ack loop
//!
//! ```text
//!   WS stream ──► decode WireFrame ──► hex-decode payload ──► Frame
//!        ──► ApplyEngine::feed ──► on commit: SqliteStorage::apply_batch (spawn_blocking)
//!        ──► send Ack { lsn = checkpoint }
//! ```
//!
//! The storage is synchronous and may block (SQLite I/O); the apply runs on
//! `spawn_blocking` so the async runtime stays responsive. On WASM (no
//! `spawn_blocking`) the FFI shim runs the engine inline — see ADR-0015.
//!
//! ## Auth
//!
//! The bearer token is passed via `?token=` on the WebSocket URL. Browsers
//! can't set headers on a WS handshake, so the transport accepts the token as a
//! query parameter (ADR-0010); we use the same path for consistency across
//! native + future-FFI clients.
//!
//! ## Reconnect semantics
//!
//! A WebSocket drop is expected (network blip, server restart, eviction). The
//! client treats it as "flush whatever's pending, then reconnect from the
//! durable checkpoint." Because `apply_batch` is atomic and idempotent, a
//! reconnect that re-receives the tail of an already-applied transaction is a
//! no-op — the server's dedup + the client's idempotent upsert converge.

use std::sync::Arc;
use std::time::Duration;

use nostos_core::{ApplyEngine, ApplyOutcome, Frame};
use nostos_domain::Lsn;
use nostos_infra::wire::{ClientMessage, WireFrame};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Configuration for a [`SyncClient`].
#[derive(Debug, Clone)]
pub struct SyncClientConfig {
    /// The table to subscribe to (Phase 0 predicate floor: one table).
    pub table: String,
    /// Optional bearer token, sent as `?token=` on the WS URL.
    pub token: Option<String>,
    /// Base backoff after a disconnect; doubled each retry, capped at `max_backoff`.
    pub base_backoff: Duration,
    /// Maximum backoff between reconnect attempts.
    pub max_backoff: Duration,
    /// Give up after this many consecutive failed reconnects. `None` = forever.
    pub max_retries: Option<u32>,
    /// If no frame arrives for this duration, treat the stream as "caught up"
    /// and return cleanly from [`SyncClient::run_once`] (after a final flush).
    /// `None` = run forever until the server closes the socket. A finite value
    /// is what makes a "sync then disconnect" client deterministic; a
    /// long-lived client leaves this `None` and relies on reconnect-on-drop.
    pub idle_timeout: Option<Duration>,
}

impl Default for SyncClientConfig {
    fn default() -> Self {
        Self {
            table: "tasks".to_owned(),
            token: None,
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            max_retries: None,
            idle_timeout: None,
        }
    }
}

/// The outcome of one session: how many frames were received + the final
/// durable checkpoint. Returned when the stream ends cleanly OR the client gives
/// up reconnecting.
#[derive(Debug, Clone, Copy)]
pub struct SessionOutcome {
    pub frames_received: u64,
    pub commits: u64,
    pub checkpoint: Lsn,
}

/// A Nostos sync client. Owns its storage + apply engine; the engine is held
/// behind a `Mutex` because the apply runs on `spawn_blocking` (a separate
/// thread) while the WS reader stays on the async task.
pub struct SyncClient<S: nostos_core::Storage + Send + 'static> {
    url: String,
    config: SyncClientConfig,
    engine: Arc<Mutex<ApplyEngine<S>>>,
}

impl<S: nostos_core::Storage + Send + 'static> SyncClient<S> {
    /// Build a client targeting `url` (e.g. `ws://127.0.0.1:9999/sync`), with
    /// the given storage backend and config.
    #[must_use]
    pub fn new(url: impl Into<String>, storage: S, config: SyncClientConfig) -> Self {
        let engine = Arc::new(Mutex::new(ApplyEngine::new(storage)));
        Self {
            url: url.into(),
            config,
            engine,
        }
    }

    /// Read the current durable checkpoint (delegates through the engine).
    pub async fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        self.engine.lock().await.checkpoint()
    }

    /// The WS URL to connect to, with `?token=` appended if a token is set.
    fn connect_url(&self) -> String {
        match &self.config.token {
            Some(token) if !token.is_empty() => {
                // Append token as a query param (the transport reads ?token=).
                let sep = if self.url.contains('?') { '&' } else { '?' };
                format!("{}{sep}token={token}", self.url)
            }
            _ => self.url.clone(),
        }
    }

    /// Run one connection attempt to completion: connect, subscribe, apply until
    /// the stream ends or errors. Does NOT reconnect on its own — see
    /// [`Self::run_with_reconnect`]. Returns the session outcome.
    ///
    /// # Errors
    /// Returns the underlying error if the connection can't be established or
    /// the apply loop hits a non-recoverable storage error.
    pub async fn run_once(&self) -> Result<SessionOutcome, ClientError> {
        let url = self.connect_url();
        let (ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let (mut write, mut read) = ws.split();

        // ---- Subscribe with the durable resume_lsn ----
        let resume_lsn = self.checkpoint().await?;
        let subscribe = ClientMessage::Subscribe {
            table: self.config.table.clone(),
            filters: vec![],
            resume_lsn: (resume_lsn > Lsn::ZERO).then_some(resume_lsn.raw()),
        };
        let sub_json = serde_json::to_string(&subscribe).expect("subscribe serializes");
        write
            .send(Message::Text(sub_json))
            .await
            .map_err(|e| ClientError::Send(e.to_string()))?;
        debug!(resume_lsn = resume_lsn.raw(), "subscribed");

        let mut frames_received: u64 = 0;
        let mut commits: u64 = 0;

        // ---- Receive → apply → ack loop ----
        // When `idle_timeout` is set, a gap longer than it means "caught up":
        // break, flush, and return (the "sync then disconnect" shape). Without
        // it we run until the server closes the socket (the long-lived shape).
        loop {
            // When `idle_timeout` is set, a gap longer than it means "caught
            // up": break, flush, and return (the "sync then disconnect" shape).
            // Without it we run until the server closes the socket (long-lived).
            let next = if let Some(idle) = self.config.idle_timeout {
                let Ok(next) = tokio::time::timeout(idle, read.next()).await else {
                    debug!("idle timeout reached; treating stream as caught up");
                    break;
                };
                next
            } else {
                read.next().await
            };

            let Some(msg) = next else { break }; // stream ended
            let msg = msg.map_err(|e| ClientError::Receive(e.to_string()))?;
            let bytes = match msg {
                Message::Text(t) => t.into_bytes(),
                Message::Binary(b) => b,
                Message::Close(_) => {
                    debug!("server sent close; ending session");
                    break;
                }
                // Pings/pongs are handled by tungstenite automatically; ignore.
                _ => continue,
            };

            let frame: WireFrame = match serde_json::from_slice(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    warn!(error = %e, "malformed frame; skipping");
                    continue;
                }
            };
            frames_received += 1;

            // Hex-decode the payload once, at the boundary (the wire carries
            // hex; downstream everything is raw bytes).
            let payload = frame.payload.as_deref().map(decode_hex).and_then(|opt| opt);

            let core_frame = Frame {
                lsn: frame.lsn,
                op: frame.op,
                table: frame.table,
                pk: frame.pk,
                payload,
                txn_id: frame.txn_id,
            };

            // Feed the engine; if this frame triggered a commit, ack it.
            let engine = Arc::clone(&self.engine);
            let outcome =
                tokio::task::spawn_blocking(move || -> nostos_core::Result<Option<ApplyOutcome>> {
                    let mut engine = engine.blocking_lock();
                    engine.feed(core_frame)
                })
                .await
                .map_err(|e| ClientError::Join(e.to_string()))??;

            if let Some(outcome) = outcome {
                commits += 1;
                let ack = ClientMessage::Ack {
                    lsn: outcome.checkpoint.raw(),
                };
                let ack_json = serde_json::to_string(&ack).expect("ack serializes");
                write
                    .send(Message::Text(ack_json))
                    .await
                    .map_err(|e| ClientError::Send(e.to_string()))?;
            }
        }

        // ---- Final flush of any pending batch before returning ----
        let engine = Arc::clone(&self.engine);
        let _ = tokio::task::spawn_blocking(move || -> nostos_core::Result<Option<ApplyOutcome>> {
            let mut engine = engine.blocking_lock();
            engine.flush()
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))??;

        let checkpoint = self.checkpoint().await?;
        info!(
            frames_received,
            commits,
            checkpoint = checkpoint.raw(),
            "session ended"
        );
        Ok(SessionOutcome {
            frames_received,
            commits,
            checkpoint,
        })
    }

    /// Run with reconnect-on-drop: keep reconnecting (with exponential backoff)
    /// until the stream ends cleanly or `max_retries` is exhausted.
    ///
    /// This is the top-level entry point a long-lived client uses. Each call to
    /// [`Self::run_once`] is independent; reconnect re-seeds `resume_lsn` from
    /// the durable checkpoint, so the server skips already-applied frames.
    pub async fn run_with_reconnect(&self) -> Result<SessionOutcome, ClientError> {
        let mut backoff = self.config.base_backoff;
        let mut attempt: u32 = 0;
        let mut total_frames: u64 = 0;
        let mut total_commits: u64 = 0;

        loop {
            attempt += 1;
            match self.run_once().await {
                Ok(outcome) => {
                    total_frames += outcome.frames_received;
                    total_commits += outcome.commits;
                    // A clean end (server-initiated close) means we're done.
                    return Ok(SessionOutcome {
                        frames_received: total_frames,
                        commits: total_commits,
                        checkpoint: outcome.checkpoint,
                    });
                }
                Err(e) => {
                    warn!(attempt, error = %e, "session failed; backing off");
                    if let Some(max) = self.config.max_retries {
                        if attempt >= max {
                            return Err(e);
                        }
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.config.max_backoff);
                }
            }
        }
    }
}

/// Errors from the sync client. Surfaced so a caller can distinguish a
/// transient connect failure (retry) from a fatal storage error (don't retry).
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("send failed: {0}")]
    Send(String),
    #[error("receive failed: {0}")]
    Receive(String),
    #[error("apply task panicked/joined: {0}")]
    Join(String),
    #[error(transparent)]
    Storage(#[from] nostos_core::StorageError),
}

/// Decode a hex string to bytes. The wire payload is hex-encoded (see
/// `nostos_infra::wire`); we decode once at the client boundary.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_domain::Operation;

    #[test]
    fn decode_hex_roundtrips() {
        assert_eq!(decode_hex("6869"), Some(b"hi".to_vec()));
        assert_eq!(decode_hex(""), Some(vec![]));
        assert_eq!(decode_hex("abc"), None); // odd length
        assert_eq!(decode_hex("zz"), None); // non-hex
    }

    #[test]
    fn config_default_is_sane() {
        let c = SyncClientConfig::default();
        assert_eq!(c.table, "tasks");
        assert!(c.token.is_none());
        assert!(c.base_backoff < c.max_backoff);
    }

    #[test]
    fn connect_url_appends_token_as_query() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig {
                token: Some("tok".into()),
                ..SyncClientConfig::default()
            },
        );
        assert_eq!(c.connect_url(), "ws://localhost:9999/sync?token=tok");

        // URL that already has a query string → append with &.
        let c2 = SyncClient::new(
            "ws://localhost:9999/sync?x=1",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig {
                token: Some("tok".into()),
                ..SyncClientConfig::default()
            },
        );
        assert_eq!(c2.connect_url(), "ws://localhost:9999/sync?x=1&token=tok");
    }

    #[test]
    fn connect_url_omits_empty_token() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig {
                token: None,
                ..SyncClientConfig::default()
            },
        );
        assert_eq!(c.connect_url(), "ws://localhost:9999/sync");
    }

    // A compile-time assertion that the operation-enum mapping is exhaustive:
    // the wire's Operation IS the domain's Operation (re-exported), so the
    // Frame::into_row_op match covers Insert/Update/Delete with no fallthrough.
    #[test]
    fn operation_variants_are_exhaustive() {
        for op in [Operation::Insert, Operation::Update, Operation::Delete] {
            let f = Frame {
                lsn: 1,
                op,
                table: "t".into(),
                pk: "p".into(),
                payload: Some(vec![0]),
                txn_id: None,
            };
            // Every variant must map to a RowOp without panic.
            let _ = f.into_row_op();
        }
    }

    // NOTE: the end-to-end client behavior (subscribe, apply, reconnect) is
    // proven in crates/nostos-client/tests/chaos_resume.rs against a real
    // in-process server + FakeReplicator — that's where zero-loss/zero-dup is
    // asserted over a genuine socket, not a unit test here.
}
