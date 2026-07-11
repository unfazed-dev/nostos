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
//! ## The write half — durable outbox (ADR-0013, D3)
//!
//! `SyncClient::write` enqueues a [`PendingWrite`] to the durable outbox and
//! returns immediately — it NEVER blocks on the network to capture user intent.
//! After each subscribe-ack, the connected loop flushes `pending()` in order:
//! each queued write goes out as a `Write` frame; the matching `WriteResult`
//! frame (correlated by `client_write_id == outbox id`) drives `mark_done` on
//! `ok:true`. On `ok:false` the write stays queued and the error surfaces via
//! the client's log channel.
//!
//! ponytail: failed writes retry forever and block the queue head; add a
//! dead-letter policy when a design partner hits a permanent rejection.
//!
//! The flush and the apply both reach the storage through the same engine
//! mutex, so they're serialized by construction (single-threaded, per the
//! [`nostos_core::Storage`] contract — the outbox and the data share one SQLite
//! connection).
//!
//! ## Auth
//!
//! The bearer token is passed via `?token=` on the WebSocket URL. Browsers
//! can't set headers on a WS handshake, so the transport accepts the token as
//! a query parameter (ADR-0010); we use the same path for consistency across
//! native + future-FFI clients.
//!
//! ## Reconnect semantics
//!
//! A WebSocket drop is expected (network blip, server restart, eviction). The
//! client treats it as "flush whatever's pending, then reconnect from the
//! durable checkpoint." Because `apply_batch` is atomic and idempotent, a
//! reconnect that re-receives the tail of an already-applied transaction is a
//! no-op — the server's dedup + the client's idempotent upsert converge.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use nostos_core::{ApplyEngine, ApplyOutcome, Frame, Outbox, PendingWrite};
use nostos_domain::Lsn;
use nostos_infra::wire::{decode_frames, ClientMessage};
use futures_util::{Sink, SinkExt, StreamExt};
use tokio::sync::{Mutex, Notify};
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
    ///
    /// This is a *session*-level backstop (it tears down and reconnects the
    /// whole WS connection) — it is NOT the mechanism that closes a buffered
    /// apply batch anymore; see `flush_quiesce` for that. A long-lived client
    /// can still set a generous value here as defense-in-depth (a periodic
    /// reconnect that re-resolves `resume_lsn` and re-flushes the outbox), but
    /// should not rely on it for per-write latency.
    pub idle_timeout: Option<Duration>,
    /// A buffered-but-unflushed apply batch (see [`ApplyEngine::has_pending`])
    /// is force-flushed if no new frame arrives within this long after the
    /// last one. This is what closes a real Postgres transaction's frames
    /// when they are the LAST activity on an otherwise-idle table — the wire
    /// carries no explicit commit marker (only `txn_id` equality/inequality
    /// across frames), so without a bounded fallback a solitary write's
    /// frames buffer forever (see the `SyncClient` module doc's receive-loop
    /// diagram and ADR-0016's addendum "client apply flush bound").
    ///
    /// Resets on every frame received while a batch is pending — a fast burst
    /// of same-txn frames never triggers this early; only a genuine gap does.
    /// `None` disables the bound entirely (only [`Self::idle_timeout`] or the
    /// next differing-`txn_id` frame would then close the batch) — not
    /// recommended for a real Postgres source.
    ///
    /// ponytail: this is a heuristic, not a protocol guarantee. A single
    /// Postgres transaction whose events are delivered with a gap wider than
    /// this window (a genuinely huge transaction stalling on a slow network)
    /// would be force-flushed mid-transaction, splitting one atomic commit
    /// across two `apply_batch` calls — no data is lost or duplicated (each
    /// half still commits durably and the checkpoint still advances
    /// correctly), but a reader could transiently observe the txn half-
    /// applied. Fixing this precisely requires the wire to carry an explicit
    /// commit boundary (a `nostos-infra` / wire-protocol change, out of this
    /// fix's scope) or increasing this window at the cost of higher latency
    /// for the common "quiet table, one write" case. Default (50ms) is chosen
    /// to be comfortably longer than a genuine same-transaction frame burst
    /// (same-process delivery over one WS connection) and short enough that a
    /// solitary write feels instant to a user.
    pub flush_quiesce: Option<Duration>,
    /// An optional safe-SQL-subset predicate (ADR-0012) the server compiles and
    /// ANDs into the session — e.g. `"priority > 5"` or
    /// `"status = open AND priority >= 3"`. The grammar is the six comparison
    /// operators + `AND`/`OR`/`NOT` + parens (see `parse_predicate_expr`); a
    /// parse failure closes the socket with an `invalid where_sql:` reason
    /// before any event flows. `None` (the default) = match-all on `table`.
    /// Server-enforced: the principal's tenant scoping always wraps this, so a
    /// `where_sql` can never widen scope past its tenant.
    pub where_sql: Option<String>,
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
            flush_quiesce: Some(DEFAULT_FLUSH_QUIESCE),
            where_sql: None,
        }
    }
}

/// Default [`SyncClientConfig::flush_quiesce`] — see that field's doc for the
/// tradeoff this window encodes.
pub const DEFAULT_FLUSH_QUIESCE: Duration = Duration::from_millis(50);

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
///
/// The storage `S` implements BOTH [`nostos_core::Storage`] (the apply/checkpoint
/// surface) AND [`nostos_core::Outbox`] (the durable write queue). For
/// `SqliteStorage` both are backed by the same SQLite file + connection, so a
/// crash can't strand the outbox without the data (ADR-0013). The flush loop
/// reaches the outbox through the same engine mutex as the apply loop — they're
/// serialized by construction.
pub struct SyncClient<S>
where
    S: nostos_core::Storage + Outbox + Send + 'static,
{
    url: String,
    config: SyncClientConfig,
    engine: Arc<Mutex<ApplyEngine<S>>>,
    /// Broadcasts one [`ApplyOutcome`] per commit (each transaction-boundary or
    /// soft-cap flush that lands in storage — see [`Self::run_once`]). Additive:
    /// nothing subscribes by default (`send` on zero receivers is a harmless
    /// no-op), so this changes no existing behavior. Exists so an in-process
    /// readback consumer (e.g. `nostos_flutter`'s `watch()`) can re-query
    /// storage and re-emit after every applied batch instead of polling.
    changes: tokio::sync::broadcast::Sender<ApplyOutcome>,
    /// Wakes the connected receive loop (see [`Self::run_once`]) when
    /// [`Self::write`] enqueues a new outbox entry, so it can send the write
    /// immediately instead of waiting for the next reconnect. `notify_one`
    /// before anyone is `.await`ing `notified()` stores a single permit that
    /// the next `notified().await` consumes right away (`tokio::sync::Notify`
    /// semantics) — a write racing ahead of `run_once`'s own startup flush is
    /// never lost, just picked up by whichever side gets there first.
    write_notify: Notify,
}

impl<S> SyncClient<S>
where
    S: nostos_core::Storage + Outbox + Send + 'static,
{
    /// Build a client targeting `url` (e.g. `ws://127.0.0.1:9999/sync`), with
    /// the given storage backend and config.
    #[must_use]
    pub fn new(url: impl Into<String>, storage: S, config: SyncClientConfig) -> Self {
        let engine = Arc::new(Mutex::new(ApplyEngine::new(storage)));
        // Capacity is a lag buffer, not a hard cap: a slow/absent subscriber
        // just misses old notifications (`RecvError::Lagged`) — the next one
        // still carries the latest checkpoint, and a readback consumer like
        // `watch()` re-queries storage rather than replaying a diff, so a
        // lagged receiver self-heals on the next tick.
        let (changes, _) = tokio::sync::broadcast::channel(64);
        Self {
            url: url.into(),
            config,
            engine,
            changes,
            write_notify: Notify::new(),
        }
    }

    /// Subscribe to per-commit apply notifications. Each applied batch (a
    /// transaction boundary or soft-cap flush — see [`ApplyEngine::feed`] and
    /// [`ApplyEngine::flush`]) broadcasts once, after it is durable. Multiple
    /// subscribers are independent (broadcast, not mpsc); a receiver created
    /// after a notification was sent simply doesn't see it — call this before
    /// [`Self::run_once`]/[`Self::run_with_reconnect`] starts if you need every
    /// tick from the beginning of the session.
    #[must_use]
    pub fn subscribe_changes(&self) -> tokio::sync::broadcast::Receiver<ApplyOutcome> {
        self.changes.subscribe()
    }

    /// Read the current durable checkpoint (delegates through the engine).
    pub async fn checkpoint(&self) -> nostos_core::Result<Lsn> {
        self.engine.lock().await.checkpoint()
    }

    /// Enqueue a local write to the durable outbox. Returns the write's
    /// monotonically increasing id (the correlation key on the wire).
    ///
    /// **Always succeeds regardless of connection state** — that's the whole
    /// point of the outbox: a user action is captured durably the instant it
    /// happens, not gated on a server round-trip. The connected flush loop (in
    /// [`Self::run_once`]) drains the queue after every subscribe-ack AND is
    /// woken to re-drain it on every subsequent call to this method (via
    /// [`Self::write_notify`]) — so a write made mid-session, not just at
    /// connect time, still reaches the wire without waiting for a reconnect.
    ///
    /// # Errors
    /// Returns [`ClientError::Storage`] only if the durable enqueue itself
    /// failed (disk full, SQLite busy) — i.e. the write did NOT land and the
    /// caller MUST surface that to the user.
    pub async fn write(&self, write: PendingWrite) -> Result<u64, ClientError> {
        let engine = Arc::clone(&self.engine);
        let id = tokio::task::spawn_blocking(move || -> nostos_core::Result<u64> {
            let mut engine = engine.blocking_lock();
            engine.storage_mut().enqueue(write)
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))??;
        debug!(write_id = id, "enqueued local write to outbox");
        // Wake a live `run_once` loop so it re-drains the outbox now, instead
        // of only at the next connect/reconnect. Harmless if nobody's running
        // yet (`Notify` stores the permit) or if the write already went out
        // via the startup flush racing ahead of us (the next drain just finds
        // nothing new to send).
        self.write_notify.notify_one();
        Ok(id)
    }

    /// Snapshot the durable outbox (oldest first). Used by the flush loop in
    /// [`Self::run_once`] to send pending writes after a subscribe-ack.
    async fn pending_writes(&self) -> Result<Vec<(u64, PendingWrite)>, ClientError> {
        let engine = Arc::clone(&self.engine);
        let pending = tokio::task::spawn_blocking(move || -> nostos_core::Result<Vec<_>> {
            // `pending` takes `&self` — borrow the storage through the engine
            // without taking the write half of the mutex exclusively for long.
            let engine = engine.blocking_lock();
            engine.storage().pending()
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))??;
        Ok(pending)
    }

    /// Run a read-only closure against the backing storage, off the async
    /// runtime (`spawn_blocking`, matching every other storage access on this
    /// type — SQLite I/O may block). Generic over the return value so a caller
    /// can reach a backend-specific accessor that isn't part of the
    /// [`nostos_core::Storage`] trait — e.g. `SqliteStorage::rows_for` — without
    /// this crate hardcoding that backend into `SyncClient`'s public API.
    ///
    /// This is the seam an in-process readback consumer (e.g. `nostos_flutter`'s
    /// `watch()`) uses to re-query the current row set after a
    /// [`Self::subscribe_changes`] notification, instead of maintaining a
    /// second, parallel view of the data.
    ///
    /// # Errors
    /// Returns [`ClientError::Join`] only if the blocking task panics.
    pub async fn with_storage<F, R>(&self, f: F) -> Result<R, ClientError>
    where
        F: FnOnce(&S) -> R + Send + 'static,
        R: Send + 'static,
    {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || {
            let engine = engine.blocking_lock();
            f(engine.storage())
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))
    }

    /// Mark an outbox write done (the server ack'd it with `WriteResult{ok:true}`).
    async fn mark_write_done(&self, id: u64) -> Result<(), ClientError> {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || -> nostos_core::Result<()> {
            let mut engine = engine.blocking_lock();
            engine.storage_mut().mark_done(id)
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))??;
        Ok(())
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

    /// Send every outbox write not yet sent over THIS connection. Called once
    /// right after subscribe (the startup backlog flush) and again whenever
    /// [`Self::write_notify`] fires (a write enqueued mid-session — see
    /// [`Self::run_once`]'s receive loop).
    ///
    /// `sent_this_conn` tracks ids already sent over this WS connection so a
    /// write in flight isn't resent every time the notifier fires while its
    /// `WriteResult` ack hasn't landed yet. A fresh connection (reconnect)
    /// gets a fresh, empty set and legitimately resends anything still
    /// outstanding — `mark_done`/the server's upsert are idempotent, so a
    /// genuine resend after a real drop is still safe.
    async fn flush_outbox<W>(
        &self,
        write: &mut W,
        sent_this_conn: &mut HashSet<u64>,
    ) -> Result<(), ClientError>
    where
        W: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        // The flush and the apply share the engine mutex, so they're
        // serialized by construction (single-threaded per the Storage
        // contract). `mark_done` happens later, in the receive loop, when
        // each `WriteResult{ok:true}` lands.
        let pending = self.pending_writes().await?;
        let fresh: Vec<(u64, PendingWrite)> = pending
            .into_iter()
            .filter(|(id, _)| sent_this_conn.insert(*id))
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        debug!(n = fresh.len(), "flushing pending outbox writes");
        for (id, pw) in &fresh {
            // The outbox id IS the wire correlation key — a string on the
            // wire (ClientMessage::Write::client_write_id is a String), so
            // render the u64 once here. The server echoes it verbatim.
            let wire = ClientMessage::Write {
                table: pw.table.clone(),
                op: pw.op.as_wire_str().to_string(),
                pk: pw.pk.clone(),
                payload: pw
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
                client_write_id: id.to_string(),
            };
            let json = serde_json::to_string(&wire).expect("write serializes");
            write
                .send(Message::Text(json))
                .await
                .map_err(|e| ClientError::Send(e.to_string()))?;
        }
        Ok(())
    }

    /// Send an `Ack` for a landed commit and broadcast it on [`Self::changes`].
    /// Shared by the frame-triggered commit path and the quiesce-triggered
    /// force-flush path in [`Self::run_once`] — both are "a batch just became
    /// durable" and get identical treatment.
    async fn ack_and_notify<W>(
        &self,
        write: &mut W,
        outcome: ApplyOutcome,
    ) -> Result<(), ClientError>
    where
        W: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let ack = ClientMessage::Ack {
            lsn: outcome.checkpoint.raw(),
        };
        let ack_json = serde_json::to_string(&ack).expect("ack serializes");
        write
            .send(Message::Text(ack_json))
            .await
            .map_err(|e| ClientError::Send(e.to_string()))?;
        // Best-effort: no receivers is not an error (see `changes` doc).
        let _ = self.changes.send(outcome);
        Ok(())
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
            where_sql: self.config.where_sql.clone(),
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

        // ---- Flush the durable outbox (ADR-0013, D3): the startup backlog ----
        // Everything already queued before this connection came up (offline
        // writes, or ones made while a prior connection was mid-handshake)
        // goes out fire-and-forget here; the matching `WriteResult` acks are
        // handled in the receive loop below (correlated by
        // `client_write_id == outbox id`). Writes made AFTER this point, for
        // the life of this connection, are handled by the `write_notify`
        // branch in the loop, not here.
        let mut sent_this_conn: HashSet<u64> = HashSet::new();
        self.flush_outbox(&mut write, &mut sent_this_conn).await?;

        // ---- Receive → apply → ack loop ----
        // Three independent triggers race each loop iteration:
        //   1. the next WS message (idle_timeout-bounded if configured — a
        //      long gap with NOTHING pending means "caught up", so break and
        //      return the session, the "sync then disconnect" shape);
        //   2. the flush-quiesce timer, armed only while a batch is buffered
        //      (`ApplyEngine::has_pending`) — closes a real Postgres
        //      transaction's frames when they're the last activity on an
        //      otherwise-idle table (see `SyncClientConfig::flush_quiesce`);
        //   3. `write_notify` — a write enqueued mid-session, resent now
        //      instead of waiting for a reconnect (see `Self::write`).
        let quiesce = self.config.flush_quiesce;
        let mut last_frame_at = tokio::time::Instant::now();
        loop {
            let has_pending = self.engine.lock().await.has_pending();
            let quiesce_deadline = quiesce.filter(|_| has_pending).map(|q| last_frame_at + q);

            tokio::select! {
                // ---- Branch 1: the next WS message ----
                recv = async {
                    if let Some(idle) = self.config.idle_timeout {
                        tokio::time::timeout(idle, read.next()).await
                    } else {
                        Ok(read.next().await)
                    }
                } => {
                    let Ok(next) = recv else {
                        debug!("idle timeout reached; treating stream as caught up");
                        break;
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

                    // D3: a `write_result` frame is its own wire shape (a single JSON
                    // object tagged `"type":"write_result"`, never batched with
                    // replication events). It won't decode as a `WireFrame` (it carries
                    // no lsn/op/table), so intercept it BEFORE `decode_frames` and drive
                    // the outbox ack. Anything else falls through to the replication
                    // path below.
                    if let Some(result) = decode_write_result(&bytes) {
                        // Correlate by client_write_id == outbox id.
                        let id: u64 = if let Ok(id) = result.client_write_id.parse() {
                            id
                        } else {
                            warn!(
                                client_write_id = %result.client_write_id,
                                "write_result with non-numeric client_write_id; ignoring"
                            );
                            continue;
                        };
                        if result.ok {
                            // Ack'd: remove from the durable outbox. Idempotent — a
                            // redelivery after a partial flush removes nothing (already
                            // gone), not an error.
                            if let Err(e) = self.mark_write_done(id).await {
                                warn!(write_id = id, error = %e, "mark_done failed; write stays queued");
                            } else {
                                debug!(write_id = id, "write ack'd — removed from outbox");
                            }
                        } else {
                            // ok:false — the write is NOT removed. It stays at the queue
                            // head and is retried on the next flush. The error surfaces
                            // here so an operator sees it; the user-facing surface is a
                            // Phase-2 concern.
                            //
                            // ponytail: failed writes retry forever and block the queue
                            // head; add a dead-letter policy when a design partner hits
                            // a permanent rejection.
                            warn!(
                                write_id = id,
                                error = result.error.as_deref().unwrap_or("(no detail)"),
                                "write rejected by server; stays queued, will retry"
                            );
                        }
                        continue;
                    }

                    // C3 batched-writes: one WS message may carry a JSON array of
                    // frames (server coalesces under backlog) OR a legacy single object.
                    // `decode_frames` handles both; iterate every frame inside it.
                    for frame in decode_frames(&bytes) {
                        frames_received += 1;
                        last_frame_at = tokio::time::Instant::now();

                        // Hex-decode the payload once, at the boundary (the wire
                        // carries hex; downstream everything is raw bytes).
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
                        let outcome = tokio::task::spawn_blocking(
                            move || -> nostos_core::Result<Option<ApplyOutcome>> {
                                let mut engine = engine.blocking_lock();
                                engine.feed(core_frame)
                            },
                        )
                        .await
                        .map_err(|e| ClientError::Join(e.to_string()))??;

                        if let Some(outcome) = outcome {
                            commits += 1;
                            self.ack_and_notify(&mut write, outcome).await?;
                        }
                    }
                }

                // ---- Branch 2: flush-quiesce fired — no follow-up frame arrived
                //      within the window, so close the buffered batch now. ----
                () = tokio::time::sleep_until(quiesce_deadline.unwrap_or_else(tokio::time::Instant::now)), if quiesce_deadline.is_some() => {
                    let engine = Arc::clone(&self.engine);
                    let flushed = tokio::task::spawn_blocking(
                        move || -> nostos_core::Result<Option<ApplyOutcome>> {
                            let mut engine = engine.blocking_lock();
                            engine.flush()
                        },
                    )
                    .await
                    .map_err(|e| ClientError::Join(e.to_string()))??;
                    if let Some(outcome) = flushed {
                        commits += 1;
                        debug!(
                            rows = outcome.rows_applied,
                            checkpoint = outcome.checkpoint.raw(),
                            "flush-quiesce: no follow-up frame within the window; closed the buffered batch"
                        );
                        self.ack_and_notify(&mut write, outcome).await?;
                    }
                }

                // ---- Branch 3: a write() enqueued mid-session ----
                () = self.write_notify.notified() => {
                    self.flush_outbox(&mut write, &mut sent_this_conn).await?;
                }
            }
        }

        // ---- Final flush of any pending batch before returning ----
        let engine = Arc::clone(&self.engine);
        let final_flush =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<Option<ApplyOutcome>> {
                let mut engine = engine.blocking_lock();
                engine.flush()
            })
            .await
            .map_err(|e| ClientError::Join(e.to_string()))??;
        if let Some(outcome) = final_flush {
            commits += 1;
            let _ = self.changes.send(outcome);
        }

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

/// A decoded `WriteResult` frame (server → client, the ack for a `Write`).
///
/// This is the client-side decode of the shape `nostos_infra::wire::encode_write_result`
/// produces (D2). It lives here rather than in the wire module because D3 is the
/// first consumer of the DECODED form — the wire module only encodes it (the
/// server side). When a second consumer appears, promote this to `nostos_infra::wire`.
///
/// `client_write_id` echoes the request's correlation id (the outbox row id,
/// rendered as a string on the wire). `error` is `Some` iff `ok` is `false`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteResult {
    client_write_id: String,
    ok: bool,
    error: Option<String>,
}

/// Decode a `WriteResult` frame from a WS message's bytes. Returns `None` for
/// anything that isn't a `write_result` frame (so the caller can fall through to
/// the replication-frame path). A malformed `write_result` (missing fields, bad
/// JSON) also returns `None` — it's logged + dropped, matching the
/// replication path's "drop malformed" behavior.
fn decode_write_result(bytes: &[u8]) -> Option<WriteResult> {
    // Cheap reject: only attempt the parse if the message looks like a
    // write_result frame. The tag is always present (`encode_write_result`
    // emits it), so a substring check avoids a full serde parse on every
    // replication frame. The replication path is the hot one; this keeps it
    // untouched.
    if !memchr_looks_like_write_result(bytes) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("write_result") {
        return None;
    }
    Some(WriteResult {
        client_write_id: v.get("client_write_id")?.as_str()?.to_string(),
        ok: v
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        error: v
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Cheap pre-filter for [`decode_write_result`]: does this byte slice look like
/// a `write_result` frame? A direct byte scan for the tag substring, avoiding a
/// full JSON parse on the hot replication path. Whitespace-tolerant enough for
/// the wire (the server emits compact JSON with no leading whitespace).
fn memchr_looks_like_write_result(bytes: &[u8]) -> bool {
    // The encoder emits `"type":"write_result"` (compact). Allow optional
    // whitespace around the colon for robustness.
    const NEEDLE: &[u8] = b"\"type\"";
    if !contains(bytes, NEEDLE) {
        return false;
    }
    contains(bytes, b"write_result")
}

/// Boyer-Moore-less substring search — fine for a tiny needle on a small frame.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
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

    #[tokio::test]
    async fn with_storage_runs_closure_against_backing_storage() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig::default(),
        );
        let row_count = c.with_storage(nostos_core::InMemoryStorage::row_count).await;
        assert_eq!(row_count.unwrap(), 0);
    }

    #[test]
    fn subscribe_changes_receiver_starts_with_no_pending_notifications() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig::default(),
        );
        let mut rx = c.subscribe_changes();
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    // NOTE: the end-to-end client behavior (subscribe, apply, reconnect) is
    // proven in crates/nostos-client/tests/chaos_resume.rs against a real
    // in-process server + FakeReplicator — that's where zero-loss/zero-dup is
    // asserted over a genuine socket, not a unit test here.
}
