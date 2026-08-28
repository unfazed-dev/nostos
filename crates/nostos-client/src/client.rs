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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use nostos_core::{ApplyEngine, ApplyOutcome, Frame, Outbox, PendingWrite};
use nostos_domain::Lsn;
use nostos_infra::wire::{
    decode_frames, decode_resume_info, decode_resync_required, decode_snapshot_boundary,
    decode_stream_error, ClientMessage,
};
use futures_util::{Sink, SinkExt, StreamExt};
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use uuid::Uuid;

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

/// A config-declared sync stream (P5 §4 — docs/plans/p5-sync-streams-design.md):
/// sugar for calling `sync_stream(name, params).subscribe()` at connect time.
/// `params` binds the server-defined stream's `:param` placeholders —
/// value-level, never textual (design Decision 2).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamDecl {
    /// The server-defined stream name (`[streams.<name>]` in nostos_rules.toml).
    pub name: String,
    /// Bind values for the template's `:param` placeholders (JSON scalars only).
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl StreamDecl {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        params: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            params,
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
    /// Server-defined sync streams to activate at connect (P5 §4) — config
    /// sugar; the same streams could be added lazily via
    /// [`SyncClient::sync_stream`]. Streams ride the socket's ONE global
    /// checkpoint (ADR-0009): there is no per-stream resume in v1, so every
    /// reconnect re-subscribes them and each re-add takes a fresh targeted
    /// snapshot. They count against the same per-socket 32-subscription cap.
    pub streams: Vec<StreamDecl>,
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
    /// Tables this client treats as add-wins OR-sets (ADR-0030). An
    /// `or_set_add` / `or_set_remove` on a table NOT in this set returns
    /// [`ClientError::OrSetTableNotTagged`] instead of silently storing a raw
    /// `OrSetPayload` (which the un-tagged storage apply path would write
    /// verbatim as the row value, clobbering concurrent elements — a verified
    /// silent-data-corruption defect). This set MUST match the storage's
    /// `with_or_set_tables` AND the server's `NOSTOS_OR_SET_COLUMNS`: the client
    /// gate, the storage tag, and the server column map are three views of one
    /// truth, and a mismatch (e.g. client tags but storage doesn't) still
    /// clobbers. Empty by default — OR-set opt-in is explicit.
    pub or_set_tables: HashSet<String>,
    /// Tables this client treats as PN-Counter CRDTs (ADR-0030 addendum). A
    /// `counter_increment` / `counter_decrement` on a table NOT in this set
    /// returns [`ClientError::CounterTableNotTagged`]. This set MUST match the
    /// storage's `with_counter_tables` AND the server's `NOSTOS_COUNTER_COLUMNS`
    /// (same three-views-of-one-truth rule as `or_set_tables`). Empty by default.
    pub counter_tables: HashSet<String>,
    /// Stable per-replica id for PN-Counter CRDT entries (ADR-0030 addendum).
    /// Each counter payload keys {p,n} by replica id; merge takes per-replica
    /// max. Defaults to a fresh UUID v4 — stable for the process lifetime. A
    /// persisted id (loaded from device storage on startup) gives cross-session
    /// stability so a restart doesn't seed a new replica entry each time.
    pub client_id: String,
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
            streams: Vec::new(),
            dead_letter_max_attempts: DEFAULT_DEAD_LETTER_MAX_ATTEMPTS,
            or_set_tables: HashSet::new(),
            counter_tables: HashSet::new(),
            client_id: Uuid::new_v4().to_string(),
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

/// A mid-session stream command (P5 §4) — the client→session-task queue. The
/// write path (ADR-0013 D3) already proved the notify+drain shape; streams
/// reuse it. In-memory only: server-side stream sessions die with the socket,
/// so nothing here needs durability — on reconnect the session re-sends the
/// ACTIVE set and drains the queue (both idempotent server-side: subscribe is
/// replace-by-id, unsubscribe of an unknown id is a no-op).
#[derive(Debug)]
enum StreamCommand {
    Subscribe {
        id: String,
        name: String,
        params: serde_json::Map<String, serde_json::Value>,
    },
    Unsubscribe {
        id: String,
    },
}

/// The client's stream state (P5 §4): which streams are active (re-sent on
/// every reconnect) plus commands queued while no session loop was draining.
#[derive(Debug, Default)]
struct StreamRegistry {
    /// Active streams by client-chosen id: `(name, params)`.
    active: HashMap<String, (String, serde_json::Map<String, serde_json::Value>)>,
    /// Commands queued but not yet sent on a live socket.
    pending: Vec<StreamCommand>,
    /// Monotonic id allocator for `sync_stream(...).subscribe()`.
    next_id: u64,
}

/// A live sync-stream subscription (P5 §4). `unsubscribe()` (or drop) queues
/// `unsubscribe_stream` and stops the reconnect re-send. v1 leaves local rows
/// in place on unsubscribe — eviction is separate; PowerSync behaves the same.
pub struct StreamHandle {
    id: String,
    streams: Arc<std::sync::Mutex<StreamRegistry>>,
    notify: Arc<Notify>,
    done: bool,
}

impl StreamHandle {
    /// The client-chosen stream id (matches `subscribe_stream`'s `id` and the
    /// `stream` key on this stream's snapshot boundary frames).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Unsubscribe now (idempotent; also runs on drop).
    pub fn unsubscribe(mut self) {
        self.unsub_inner();
    }

    fn unsub_inner(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        {
            let mut registry = self.streams.lock().expect("stream registry lock poisoned");
            registry.active.remove(&self.id);
            registry.pending.push(StreamCommand::Unsubscribe {
                id: self.id.clone(),
            });
        }
        self.notify.notify_one();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.unsub_inner();
    }
}

/// The builder returned by [`SyncClient::sync_stream`] — PowerSync's
/// `syncStream(name, params).subscribe()` shape (P5 §4).
pub struct StreamSubscription<'a, S>
where
    S: nostos_core::Storage + Outbox + Send + 'static,
{
    client: &'a SyncClient<S>,
    name: String,
    params: serde_json::Map<String, serde_json::Value>,
}

impl<S> StreamSubscription<'_, S>
where
    S: nostos_core::Storage + Outbox + Send + 'static,
{
    /// Activate the stream: queue the `subscribe_stream` frame for the live
    /// socket (or the next connect) and record it for reconnect re-send.
    ///
    /// Non-async in v1 (a deliberate deviation from the design sketch's
    /// `.subscribe().await?`): there is no server round-trip ack to await —
    /// rejects arrive asynchronously as `stream_error` frames and are logged
    /// loud. Params are a typed JSON map, so the object shape is enforced at
    /// the type level and there is nothing to validate here.
    pub fn subscribe(self) -> StreamHandle {
        let (id, streams, notify) = {
            let mut registry = self
                .client
                .streams
                .lock()
                .expect("stream registry lock poisoned");
            let id = format!("s-{}", registry.next_id);
            registry.next_id += 1;
            registry
                .active
                .insert(id.clone(), (self.name.clone(), self.params.clone()));
            registry.pending.push(StreamCommand::Subscribe {
                id: id.clone(),
                name: self.name,
                params: self.params,
            });
            (
                id,
                Arc::clone(&self.client.streams),
                Arc::clone(&self.client.stream_notify),
            )
        };
        notify.notify_one();
        StreamHandle {
            id,
            streams,
            notify,
            done: false,
        }
    }
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
    /// Client HLC state for optimistic OR-set edits (ADR-0030 Decision 4,
    /// relaxed): each `or_set_add` / `or_set_remove` mints the next HLC here so a
    /// local edit is comparable to remote elements on merge. `None` until the
    /// first mint. Mutex (not atomic) — mints are rare (user edits), and the
    /// read-modify-write needs the previous value.
    hlc_state: std::sync::Mutex<Option<nostos_domain::Hlc>>,
    /// Non-destructive disconnect gate (ADR-0037 task 5.1). `true` = a
    /// [`SyncClient::disconnect`] request is outstanding: the live
    /// `run_once`/`run_with_reconnect` loop winds down cleanly (final flush +
    /// ack, then return), and further runs return immediately until
    /// [`SyncClient::resume`] clears it. A `watch` channel (not an `AtomicBool`)
    /// because the run loops must WAKE from a parked `select!` on the request,
    /// not just notice it on the next iteration.
    disconnect_gate: tokio::sync::watch::Sender<bool>,
    /// P5 sync streams: active set + pending command queue (see
    /// `StreamRegistry`). std Mutex — critical sections are tiny and never
    /// span an await (same discipline as `hlc_state`). Arc'd so a
    /// `StreamHandle` outlives the client borrow that created it.
    streams: Arc<std::sync::Mutex<StreamRegistry>>,
    /// Wakes the session loop when a stream command lands mid-session (same
    /// pattern as `write_notify`).
    stream_notify: Arc<Notify>,
    /// TRUE only while the CURRENT session is PROVEN: the server accepted the
    /// subscribe(s) and at least one post-acceptance frame has arrived (a
    /// replication event, snapshot boundary, or write ack — `resume_info`
    /// deliberately does NOT count: the transport sends it BEFORE registering
    /// the first table, so a session that is about to be rejected still sees
    /// one). This is the honest `connected` signal UI layers were missing
    /// (observed 2026-08-27: a rules-rejected subscribe loop flapped
    /// `Connected` on a grace-window heuristic while ZERO rows ever arrived,
    /// and `waitForFirstSync()` completed against a session that never
    /// synced). `watch` (not broadcast): a late subscriber must read the
    /// current value, and intermediate flaps coalesce. `run_once` clears it
    /// at session start; [`Self::reset_subscribed`] lets a caller clear it
    /// synchronously before spawning the next attempt (bridge loops need
    /// this to avoid selecting on a stale `true` from the previous session).
    subscribed: tokio::sync::watch::Sender<bool>,
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
        let (disconnect_gate, _) = tokio::sync::watch::channel(false);
        let (subscribed, _) = tokio::sync::watch::channel(false);
        // P5: config-declared streams seed the ACTIVE set directly (no
        // pending entry needed — run_once re-sends the active set on every
        // connect, which covers the first one too).
        let streams = {
            let mut registry = StreamRegistry::default();
            for decl in &config.streams {
                let id = format!("cfg-{}", registry.next_id);
                registry.next_id += 1;
                registry
                    .active
                    .insert(id, (decl.name.clone(), decl.params.clone()));
            }
            Arc::new(std::sync::Mutex::new(registry))
        };
        Self {
            url: url.into(),
            config,
            token,
            engine,
            changes,
            write_notify: Notify::new(),
            write_status,
            hlc_state: std::sync::Mutex::new(None),
            disconnect_gate,
            subscribed,
            streams,
            stream_notify: Arc::new(Notify::new()),
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

    /// Declare a parameterized sync-stream subscription (P5 —
    /// docs/plans/p5-sync-streams-design.md §4), PowerSync's
    /// `syncStream(name, params).subscribe()` shape.
    ///
    /// Lazy: the `subscribe_stream` frame goes out on the live socket if one
    /// is connected, else on the next (re)connect. The stream re-subscribes
    /// on EVERY reconnect (each re-add takes a fresh targeted snapshot — no
    /// per-stream resume in v1; the socket's one checkpoint + idempotent
    /// apply already prevent duplicate rows) until the returned
    /// [`StreamHandle`] is unsubscribed or dropped. Rows surface through the
    /// existing reactive layer ([`Self::subscribe_changes`] + storage) —
    /// streams control WHICH rows land, not how reads react.
    ///
    /// The server answers rejects with a non-fatal `stream_error` frame
    /// (unknown stream, bad params), logged loud; the socket stays up.
    pub fn sync_stream(
        &self,
        name: impl Into<String>,
        params: serde_json::Map<String, serde_json::Value>,
    ) -> StreamSubscription<'_, S> {
        StreamSubscription {
            client: self,
            name: name.into(),
            params,
        }
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

    /// Read the rules checksum this client last synced under (ADR-0031 D2).
    /// 0 on a fresh DB (or a `resume_info` that never carried a checksum) →
    /// the Subscribe omits `rules_checksum` → the server uses the composed-
    /// epoch fallback. Delegates through the engine.
    pub async fn rules_checksum(&self) -> nostos_core::Result<u64> {
        self.engine.lock().await.rules_checksum()
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

    /// Enqueue a batch of writes atomically (all-or-nothing) — ADR-0032 T3.
    /// All writes land in one storage transaction or none do. Each write is
    /// also applied optimistically (instant-local). Returns the outbox ids in
    /// the same order as `writes`.
    pub async fn write_batch(&self, writes: Vec<PendingWrite>) -> Result<Vec<u64>, ClientError> {
        let n = writes.len();
        let engine = Arc::clone(&self.engine);
        let (ids, local_tick) =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<(Vec<u64>, Option<ApplyOutcome>)> {
                let mut engine = engine.blocking_lock();
                // Atomic enqueue: all-or-nothing at the storage level.
                let ids = engine.storage_mut().enqueue_batch(writes.clone())?;
                // Optimistic local apply for each write (same as write()).
                let mut applied_count = 0usize;
                for w in &writes {
                    match engine.storage_mut().apply_local(w) {
                        Ok(()) => applied_count += 1,
                        Err(e) => {
                            warn!(error = %e, "instant-local apply failed in batch; write still queued");
                        }
                    }
                }
                let local_tick = if applied_count > 0 {
                    engine
                        .checkpoint()
                        .ok()
                        .map(|checkpoint| ApplyOutcome { checkpoint, rows_applied: applied_count })
                } else {
                    None
                };
                Ok((ids, local_tick))
            })
            .await
            .map_err(|e| ClientError::Join(e.to_string()))??;
        debug!(n, "enqueued batch of writes to outbox atomically");
        self.update_write_status(|s| s.pending += n as u64);
        if let Some(tick) = local_tick {
            let _ = self.changes.send(tick);
        }
        self.write_notify.notify_one();
        Ok(ids)
    }

    /// Add `element` to the add-wins OR-set in row `pk` of `table` (ADR-0030).
    /// Mints a client HLC for the add, enqueues a merge-upsert, and applies
    /// optimistically — the element renders locally immediately and converges
    /// with concurrent remote adds on the server's echo (no clobber). Requires
    /// the storage to tag `table` as an OR-set ([`SqliteStorage::with_or_set_tables`]
    /// / [`nostos_core::InMemoryStorage::with_or_set_tables`]).
    pub async fn or_set_add(
        &self,
        table: &str,
        pk: &str,
        element: &str,
    ) -> Result<u64, ClientError> {
        self.or_set_op(table, pk, element, false).await
    }

    /// Remove `element` from the OR-set — a tombstone at a fresh HLC. Add-wins:
    /// a concurrent or later re-add (a higher HLC) re-activates the element.
    pub async fn or_set_remove(
        &self,
        table: &str,
        pk: &str,
        element: &str,
    ) -> Result<u64, ClientError> {
        self.or_set_op(table, pk, element, true).await
    }

    /// Shared add/remove builder: mint one client HLC, wrap the single element
    /// in an [`nostos_domain::OrSetPayload`], and enqueue it as a merge-upsert.
    /// The optimistic apply (`write` → `apply_local`) merges element-wise by HLC.
    async fn or_set_op(
        &self,
        table: &str,
        pk: &str,
        element: &str,
        remove: bool,
    ) -> Result<u64, ClientError> {
        // Loud-fail gate (ADR-0030): a table the client does not tag as an
        // OR-set must never reach the enqueue path. Without it the OrSetPayload
        // is stored verbatim as the row value on an un-tagged storage apply path
        // → concurrent elements silently clobber. The tag here MUST match the
        // storage's `with_or_set_tables` AND the server's `NOSTOS_OR_SET_COLUMNS`.
        if !self.config.or_set_tables.contains(table) {
            return Err(ClientError::OrSetTableNotTagged(table.to_string()));
        }
        let h = self.mint_hlc();
        let element = nostos_domain::OrSetElement {
            v: element.to_string(),
            // A remove carries no add (`h = ZERO`); its tombstone `d` is the
            // minted HLC. The merge takes the per-element max, so a real add's
            // HLC is never reduced and add-wins holds.
            h: if remove { nostos_domain::Hlc::ZERO } else { h },
            d: if remove { Some(h) } else { None },
        };
        let payload = serde_json::to_string(&nostos_domain::OrSetPayload {
            elements: vec![element],
        })
        .expect("OrSetPayload serializes infallibly");
        self.write(nostos_core::PendingWrite {
            table: table.to_string(),
            op: nostos_core::WriteOp::Upsert,
            pk: pk.to_string(),
            payload_json: Some(payload),
        })
        .await
    }

    /// Increment the PN-Counter in row `pk` of `table` by `delta` (ADR-0030
    /// addendum). Read-modify-write: reads the current counter payload, applies
    /// the delta to this replica's entry, and enqueues the full result. The
    /// per-replica max merge converges across replicas; the read-modify-write
    /// makes same-replica increments cumulative (not clobbered by a concurrent
    /// same-replica delta). Requires the storage to tag `table` as a counter
    /// ([`SqliteStorage::with_counter_tables`] /
    /// [`nostos_core::InMemoryStorage::with_counter_tables`]).
    pub async fn counter_increment(
        &self,
        table: &str,
        pk: &str,
        delta: i64,
    ) -> Result<u64, ClientError> {
        self.counter_op(table, pk, delta).await
    }

    /// Decrement the PN-Counter by `delta` (bumps the negative counter `n` for
    /// this replica). Same read-modify-write as [`Self::counter_increment`].
    pub async fn counter_decrement(
        &self,
        table: &str,
        pk: &str,
        delta: u64,
    ) -> Result<u64, ClientError> {
        let neg = i64::try_from(delta).map_or(i64::MIN, |d| -d);
        self.counter_op(table, pk, neg).await
    }

    /// Shared counter read-modify-write: read current payload, apply delta to
    /// this replica's entry, enqueue as a merge-upsert. The read + delta-apply +
    /// enqueue + optimistic apply all run under ONE engine lock, so two
    /// concurrent local increments on the same pk serialize (no lost update
    /// from same-replica races — the crux of PN-Counters).
    async fn counter_op(&self, table: &str, pk: &str, delta: i64) -> Result<u64, ClientError> {
        // Loud-fail gate (ADR-0030 addendum): a table the client does not tag
        // as a counter must never reach the enqueue path. Same three-views-of-
        // one-truth rule as or_set_tables — this tag MUST match the storage's
        // `with_counter_tables` AND the server's `NOSTOS_COUNTER_COLUMNS`.
        if !self.config.counter_tables.contains(table) {
            return Err(ClientError::CounterTableNotTagged(table.to_string()));
        }
        let replica = self.config.client_id.clone();
        let table_owned = table.to_string();
        let pk_owned = pk.to_string();
        let engine = Arc::clone(&self.engine);
        let (id, local_tick) =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<(u64, Option<ApplyOutcome>)> {
                let mut engine = engine.blocking_lock();
                // Read-modify-write: read the current counter payload, apply the
                // delta to this replica's entry, produce the new full payload.
                // The enqueued payload carries the bumped cumulative value, so
                // the per-replica max merge can't lose a delta to a concurrent
                // same-replica write (the PN-Counter cumulative-increment crux).
                let existing = engine
                    .storage()
                    .read_payload(&table_owned, &pk_owned)?
                    .unwrap_or_default();
                let payload_bytes =
                    nostos_domain::counter_apply_delta(&existing, &replica, delta);
                let payload_json = String::from_utf8(payload_bytes)
                    .expect("counter_apply_delta serializes valid UTF-8 JSON");
                let write = nostos_core::PendingWrite {
                    table: table_owned,
                    op: nostos_core::WriteOp::Upsert,
                    pk: pk_owned,
                    payload_json: Some(payload_json),
                };
                let id = engine.storage_mut().enqueue(write.clone())?;
                let applied = match engine.storage_mut().apply_local(&write) {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(write_id = id, error = %e, "counter instant-local apply failed; write still queued");
                        false
                    }
                };
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
        debug!(write_id = id, "enqueued counter write to outbox");
        self.update_write_status(|s| s.pending += 1);
        if let Some(tick) = local_tick {
            let _ = self.changes.send(tick);
        }
        self.write_notify.notify_one();
        Ok(id)
    }

    /// Mint the next client HLC, advancing this client's monotone HLC state
    /// (ADR-0030 Decision 4). Wall time from the system clock; on clock error
    /// falls back to 0 (the logical counter still preserves monotonicity).
    fn mint_hlc(&self) -> nostos_domain::Hlc {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u64, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut state = self.hlc_state.lock().expect("hlc_state lock poisoned");
        let h = nostos_domain::Hlc::mint(*state, now_ms);
        *state = Some(h);
        h
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

    /// ADR-0029 D1: wipe local rows AND the outbox — the sign-out local-state
    /// wipe. The SDK binding's `signOut()` must abort its run loop FIRST
    /// (quiescence), then call this: clearing under a live apply/flush loop
    /// races (a post-clear frame re-populates storage; a post-clear flush
    /// re-queues the outbox). Half a clear is a cross-user leak, so both
    /// [`nostos_core::Storage::clear`] AND [`nostos_core::Outbox::clear`] run
    /// under one engine lock. Idempotent.
    pub async fn clear_local_state(&self) -> Result<(), ClientError> {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || -> nostos_core::Result<()> {
            let mut engine = engine.blocking_lock();
            let s = engine.storage_mut();
            <S as nostos_core::Storage>::clear(s)?;
            <S as nostos_core::Outbox>::clear(s)?;
            Ok(())
        })
        .await
        .map_err(|e| ClientError::Join(e.to_string()))?
        .map_err(ClientError::Storage)
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
        let error_owned = server_error.map(str::to_string);
        let (attempts, dld, remaining) =
            tokio::task::spawn_blocking(move || -> nostos_core::Result<(u32, bool, u64)> {
                let engine = engine.blocking_lock();
                let count = engine.storage().bump_attempts(id)?;
                if count >= max {
                    engine
                        .storage()
                        .mark_dead_letter_with_error(id, error_owned.as_deref())?;
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

    /// Drain the queued stream commands onto the live socket (P5 §4). Every
    /// send is idempotent server-side (subscribe replaces by id; unsubscribe
    /// of an unknown id is a no-op), so a command that raced a reconnect is
    /// safe to send twice — the reconnect path re-sends the active set AND
    /// drains this queue verbatim rather than trying to dedup the two.
    async fn drain_stream_commands<W>(&self, write: &mut W) -> Result<(), ClientError>
    where
        W: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        let cmds = {
            let mut registry = self.streams.lock().expect("stream registry lock poisoned");
            std::mem::take(&mut registry.pending)
        };
        for cmd in cmds {
            let frame = match cmd {
                StreamCommand::Subscribe { id, name, params } => ClientMessage::SubscribeStream {
                    id,
                    stream: name,
                    params,
                },
                StreamCommand::Unsubscribe { id } => ClientMessage::UnsubscribeStream { id },
            };
            let json = serde_json::to_string(&frame).expect("stream frame serializes");
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

    /// Request a NON-DESTRUCTIVE disconnect (ADR-0037 task 5.1): the live
    /// [`Self::run_once`]/[`Self::run_with_reconnect`] loop winds down cleanly —
    /// the receive loop breaks, any buffered batch is force-flushed, the final
    /// checkpoint is ack'd, and the WS stream drops with the task. The durable
    /// store (rows + checkpoint + epoch + outbox) is UNTOUCHED — this is the
    /// push-notification sleep primitive, the sibling of [`Self::resume`], and
    /// the exact counterpart of `nostos_node`'s `close()`. Contrast
    /// [`Self::clear_local_state`], which wipes everything and is only for
    /// sign-out (ADR-0029).
    ///
    /// Synchronous and callable from any thread: it only sets the gate — the
    /// loop notices at its next `select!` iteration, so a frame mid-apply still
    /// lands atomically (no torn batches). Idempotent; safe with no live loop
    /// (a later `run_once`/`run_with_reconnect` returns immediately until
    /// [`Self::resume`]). `Self::checkpoint`, `Self::write`, and
    /// `Self::with_storage` keep working while disconnected — the engine is
    /// still live, only the socket is gone.
    pub fn disconnect(&self) {
        self.disconnect_gate.send_replace(true);
    }

    /// Clear the disconnect request (ADR-0037 task 5.1) so the next
    /// `run_once`/`run_with_reconnect` may connect again. The reconnected
    /// session re-seeds `resume_lsn` from the durable checkpoint (see
    /// [`Self::run_once`]), so only the delta past the checkpoint flows — this
    /// is the wake primitive a push-poked backgrounded app calls. Synchronous,
    /// idempotent, and a no-op on a client that was never disconnected.
    ///
    /// Does NOT itself start a loop: the caller re-enters
    /// [`Self::run_with_reconnect`] (the mobile SDKs' `resume()` does exactly
    /// that — `SyncClient` does not own its run task; the embedding does).
    pub fn resume(&self) {
        self.disconnect_gate.send_replace(false);
    }

    /// The honest session-proven signal (see the `subscribed` field): a
    /// receiver that flips `true` once the CURRENT session's subscribe has
    /// been accepted AND a post-acceptance frame has arrived. A UI layer
    /// that renders "synced" off this cannot lie the way a connect-grace
    /// heuristic did (2026-08-27: rejected-subscribe loops showed
    /// `Connected` with zero rows ever delivered).
    ///
    /// The value resets to `false` at the start of every `run_once`. A caller
    /// driving its own retry loop should [`Self::reset_subscribed`] before
    /// spawning the next attempt so it never selects on a stale `true`.
    #[must_use]
    pub fn subscribed(&self) -> tokio::sync::watch::Receiver<bool> {
        self.subscribed.subscribe()
    }

    /// Synchronously clear the session-proven signal (see [`Self::subscribed`]).
    /// For bridge/retry loops that must not observe the PREVIOUS session's
    /// `true` while the next `run_once` is still between creation and its
    /// first poll (the internal reset only runs once the future is polled).
    pub fn reset_subscribed(&self) {
        self.subscribed.send_if_modified(|v| {
            if !*v {
                false
            } else {
                *v = false;
                true
            }
        });
    }

    /// Mark the current session PROVEN. Private: only the receive loop may
    /// say the server accepted us (a post-acceptance frame arrived).
    fn mark_subscribed(&self) {
        self.subscribed.send_if_modified(|v| {
            if *v {
                false
            } else {
                *v = true;
                true
            }
        });
    }

    /// Run one connection attempt to completion: connect, subscribe, apply until
    /// the stream ends or errors. Does NOT reconnect on its own — see
    /// [`Self::run_with_reconnect`]. Returns the session outcome.
    ///
    /// # Errors
    /// Returns the underlying error if the connection can't be established or
    /// the apply loop hits a non-recoverable storage error. A first-subscribe
    /// rejection surfaces as [`ClientError::SubscribeRejected`] carrying the
    /// server's close reason.
    pub async fn run_once(&self) -> Result<SessionOutcome, ClientError> {
        let mut disconnect_rx = self.disconnect_gate.subscribe();
        // New session: the previous session's PROVEN flag is stale until the
        // server accepts THIS session's subscribe (see `subscribed`).
        self.reset_subscribed();
        // Already-requested disconnect (loop spawned after disconnect()): a
        // clean no-op session so run_with_reconnect terminates instead of
        // reconnecting forever against a sleeping client.
        if *disconnect_rx.borrow() {
            let checkpoint = self.checkpoint().await?;
            return Ok(SessionOutcome {
                frames_received: 0,
                commits: 0,
                checkpoint,
            });
        }
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
        // ADR-0031 D2: send the last-synced ruleset checksum so the server can
        // advertise a raw (epoch, checksum) pair in resume_info instead of the
        // composed fallback — 0 on a fresh DB (or a resume_info that never
        // carried one) → None → composed-epoch fallback (Task 11).
        let client_rules_checksum = self.rules_checksum().await?;
        let subscribe = ClientMessage::Subscribe {
            table: self.config.table.clone(),
            filters: vec![],
            where_sql: self.config.where_sql.clone(),
            resume_lsn: (resume_lsn > Lsn::ZERO).then_some(resume_lsn.raw()),
            epoch: (client_epoch > 0).then_some(client_epoch),
            rules_checksum: (client_rules_checksum > 0).then_some(client_rules_checksum),
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
                rules_checksum: (client_rules_checksum > 0).then_some(client_rules_checksum),
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

        // P5 §4: re-send every ACTIVE stream after the primary subscribe on
        // every (re)connect — each re-add takes a fresh targeted snapshot (no
        // per-stream resume in v1; the socket checkpoint + idempotent apply
        // prevent duplicate rows). Then drain the pending queue VERBATIM:
        // subscribes queued while disconnected are covered by the active
        // re-send (idempotent replace server-side), and a queued unsubscribe
        // is a no-op on a fresh socket — so verbatim draining loses nothing
        // and a command that raced the handshake is never dropped.
        {
            let active: Vec<(String, String, serde_json::Map<String, serde_json::Value>)> = {
                let registry = self.streams.lock().expect("stream registry lock poisoned");
                registry
                    .active
                    .iter()
                    .map(|(id, (name, params))| (id.clone(), name.clone(), params.clone()))
                    .collect()
            };
            for (id, name, params) in active {
                let frame = ClientMessage::SubscribeStream {
                    id,
                    stream: name,
                    params,
                };
                let json = serde_json::to_string(&frame).expect("subscribe_stream serializes");
                write
                    .send(Message::Text(json))
                    .await
                    .map_err(|e| ClientError::Send(e.to_string()))?;
            }
            self.drain_stream_commands(&mut write).await?;
        }

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
        // Four independent triggers race each loop iteration:
        //   1. the next WS message (idle_timeout-bounded if configured — a
        //      long gap with NOTHING pending means "caught up", so break and
        //      return the session, the "sync then disconnect" shape);
        //   2. the flush-quiesce timer, armed only while a batch is buffered
        //      (`ApplyEngine::has_pending`) — closes a real Postgres
        //      transaction's frames when they're the last activity on an
        //      otherwise-idle table (see `SyncClientConfig::flush_quiesce`);
        //   3. `write_notify` — a write enqueued mid-session, resent now
        //      instead of waiting for a reconnect (see `Self::write`);
        //   4. the disconnect gate — a `disconnect()` request breaks the loop
        //      through the same clean tail (non-destructive teardown,
        //      ADR-0037 task 5.1).
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
                        Message::Close(frame) => {
                            // A first-subscribe rejection closes the socket
                            // with code INVALID (1008) + the rejection
                            // reason (see `run_session`'s fatal-reject path).
                            // Swallowing it (the old behavior) turned a
                            // rules misconfiguration into an infinite,
                            // silent reconnect loop that still LOOKED
                            // connected-ish to UI layers (2026-08-27 incident:
                            // `waitForFirstSync()` completed against a
                            // session that never synced). Surface it as a
                            // distinct error so retry loops can treat it as
                            // fatal and operators see the reason. The one
                            // INVALID close that is NOT a rejection is the
                            // rules-changed reconnect request — same code,
                            // reserved reason string (the transport's
                            // `RULES_CHANGED_CLOSE_REASON`).
                            // Trigger on the REASON, not the code: every
                            // diagnostic close the transport sends carries a
                            // non-empty reason, while the clean paths do not
                            // (device-cap closes with `reason: ""`; idle
                            // closes are bare). Matching a specific code is
                            // fragile across stacks — the server's axum
                            // `close_code::INVALID` arrives as tungstenite
                            // `CloseCode::Invalid` here, whose numeric value
                            // is NOT what a code-number match would guess.
                            // The one reserved diagnostic reason that is NOT
                            // a rejection is rules-changed (a reconnect
                            // request).
                            let reason = frame
                                .as_ref()
                                .map(|f| f.reason.clone().into_owned())
                                .unwrap_or_default();
                            let is_rules_changed =
                                reason == nostos_infra::transport::RULES_CHANGED_CLOSE_REASON;
                            if !reason.is_empty() && !is_rules_changed {
                                warn!(%reason, "subscribe rejected; server closed with reason");
                                return Err(ClientError::SubscribeRejected(reason));
                            }
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
                        // A write ack proves the server accepted this session
                        // (the transport only processes writes for registered
                        // tables) — mark the session PROVEN (see `subscribed`).
                        self.mark_subscribed();
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

                    // ADR-0025 F2 + ADR-0031 D2: `resume_info` advertises the
                    // server's current slot epoch, and MAY also carry a rules
                    // checksum (raw pair — see `resume_advertisement` in
                    // nostos-infra/transport.rs). Persist both so the NEXT
                    // reconnect's Subscribe carries what this session was gated
                    // against. Both writes happen in the SAME spawn_blocking
                    // hop: two hops could interleave and strand a half-updated
                    // pair (an epoch from one resume_info paired with a
                    // checksum from a different one). Absent checksum: leave
                    // the stored value untouched, don't zero it — a stray frame
                    // without a checksum must not downgrade a client that's
                    // already on the explicit path back to the composed
                    // fallback. Intercepted before the row path; never batched
                    // with events.
                    if let Some((epoch, rules_checksum)) = decode_resume_info(&bytes) {
                        let engine = Arc::clone(&self.engine);
                        // Non-fatal: a persist failure just means the next
                        // reconnect falls back to snapshot (epoch unknown) — it
                        // must NOT kill this session's data delivery.
                        match tokio::task::spawn_blocking(move || -> nostos_core::Result<()> {
                            let engine = engine.blocking_lock();
                            engine.save_epoch(epoch)?;
                            if let Some(checksum) = rules_checksum {
                                engine.save_rules_checksum(checksum)?;
                            }
                            Ok(())
                        })
                        .await
                        .map_err(|e| ClientError::Join(e.to_string()))
                        {
                            Ok(Ok(())) => {
                                debug!(
                                    server_epoch = epoch,
                                    rules_checksum = ?rules_checksum,
                                    "resume_info received — epoch + checksum persisted"
                                );
                            }
                            Ok(Err(e)) => warn!(
                                server_epoch = epoch,
                                rules_checksum = ?rules_checksum,
                                error = %e,
                                "save_epoch/save_rules_checksum failed; next reconnect falls back to snapshot"
                            ),
                            Err(e) => warn!(
                                server_epoch = epoch,
                                rules_checksum = ?rules_checksum,
                                error = %e,
                                "resume_info persist task join failed"
                            ),
                        }
                        last_frame_at = tokio::time::Instant::now();
                        continue;
                    }

                    // P5 §1: a `stream_error` is the server's NON-fatal stream
                    // reject (unknown stream, bad params). v1 surfaces it loud
                    // in logs only — the active set keeps the entry, so a
                    // reconnect retries (a server-side stream-definition fix
                    // hot-reloads and the next attempt succeeds). The row path
                    // never sees it (no lsn/op/pk → wouldn't decode as a frame).
                    if let Some((stream_id, error)) = decode_stream_error(&bytes) {
                        warn!(stream_id = %stream_id, %error, "stream_error from server");
                        continue;
                    }

                    // ADR-0040: `resync_required` = the server shed events on
                    // this stream (capacity loss; no replay path for a live
                    // session). Recovery: wipe rows/checkpoint/epoch (`clear`
                    // zeroes epoch too), end THIS session, re-arm the gate,
                    // and surface an Err so `run_with_reconnect` retries into
                    // a fresh subscribe whose snapshot reconciles the gap.
                    // (A bare `disconnect()` would make the Ok path terminate
                    // the whole loop — the exact bug this sequence avoids.)
                    if let Some((table, reason)) = decode_resync_required(&bytes) {
                        warn!(table = %table, %reason, "resync_required from server");
                        debug!(table = %table, %reason, "[ADR-0040] resync signal received; clearing local state");
                        self.clear_local_state().await?;
                        debug!("[ADR-0040] local state cleared; disconnecting for fresh reconcile");
                        self.disconnect();
                        self.resume();
                        return Err(ClientError::ResyncRequired(reason));
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
                    //
                    // P5 (design §1): a STREAM-targeted boundary carries an
                    // extra `"stream":"<id>"` key and brackets only the SUBSET
                    // of the table matching the stream's bound predicate.
                    // Driving the table-scoped reap from it would DELETE every
                    // local row outside that subset (e.g. the base table
                    // subscription's rows) — so stream-tagged boundaries are
                    // logged but NEVER reach the engine. Only untagged,
                    // table-level boundaries reconcile.
                    if let Some((table, stream, begin)) = decode_snapshot_boundary(&bytes) {
                        // A snapshot boundary only ever follows an ACCEPTED
                        // subscribe (the transport brackets the snapshot it
                        // takes at registration) — including the empty-table
                        // case, where begin/end still bracket zero rows. This
                        // is the guaranteed first PROVEN frame; the decode
                        // loop below marks the streaming path.
                        self.mark_subscribed();
                        if let Some(stream_id) = stream {
                            debug!(table = %table, stream = %stream_id, begin,
                                "stream snapshot boundary — subset bracket, no reconcile");
                            continue;
                        }
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
                        // Replication events prove the session doubly
                        // (acceptance + data); marked here so the flag is set
                        // even if this table's snapshot bracket was consumed
                        // before this client attached (resume path).
                        self.mark_subscribed();

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

                // ---- Branch 5 (P5 §4): a stream command queued mid-session.
                //      Placed after write_notify deliberately: outbox writes
                //      are user data, stream commands are read-path shaping. ----
                () = self.stream_notify.notified() => {
                    self.drain_stream_commands(&mut write).await?;
                }

                // ---- Branch 4: disconnect() requested — wind down cleanly.
                //      The loop break routes through the same final flush +
                //      checkpoint tail as a clean stream end, so nothing
                //      buffered is lost. A false transition (a resume() racing
                //      the loop's exit) just keeps the session alive. ----
                _ = disconnect_rx.changed() => {
                    if *disconnect_rx.borrow_and_update() {
                        debug!("disconnect() requested; winding down session");
                        break;
                    }
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
    ///
    /// A [`Self::disconnect`] request ends the loop from ANY state — mid-session
    /// (run_once breaks and returns cleanly) or mid-backoff (the sleep is
    /// gated; the next run_once is a no-op) — so the caller's `await` resolves
    /// promptly without an abort.
    pub async fn run_with_reconnect(&self) -> Result<SessionOutcome, ClientError> {
        let mut disconnect_rx = self.disconnect_gate.subscribe();
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
                    // A clean end (server-initiated close or disconnect())
                    // means we're done.
                    return Ok(SessionOutcome {
                        frames_received: total_frames,
                        commits: total_commits,
                        checkpoint: outcome.checkpoint,
                    });
                }
                Err(e) => {
                    // A subscribe rejection is FATAL: the server told us this
                    // session's rules deny the table — reconnecting re-sends
                    // the same denied subscribe forever (the 2026-08-27
                    // silent-loop incident). Return the reason to the caller;
                    // the operator fixes the ruleset, then the app reconnects.
                    if matches!(e, ClientError::SubscribeRejected(_)) {
                        return Err(e);
                    }
                    warn!(attempt, error = %e, "session failed; backing off");
                    if let Some(max) = self.config.max_retries {
                        if attempt >= max {
                            return Err(e);
                        }
                    }
                    // Gate the backoff sleep too: disconnect() while waiting to
                    // reconnect wakes here, the next run_once no-ops, and the
                    // loop returns — a sleeping client must not hold the task
                    // hostage for a full backoff window (up to max_backoff).
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        _ = disconnect_rx.changed() => {}
                    }
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
    /// The server REJECTED the session's first subscribe (rules denied the
    /// table, where_sql rejected, ...) and closed with its reason. NOT
    /// transient: retrying cannot heal a rules misconfiguration, so retry
    /// loops ([`SyncClient::run_with_reconnect`], the nostos_flutter bridge)
    /// treat this as fatal — surface it, fix the rules, resubscribe.
    #[error("subscribe rejected by server: {0}")]
    SubscribeRejected(String),
    #[error("send failed: {0}")]
    Send(String),
    #[error("receive failed: {0}")]
    Receive(String),
    #[error("apply task panicked/joined: {0}")]
    Join(String),
    #[error(transparent)]
    Storage(#[from] nostos_core::StorageError),
    #[error("or_set op on table not tagged as an OR-set: {0} — tag it in SyncClientConfig::or_set_tables, SqliteStorage::with_or_set_tables, and NOSTOS_OR_SET_COLUMNS")]
    OrSetTableNotTagged(String),
    #[error("counter op on table not tagged as a PN-Counter: {0} — tag it in SyncClientConfig::counter_tables, SqliteStorage::with_counter_tables, and NOSTOS_COUNTER_COLUMNS")]
    CounterTableNotTagged(String),
    #[error(
        "resync_required from server: {0} — local state cleared; the reconnect snapshot-reconciles"
    )]
    ResyncRequired(String),
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

    #[tokio::test]
    async fn clear_local_state_wipes_rows_and_outbox() {
        // ADR-0029 D1: the sign-out wipe at the SyncClient seam. The SDK
        // binding's `signOut()` aborts its run loop first (quiescence), then
        // calls this. The wipe semantics are unit-tested at the storage layer
        // (sqlite.rs `clear_*`); this proves the seam invokes BOTH clears.
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            nostos_core::InMemoryStorage::new(),
            SyncClientConfig::default(),
        );
        c.write(nostos_core::PendingWrite {
            table: "tasks".into(),
            op: nostos_core::WriteOp::Upsert,
            pk: "t1".into(),
            payload_json: Some(r#"{"id":"t1","title":"seed"}"#.into()),
        })
        .await
        .unwrap();

        // Pre: the optimistic write applied a row AND queued in the outbox.
        assert_eq!(
            c.with_storage(nostos_core::InMemoryStorage::row_count)
                .await
                .unwrap(),
            1,
            "seed row applied"
        );
        assert_eq!(
            c.with_storage(|s| s.pending().map_or(0, |p| p.len()))
                .await
                .unwrap(),
            1,
            "seed write queued in outbox"
        );

        c.clear_local_state().await.unwrap();

        // Post: both wiped — half a clear is a cross-user leak (ADR-0029).
        assert_eq!(
            c.with_storage(nostos_core::InMemoryStorage::row_count)
                .await
                .unwrap(),
            0,
            "clear_local_state wiped cairn_data"
        );
        assert_eq!(
            c.with_storage(|s| s.pending().map_or(0, |p| p.len()))
                .await
                .unwrap(),
            0,
            "clear_local_state drained the outbox"
        );
    }

    #[tokio::test]
    async fn or_set_add_merges_concurrent_adds_optimistically() {
        // ADR-0030 piece 2 slice 4: or_set_add mints a client HLC + enqueues a
        // merge-upsert; two adds of DIFFERENT elements to the same row MERGE
        // optimistically (both present) instead of clobbering — the offline-first
        // OR-set convergence that plain LWW can't do. Then a remove tombstones
        // one element (add-wins: the later remove HLC beats the earlier add).
        let storage = nostos_core::InMemoryStorage::new()
            .with_or_set_tables(["tags".to_string()].into_iter().collect::<HashSet<_>>());
        let mut config = SyncClientConfig::default();
        config.or_set_tables.insert("tags".to_string());
        let c = SyncClient::new("ws://localhost:9999/sync", storage, config);
        c.or_set_add("tags", "s1", "x").await.unwrap();
        c.or_set_add("tags", "s1", "y").await.unwrap();

        let mut present = c
            .with_storage(|s| {
                nostos_domain::present_elements(s.payload("tags", "s1").unwrap_or(&[])).unwrap()
            })
            .await
            .unwrap();
        present.sort();
        assert_eq!(present, vec!["x".to_string(), "y".to_string()]);

        // Remove x (mints a later HLC than both adds) → x tombstoned, y survives.
        c.or_set_remove("tags", "s1", "x").await.unwrap();
        let after = c
            .with_storage(|s| {
                nostos_domain::present_elements(s.payload("tags", "s1").unwrap_or(&[])).unwrap()
            })
            .await
            .unwrap();
        assert_eq!(
            after,
            vec!["y".to_string()],
            "remove tombstones x; y survives"
        );
    }

    // ADR-0030 loud-fail gate: an or_set op on a table NOT in
    // SyncClientConfig::or_set_tables must Err up front (not silently enqueue
    // a raw OrSetPayload that an un-tagged storage apply path would write
    // verbatim, clobbering concurrent elements). And it must leave the outbox
    // untouched — no write enqueued, no half-applied state.
    #[tokio::test]
    async fn or_set_add_on_untagged_table_errors() {
        // Storage tags "tags" as an OR-set, but the CLIENT config does not —
        // the client gate fires first, before the storage tag is even consulted.
        let storage = nostos_core::InMemoryStorage::new()
            .with_or_set_tables(["tags".to_string()].into_iter().collect::<HashSet<_>>());
        let c = SyncClient::new(
            "ws://localhost:9999/sync",
            storage,
            SyncClientConfig::default(), // or_set_tables empty by default
        );

        let pending_before = c
            .with_storage(|s| s.pending().map_or(0, |p| p.len()))
            .await
            .unwrap();

        let err = c
            .or_set_add("tags", "s1", "x")
            .await
            .expect_err("or_set_add on an un-tagged client table must Err");
        assert!(
            matches!(err, ClientError::OrSetTableNotTagged(ref t) if t == "tags"),
            "expected OrSetTableNotTagged(\"tags\"), got {err:?}"
        );

        let pending_after = c
            .with_storage(|s| s.pending().map_or(0, |p| p.len()))
            .await
            .unwrap();
        assert_eq!(
            pending_before, pending_after,
            "no write enqueued on a rejected or_set op"
        );
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

    // ---- P5 sync streams: registry/handle mechanics (socket-free) ----

    fn stream_client(config: SyncClientConfig) -> SyncClient<crate::SqliteStorage> {
        SyncClient::new(
            "ws://localhost:9999/sync",
            crate::SqliteStorage::open_in_memory().expect("open"),
            config,
        )
    }

    fn params(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn sync_stream_subscribe_activates_and_queues() {
        let client = stream_client(SyncClientConfig::default());
        let handle = client
            .sync_stream("lists", params(&[("owner", serde_json::json!("u1"))]))
            .subscribe();
        assert_eq!(handle.id(), "s-0");
        let registry = client.streams.lock().expect("lock");
        assert_eq!(registry.active.len(), 1);
        assert!(registry.active["s-0"].0 == "lists");
        assert!(
            matches!(registry.pending.first(), Some(StreamCommand::Subscribe { id, .. }) if id == "s-0")
        );
    }

    #[test]
    fn stream_ids_are_unique_per_subscribe() {
        let client = stream_client(SyncClientConfig::default());
        let a = client.sync_stream("lists", params(&[])).subscribe();
        let b = client.sync_stream("inbox", params(&[])).subscribe();
        assert_ne!(a.id(), b.id());
        assert_eq!(client.streams.lock().expect("lock").active.len(), 2);
    }

    #[test]
    fn unsubscribe_removes_from_active_and_queues_command() {
        let client = stream_client(SyncClientConfig::default());
        let handle = client.sync_stream("lists", params(&[])).subscribe();
        let id = handle.id().to_string();
        handle.unsubscribe();
        let registry = client.streams.lock().expect("lock");
        assert!(registry.active.is_empty());
        assert!(
            matches!(registry.pending.last(), Some(StreamCommand::Unsubscribe { id: i }) if *i == id)
        );
    }

    #[test]
    fn drop_unsubscribes_idempotently() {
        let client = stream_client(SyncClientConfig::default());
        let handle = client.sync_stream("lists", params(&[])).subscribe();
        let id = handle.id().to_string();
        drop(handle);
        let pending = client.streams.lock().expect("lock").pending.len();
        // Exactly ONE unsubscribe queued by drop (done flag prevents a second).
        let active_empty = client.streams.lock().expect("lock").active.is_empty();
        assert!(active_empty);
        let unsubs = {
            let registry = client.streams.lock().expect("lock");
            registry
                .pending
                .iter()
                .filter(|c| matches!(c, StreamCommand::Unsubscribe { id: i } if *i == id))
                .count()
        };
        let _ = pending;
        assert_eq!(unsubs, 1);
    }

    #[test]
    fn config_declared_streams_seed_the_active_set() {
        let config = SyncClientConfig {
            streams: vec![StreamDecl::new(
                "lists",
                params(&[("owner", serde_json::json!("u1"))]),
            )],
            ..SyncClientConfig::default()
        };
        let client = stream_client(config);
        let registry = client.streams.lock().expect("lock");
        assert_eq!(registry.active.len(), 1);
        assert_eq!(registry.active["cfg-0"].0, "lists");
        // No pending entry: run_once's active re-send covers the first connect.
        assert!(registry.pending.is_empty());
    }
}
