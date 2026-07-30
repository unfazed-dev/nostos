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
//! `ok:true`. On `ok:false` the write's retry counter is bumped; once it
//! reaches `dead_letter_max_attempts` (ADR-0013 v2) the write is quarantined
//! (removed from the pending queue but NOT deleted) so the queue head advances
//! past a permanently-failing write. The error surfaces via the client's log
//! channel on every rejection. The user-facing surface is
//! [`WriteQueueStatus`] (ADR-0027): pending/dead-lettered counts and the
//! server's message, published on a `watch` channel — dead-letters only, since
//! a plain rejection is usually transient and retries.
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
use nostos_infra::wire::{decode_control_frame, decode_frames, decode_resume_info, ClientMessage};
use futures_util::{Sink, SinkExt, StreamExt};
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// One additional table subscription for a [`SyncClientConfig`]: a table name
/// plus an optional safe-SQL `where_sql` predicate (ADR-0012). A connection
/// subscribes to the primary `table` PLUS every entry in `extra_tables` over
/// one `/sync` socket — multi-table-per-handle (D1/ADR-0022). All tables share
/// one resume LSN, one checkpoint, and one ack stream (ADR-0009's single global
/// checkpoint).
#[derive(Debug, Clone)]
pub struct TableSub {
    /// Table name to subscribe to.
    pub name: String,
    /// Optional safe-SQL predicate scoped to THIS table (ADR-0012).
    pub where_sql: Option<String>,
}

impl TableSub {
    /// A match-all subscription to `name` (no `where_sql`).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            where_sql: None,
        }
    }
}

/// Configuration for a [`SyncClient`].
#[derive(Debug, Clone)]
pub struct SyncClientConfig {
    /// The PRIMARY table to subscribe to (the N=1 default). For multi-table,
    /// add more via [`Self::extra_tables`].
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
    /// Additional tables to subscribe to alongside the primary `table`, each
    /// with its own optional `where_sql` (D1/ADR-0022 multi-table-per-handle).
    /// All subscriptions share one resume LSN, one checkpoint, and one ack
    /// stream over the single `/sync` socket. Empty (the default) = single-
    /// table, the historical behavior. The server caps a socket at 32 tables.
    pub extra_tables: Vec<TableSub>,
    /// Maximum number of `WriteResult{ok:false}` rejections before a write is
    /// dead-lettered (quarantined). Once a write's attempt count (bumped on
    /// every rejection) reaches this threshold, the flush loop calls
    /// `Outbox::mark_dead_letter` to remove it from the pending queue so the
    /// head advances past a permanently-failing write; the row stays in the
    /// backing store for inspection (e.g. `SqliteStorage::dead_letter_entries`)
    /// — it is NOT silently deleted. ADR-0013 v2 dead-letter policy.
    ///
    /// The default (50) is a deliberately generous ceiling: a transient
    /// rejection (a constraint violation racing with a concurrent write, a
    /// momentarily-unwritable table) should resolve within a handful of
    /// retries, and only a genuinely permanent failure (server bug, schema
    /// drift, an unauthorized row) hits the cap. Set to a small value for
    /// faster failover in test environments; set to `u32::MAX` to effectively
    /// disable dead-lettering (the pre-v2 retry-forever behavior).
    pub dead_letter_max_attempts: u32,
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
            extra_tables: Vec::new(),
            dead_letter_max_attempts: DEFAULT_DEAD_LETTER_MAX_ATTEMPTS,
        }
    }
}

/// Default [`SyncClientConfig::flush_quiesce`] — see that field's doc for the
/// tradeoff this window encodes.
pub const DEFAULT_FLUSH_QUIESCE: Duration = Duration::from_millis(50);

/// Default [`SyncClientConfig::dead_letter_max_attempts`] — see that field's
/// doc for the rationale (generous ceiling; only permanent failures hit it).
pub const DEFAULT_DEAD_LETTER_MAX_ATTEMPTS: u32 = 50;

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
/// A snapshot of the durable outbox, for UI binding (ADR-0027).
///
/// The engine already knew all of this — it just had nowhere to say it. Before
/// this existed, a rejected write was a `warn!` in the logs and nothing else,
/// so an app literally could not tell its user that a write was lost (the Dart
/// `write()` returns an outbox id, not a server ack).
///
/// The distinction that matters is transient-vs-permanent. A `WriteResult{ok:
/// false}` is often legitimate and self-healing — see
/// [`SyncClientConfig::dead_letter_max_attempts`] — so surfacing every
/// rejection would make apps show scary errors for writes that are about to
/// succeed on retry. [`Self::last_error`] is therefore set ONLY when a write is
/// dead-lettered, i.e. when it has permanently failed and left the queue. That
/// is the point at which a human has to be told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteQueueStatus {
    /// Writes durably queued but not yet ack'd by the server. `> 0` while
    /// offline is normal and expected — that IS the offline-first promise.
    pub pending: u64,
    /// Writes permanently failed this session (quarantined, NOT deleted —
    /// still inspectable via e.g. `SqliteStorage::dead_letter_entries`).
    pub dead_lettered: u64,
    /// The server's error text from the most recent dead-letter, verbatim
    /// (e.g. the `NOSTOS_WRITE_TABLES` allowlist rejection, which names the
    /// exact env var to set). `None` until one happens.
    pub last_error: Option<String>,
}

pub struct SyncClient<S>
where
    S: nostos_core::Storage + Outbox + Send + 'static,
{
    url: String,
    config: SyncClientConfig,
    /// The live bearer token, seeded from `config.token` and replaceable via
    /// [`SyncClient::set_token`]. Separate from `config` because `config` is
    /// immutable after construction and a token is the one field that must
    /// outlive its initial value — see `set_token` for why.
    token: std::sync::RwLock<Option<String>>,
    engine: Arc<Mutex<ApplyEngine<S>>>,
    /// Hot outbox status for UI binding — `watch`, not `broadcast`, because a
    /// late subscriber must see the CURRENT value immediately (a status widget
    /// built after connect still needs to render "3 pending"), and because
    /// coalescing intermediate values is correct for a status readout.
    ///
    /// `pending` is seeded from storage at construction, incremented on enqueue
    /// (always a new row, so +1 is exact), and **re-counted from storage** on
    /// ack and on dead-letter. The removals re-count rather than decrement
    /// because both are idempotent — a redelivered ack for an already-removed
    /// write would otherwise decrement twice and report "0 unsynced" while
    /// writes are still queued. The scan runs inside the existing blocking
    /// section, so it costs one query per server response, never per keystroke.
    write_status: tokio::sync::watch::Sender<WriteQueueStatus>,
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
        // Seed `pending` from the durable outbox BEFORE the storage moves into
        // the engine: writes made in a previous session survive a restart, so
        // an app that reopens with 3 unsent writes must render "3 pending", not
        // "0". A read failure here is not worth refusing to construct a client
        // over — fall back to 0 and let the next mutation correct it.
        let pending = storage.pending().map_or(0, |p| p.len() as u64);
        let engine = Arc::new(Mutex::new(ApplyEngine::new(storage)));
        let (write_status, _) = tokio::sync::watch::channel(WriteQueueStatus {
            pending,
            ..WriteQueueStatus::default()
        });
        // Capacity is a lag buffer, not a hard cap: a slow/absent subscriber
        // just misses old notifications (`RecvError::Lagged`) — the next one
        // still carries the latest checkpoint, and a readback consumer like
        // `watch()` re-queries storage rather than replaying a diff, so a
        // lagged receiver self-heals on the next tick.
        let (changes, _) = tokio::sync::broadcast::channel(64);
        let token = std::sync::RwLock::new(config.token.clone());
        Self {
            url: url.into(),
            config,
            token,
            engine,
            changes,
            write_notify: Notify::new(),
            write_status,
        }
    }

    /// Replace the bearer token used by **subsequent** connections.
    ///
    /// This exists because an access token outlives nothing gracefully: a
    /// Supabase JWT expires in about an hour, the server enforces `exp`, and
    /// [`Self::run_with_reconnect`] re-sends whatever token it was built with on
    /// every attempt. Without this, a long-lived client retries a dead token
    /// forever and the app silently stops syncing while still rendering stale
    /// rows.
    ///
    /// Deliberately does **not** force a reconnect. If the socket is live the
    /// new token is simply picked up next time one is opened; if the client is
    /// already in the reconnect loop, the next attempt uses it, so a refresh
    /// self-heals within one backoff window. Nothing else is torn down — the
    /// storage, the outbox, and every `changes` subscriber survive, which is the
    /// whole point of doing this here instead of rebuilding the client.
    pub fn set_token(&self, token: Option<String>) {
        *self.token.write().expect("set_token: token lock poisoned") = token;
    }

    /// Watch the durable outbox: pending count, dead-letter count, and the last
    /// permanent failure's server error. See [`WriteQueueStatus`] for why only
    /// dead-letters set the error.
    ///
    /// Unlike [`Self::subscribe_changes`], a receiver created at any time sees
    /// the current value immediately — no need to subscribe before `run_once`.
    #[must_use]
    pub fn subscribe_write_status(&self) -> tokio::sync::watch::Receiver<WriteQueueStatus> {
        self.write_status.subscribe()
    }

    /// Current outbox status without holding a subscription.
    #[must_use]
    pub fn write_status(&self) -> WriteQueueStatus {
        self.write_status.borrow().clone()
    }

    /// Apply `f` to the live status and publish the result. `send_modify`
    /// notifies watchers unconditionally, which is what we want: a re-emitted
    /// identical value is harmless to a UI, whereas a dropped transition is not.
    fn update_write_status(&self, f: impl FnOnce(&mut WriteQueueStatus)) {
        self.write_status.send_modify(f);
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

    /// Read the durable last-seen server slot epoch (ADR-0025 reconnect-resume
    /// gate). 0 on a fresh DB → the Subscribe sends `epoch: None` → the server
    /// treats it as a mismatch (full snapshot). Delegates through the engine.
    pub async fn epoch(&self) -> nostos_core::Result<u64> {
        self.engine.lock().await.epoch()
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
        let (id, local_tick) =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<(u64, Option<ApplyOutcome>)> {
                let mut engine = engine.blocking_lock();
                // Enqueue FIRST (durable) — WS2 slice-2 instant-local write. If the
                // local apply below fails (or we crash between), the write is still
                // queued and will appear on the server's echo. No data loss either way.
                let id = engine.storage_mut().enqueue(write.clone())?;
                // Optimistic local apply: render the row in the data store NOW so
                // the UI is instant (offline-first), WITHOUT advancing the
                // replication checkpoint (the row isn't server-confirmed yet). The
                // server's echo later UPSERTs the authoritative image (reconcile,
                // last-writer-wins). Best-effort: a failure here only delays
                // visibility to echo-time — the write is already durable in the outbox.
                let applied = match engine.storage_mut().apply_local(&write) {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(write_id = id, error = %e, "instant-local apply failed; write still queued");
                        false
                    }
                };
                // On success, build a checkpoint-preserving change tick. The
                // watch pumps re-snapshot on EVERY `changes` broadcast (no
                // checkpoint dedup), so broadcasting here makes the optimistic
                // row visible offline. We send on `changes` directly — never
                // `ack_and_notify` — and read the checkpoint as-is, so neither
                // the durable checkpoint nor the ack state moves for a row that
                // isn't server-confirmed yet (the server's echo reconciles it).
                let local_tick = if applied {
                    engine
                        .checkpoint()
                        .ok()
                        .map(|checkpoint| ApplyOutcome { checkpoint, rows_applied: 1 })
                } else {
                    None
                };
                Ok((id, local_tick))
            })
            .await
            .map_err(|e| ClientError::Join(e.to_string()))??;
        debug!(write_id = id, "enqueued local write to outbox");
        // The write is durable but unsent — it counts as pending until the
        // server acks it (or it dead-letters). This is what lets an app render
        // "2 unsynced changes" while offline.
        self.update_write_status(|s| s.pending += 1);
        // Broadcast the local tick so live watch pumps re-query NOW and render
        // the optimistic row (offline-first). Best-effort: no receivers is fine.
        if let Some(tick) = local_tick {
            // Best-effort: `send` on a broadcast with no live receivers is a
            // no-op, not an error — fine (no pump attached yet).
            let _ = self.changes.send(tick);
        }
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
        // Re-count rather than decrement: `mark_done` is idempotent, and the
        // caller documents redelivery-after-a-partial-flush as a real path — a
        // second ack for an already-removed write would decrement twice and
        // undercount, showing "0 unsynced" while writes are still queued. The
        // count happens inside the SAME blocking section under the SAME lock,
        // so it costs one scan per server ack, not per keystroke.
        let remaining = tokio::task::spawn_blocking(move || -> nostos_core::Result<u64> {
            let mut engine = engine.blocking_lock();
            engine.storage_mut().mark_done(id)?;
            Ok(engine.storage().pending()?.len() as u64)
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))??;
        self.update_write_status(|s| s.pending = remaining);
        Ok(())
    }

    /// Bump the retry counter for a rejected write (`WriteResult{ok:false}`)
    /// and, if the count has reached the configured `dead_letter_max_attempts`,
    /// quarantine it via `Outbox::mark_dead_letter`. Returns `(attempts, dld)`
    /// where `attempts` is the post-bump count and `dld` is true iff the write
    /// was just dead-lettered. ADR-0013 v2 dead-letter policy.
    ///
    /// Both `Outbox` calls are `&self` (the `SqliteStorage` backend uses
    /// `Mutex<Connection>` for interior mutability), so they share one engine
    /// lock acquisition inside a single `spawn_blocking` task — the bump and
    /// the quarantine land atomically from the flush loop's perspective (no
    /// intermediate flush can observe a "bumped but not yet dead-lettered"
    /// row, which would otherwise double-count on a fast retry).
    ///
    /// A backend whose `Outbox` impl uses the trait's default no-op methods
    /// (e.g. `InMemoryStorage`) gets `bump_attempts → 0` back, so
    /// `0 >= max` is false for any positive max — the write is never
    /// dead-lettered, matching the pre-v2 retry-forever behavior for test
    /// doubles that don't model DLQ state.
    async fn bump_and_maybe_dead_letter(
        &self,
        id: u64,
        server_error: Option<&str>,
    ) -> Result<(u32, bool), ClientError> {
        let max = self.config.dead_letter_max_attempts;
        let engine = Arc::clone(&self.engine);
        let (attempts, dld, remaining) =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<(u32, bool, u64)> {
                let engine = engine.blocking_lock();
                let count = engine.storage().bump_attempts(id)?;
                if count >= max {
                    engine.storage().mark_dead_letter(id)?;
                    // Same reasoning as mark_write_done: re-count under the
                    // lock so a repeated dead-letter can't double-decrement.
                    let remaining = engine.storage().pending()?.len() as u64;
                    Ok((count, true, remaining))
                } else {
                    Ok((count, false, 0))
                }
            })
            .await
            .map_err(|e| ClientError::Join(e.to_string()))??;
        // ONLY the dead-letter transition is user-visible. A plain rejection is
        // routinely transient (a constraint race with a concurrent write) and
        // will retry on its own — reporting it would train users to ignore the
        // error, which is worse than not showing it. Once the write is
        // quarantined it has permanently failed and left the queue, and that IS
        // worth interrupting someone over.
        if dld {
            self.update_write_status(|s| {
                s.pending = remaining;
                s.dead_lettered += 1;
                s.last_error = Some(
                    server_error
                        .unwrap_or("write permanently rejected by server (no detail)")
                        .to_owned(),
                );
            });
        }
        Ok((attempts, dld))
    }

    /// The WS URL to connect to, with `?token=` appended if a token is set.
    ///
    /// Reads the live token (see [`Self::set_token`]), NOT `config.token` — the
    /// config value is only the seed. Reading the config here would silently
    /// undo every refresh.
    fn connect_url(&self) -> String {
        let token = self
            .token
            .read()
            .expect("connect_url: token lock poisoned")
            .clone();
        match &token {
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

        // ---- Subscribe with the durable resume_lsn + epoch ----
        let resume_lsn = self.checkpoint().await?;
        // ADR-0025 F2: send the last-seen server slot epoch so the reconnect-
        // resume gate can choose op-log replay (epoch matches) over a full
        // snapshot (mismatch). 0 on a fresh DB → None → server treats as
        // mismatch (correct for a first-ever connect).
        let client_epoch = self.epoch().await?;
        let subscribe = ClientMessage::Subscribe {
            table: self.config.table.clone(),
            filters: vec![],
            where_sql: self.config.where_sql.clone(),
            resume_lsn: (resume_lsn > Lsn::ZERO).then_some(resume_lsn.raw()),
            epoch: (client_epoch > 0).then_some(client_epoch),
        };
        let sub_json = serde_json::to_string(&subscribe).expect("subscribe serializes");
        write
            .send(Message::Text(sub_json))
            .await
            .map_err(|e| ClientError::Send(e.to_string()))?;
        // Additional tables (D1/ADR-0022): each gets its own Subscribe frame on
        // the SAME socket, sharing this connection's resume_lsn (one global
        // checkpoint, one ack stream — ADR-0009). The server registers each as
        // a separate session against the shared sink.
        for sub in &self.config.extra_tables {
            let subscribe = ClientMessage::Subscribe {
                table: sub.name.clone(),
                filters: vec![],
                where_sql: sub.where_sql.clone(),
                resume_lsn: (resume_lsn > Lsn::ZERO).then_some(resume_lsn.raw()),
                epoch: (client_epoch > 0).then_some(client_epoch),
            };
            let sub_json = serde_json::to_string(&subscribe).expect("subscribe serializes");
            write
                .send(Message::Text(sub_json))
                .await
                .map_err(|e| ClientError::Send(e.to_string()))?;
        }
        debug!(
            tables = 1 + self.config.extra_tables.len(),
            resume_lsn = resume_lsn.raw(),
            "subscribed"
        );

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
                            // ok:false — the write is NOT removed. Bump its retry
                            // counter; once the count reaches `dead_letter_max_attempts`,
                            // quarantine it via `mark_dead_letter` so the queue head can
                            // advance past a permanently-failing write (ADR-0013 v2).
                            // The write is NOT deleted — it stays in the backing store
                            // for inspection (e.g. `SqliteStorage::dead_letter_entries`)
                            // and is excluded from subsequent `pending()` calls. The
                            // user-facing surface for a dead-letter is a Phase-2 concern.
                            match self
                                .bump_and_maybe_dead_letter(id, result.error.as_deref())
                                .await
                            {
                                Ok((attempts, true)) => warn!(
                                    write_id = id,
                                    attempts,
                                    max = self.config.dead_letter_max_attempts,
                                    server_error = result.error.as_deref().unwrap_or("(no detail)"),
                                    "write dead-lettered after {attempts} rejections; \
                                     removed from pending queue (inspectable, NOT deleted)"
                                ),
                                Ok((attempts, false)) => warn!(
                                    write_id = id,
                                    attempts,
                                    max = self.config.dead_letter_max_attempts,
                                    server_error = result.error.as_deref().unwrap_or("(no detail)"),
                                    "write rejected by server; stays queued, will retry"
                                ),
                                Err(e) => warn!(
                                    write_id = id,
                                    error = %e,
                                    "failed to bump/dead-letter the rejected write; \
                                     stays queued (head not advanced this cycle)"
                                ),
                            }
                        }
                        continue;
                    }

                    // ADR-0025 F2: `resume_info` advertises the server's current
                    // slot epoch. Persist it so the NEXT reconnect's Subscribe
                    // carries the epoch this session was gated against (the
                    // resume gate compares client vs server epoch — a match ⇒
                    // op-log replay, a mismatch ⇒ full snapshot). Intercepted
                    // before the row path; never batched with events.
                    if let Some(epoch) = decode_resume_info(&bytes) {
                        let engine = Arc::clone(&self.engine);
                        // Non-fatal: a persist failure just means the next
                        // reconnect falls back to snapshot (epoch unknown) — it
                        // must NOT kill this session's data delivery.
                        match tokio::task::spawn_blocking(move || {
                            engine.blocking_lock().save_epoch(epoch)
                        })
                        .await
                        .map_err(|e| ClientError::Join(e.to_string()))
                        {
                            Ok(Ok(())) => {
                                debug!(server_epoch = epoch, "resume_info received — epoch persisted");
                            }
                            Ok(Err(e)) => warn!(
                                server_epoch = epoch,
                                error = %e,
                                "save_epoch failed; next reconnect falls back to snapshot"
                            ),
                            Err(e) => warn!(
                                server_epoch = epoch,
                                error = %e,
                                "save_epoch task join failed"
                            ),
                        }
                        last_frame_at = tokio::time::Instant::now();
                        continue;
                    }

                    // Snapshot-reconcile boundary (ADR-0014 offline-delete fix):
                    // a `{"type":"snapshot_begin"|"snapshot_end","table":"<t>"}`
                    // control frame is its own wire shape — it does NOT decode
                    // as a `WireFrame` (no lsn/op/pk), so intercept it BEFORE
                    // `decode_frames` and drive the engine's orphan-reap. The
                    // boundary is a single atomic op (no row applies happen
                    // between begin/end on this pump), and `snapshot_end` reaps
                    // any local PKs the snapshot did NOT re-confirm — those are
                    // rows hard-deleted server-side while the client was offline.
                    if let Some((table, begin)) = decode_control_frame(&bytes) {
                        let engine = Arc::clone(&self.engine);
                        // Clone for the 'static spawn_blocking closure; the
                        // original `table` stays alive for the debug! log below.
                        let table_for_engine = table.clone();
                        tokio::task::spawn_blocking(
                            move || -> nostos_core::Result<()> {
                                let mut engine = engine.blocking_lock();
                                // ADR-0025 hole #1: exempt the outbox's
                                // pending-local pks so the snapshot-reconcile
                                // never reaps the user's own unacked writes.
                                let exempt = engine
                                    .storage()
                                    .pending_pks_for_table(&table_for_engine)?;
                                engine.snapshot_boundary(&table_for_engine, begin, &exempt)
                            },
                        )
                        .await
                        .map_err(|e| ClientError::Join(e.to_string()))??;
                        debug!(table = %table, begin, "snapshot boundary applied");
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

    /// The load-bearing distinction in `WriteQueueStatus`: a transient
    /// rejection must stay invisible, a dead-letter must surface.
    ///
    /// If this inverts, apps either cry wolf on writes that are about to
    /// succeed on retry (last_error set too early) or silently lose a write
    /// (never set) — the exact defect this type was added to fix. Asserting
    /// only the final state would pass even if every rejection set the error,
    /// so the intermediate `is_none()` check is the point of the test.
    #[tokio::test]
    async fn only_dead_letter_surfaces_a_write_error() {
        // SqliteStorage, not InMemoryStorage: `Outbox::bump_attempts` has a
        // default impl returning Ok(0), which InMemoryStorage doesn't override,
        // so the dead-letter branch (`count >= max`) is unreachable there and
        // this test would pass vacuously.
        let client = SyncClient::new(
            "ws://localhost:9999/sync",
            crate::SqliteStorage::open_in_memory().expect("open"),
            SyncClientConfig {
                dead_letter_max_attempts: 3,
                ..SyncClientConfig::default()
            },
        );

        let id = client
            .write(nostos_core::PendingWrite {
                table: "tasks".into(),
                op: nostos_core::WriteOp::Upsert,
                pk: "t1".into(),
                payload_json: Some(r#"{"id":"t1"}"#.into()),
            })
            .await
            .expect("enqueue");
        assert_eq!(client.write_status().pending, 1, "queued write is pending");

        // Rejections 1 and 2 are under the threshold: the write stays queued
        // and will retry, so nothing is shown to the user.
        for attempt in 1..=2 {
            let (_, dead_lettered) = client
                .bump_and_maybe_dead_letter(id, Some("transient conflict"))
                .await
                .expect("bump");
            assert!(!dead_lettered, "attempt {attempt} must not dead-letter");
            let s = client.write_status();
            assert!(
                s.last_error.is_none(),
                "attempt {attempt}: a retryable rejection must stay silent, got {:?}",
                s.last_error
            );
            assert_eq!(s.pending, 1, "attempt {attempt}: still queued");
            assert_eq!(s.dead_lettered, 0);
        }

        // Attempt 3 hits dead_letter_max_attempts: permanently failed, out of
        // the queue, and now the app can tell its user.
        let (attempts, dead_lettered) = client
            .bump_and_maybe_dead_letter(id, Some("table not writable: 'tasks'"))
            .await
            .expect("bump");
        assert_eq!(attempts, 3);
        assert!(dead_lettered, "3rd rejection must dead-letter");

        let s = client.write_status();
        assert_eq!(
            s.last_error.as_deref(),
            Some("table not writable: 'tasks'"),
            "the server's actionable message must reach the app verbatim"
        );
        assert_eq!(s.dead_lettered, 1);
        assert_eq!(s.pending, 0, "dead-lettered write left the pending queue");
    }

    /// A redelivered ack must not double-decrement `pending`.
    ///
    /// `mark_done` is idempotent and the flush loop documents redelivery after
    /// a partial flush as a real path, so a naive `pending -= 1` undercounts —
    /// the app would show "all synced" with writes still queued, which is worse
    /// than showing nothing.
    #[tokio::test]
    async fn duplicate_ack_does_not_undercount_pending() {
        let client = SyncClient::new(
            "ws://localhost:9999/sync",
            crate::SqliteStorage::open_in_memory().expect("open"),
            SyncClientConfig::default(),
        );
        let mk = |pk: &str| nostos_core::PendingWrite {
            table: "tasks".into(),
            op: nostos_core::WriteOp::Upsert,
            pk: pk.into(),
            payload_json: Some(format!(r#"{{"id":"{pk}"}}"#)),
        };

        let first = client.write(mk("t1")).await.expect("enqueue");
        client.write(mk("t2")).await.expect("enqueue");
        assert_eq!(client.write_status().pending, 2);

        client.mark_write_done(first).await.expect("ack");
        assert_eq!(client.write_status().pending, 1);

        // The server redelivers the same WriteResult — nothing left to remove.
        client
            .mark_write_done(first)
            .await
            .expect("redelivered ack");
        assert_eq!(
            client.write_status().pending,
            1,
            "t2 is still queued; a duplicate ack must not drop the count to 0"
        );
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

    /// `set_token` must reach `connect_url`, or a refreshed JWT never gets used
    /// and the client retries an expired one until `max_retries`.
    #[test]
    fn set_token_changes_the_next_connect_url() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig {
                token: Some("stale".into()),
                ..SyncClientConfig::default()
            },
        );
        assert_eq!(c.connect_url(), "ws://localhost:9999/sync?token=stale");

        c.set_token(Some("fresh".into()));
        assert_eq!(
            c.connect_url(),
            "ws://localhost:9999/sync?token=fresh",
            "refreshed token must be used by the next connection"
        );

        // Clearing drops the query param entirely (anonymous / NOSTOS_SYNC_AUTH=none).
        c.set_token(None);
        assert_eq!(c.connect_url(), "ws://localhost:9999/sync");

        // An empty string is treated as absent, same as the seed path.
        c.set_token(Some(String::new()));
        assert_eq!(c.connect_url(), "ws://localhost:9999/sync");
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

    #[tokio::test]
    async fn write_broadcasts_change_tick_so_offline_writes_are_visible() {
        // WS2 slice-2 regression (offline-first): write() must broadcast a
        // checkpoint-preserving change tick after the optimistic local apply,
        // so a live watch pump re-queries and renders the row BEFORE the server
        // echoes it. Before the fix, apply_local wrote the row into cairn_data
        // but never notified `changes`, so an offline write was
        // durable-but-invisible until the echo — the "offline-first broken"
        // symptom. Checkpoint preservation is covered by the sqlite test
        // `apply_local_renders_instantly_and_echo_reconciles`; this asserts the
        // missing half: the broadcast.
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

        let _id = c
            .write(nostos_core::PendingWrite {
                table: "tasks".into(),
                op: nostos_core::WriteOp::Upsert,
                pk: "t1".into(),
                payload_json: Some(r#"{"id":"t1","title":"optimistic"}"#.into()),
            })
            .await
            .unwrap();

        // The fix: a change tick arrives immediately — no server round-trip,
        // no run_once loop needed. The watch pump re-snapshots on this tick.
        let outcome = rx.try_recv().expect(
            "write() must broadcast a change tick so the optimistic row is visible offline",
        );
        assert_eq!(outcome.rows_applied, 1);
    }

    // Regression for the "connected but lists render empty" bug. The FFI
    // `watch()` (sdk/nostos_flutter/rust/src/api/nostos.rs) must create its
    // `subscribe_changes()` receiver BEFORE its initial snapshot read, because
    // this broadcast channel has NO replay buffer — a receiver created after a
    // commit permanently misses it. This test encodes the invariant directly:
    // receiver-before-apply sees the tick; receiver-after-apply does not.
    #[tokio::test]
    async fn subscribe_changes_must_precede_apply_to_avoid_missed_snapshot() {
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig::default(),
        );

        // The correct order (what the FFI now does): subscribe, then write.
        let mut rx_before = c.subscribe_changes();
        c.write(nostos_core::PendingWrite {
            table: "tasks".into(),
            op: nostos_core::WriteOp::Upsert,
            pk: "late".into(),
            payload_json: Some(r#"{"id":"late","title":"seen"}"#.into()),
        })
        .await
        .unwrap();
        assert!(
            rx_before.try_recv().is_ok(),
            "receiver created BEFORE write must catch the change tick"
        );

        // The buggy order (receiver-after-write): the tick is lost forever.
        c.write(nostos_core::PendingWrite {
            table: "tasks".into(),
            op: nostos_core::WriteOp::Upsert,
            pk: "lost".into(),
            payload_json: Some(r#"{"id":"lost","title":"missed"}"#.into()),
        })
        .await
        .unwrap();
        let mut rx_after = c.subscribe_changes();
        assert!(
            matches!(
                rx_after.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "receiver created AFTER write misses the tick (no replay buffer) — \
             this is why watch() must subscribe before emit_snapshot"
        );
    }

    // NOTE: the end-to-end client behavior (subscribe, apply, reconnect) is
    // proven in crates/nostos-client/tests/chaos_resume.rs against a real
    // in-process server + FakeReplicator — that's where zero-loss/zero-dup is
    // asserted over a genuine socket, not a unit test here.
}
