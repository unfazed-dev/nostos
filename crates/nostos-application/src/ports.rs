//! Port traits — the driven-side interfaces the infrastructure layer implements.
//!
//! These are the seams that make Nostos hexagonal: the use-cases talk to these
//! traits, not to concrete adapters. That's what lets the benchmark swap a
//! `FakeReplicator` in for the real `PgReplicator` with zero use-case changes,
//! and lets unit tests run the fan-out loop with no tokio runtime at all.
//!
//! **Async note:** the ports are `async` because the fan-out loop awaits
//! delivery. `async_trait` is used so the same trait works for both sync test
//! doubles and async adapters. The domain layer stays pure (ADR-0001); only
//! this layer sees `async`.

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

/// Slot-health gauge reported by the PgReplicator into [`Metrics`]. Rendered as
/// a Prometheus gauge int (see `as_gauge_int`). The replicator is the only
/// writer; the server's `/metrics` endpoint is the only reader.
///
/// Encoding is intentionally a plain `u8` (not the postgres `wal_status` text)
/// so the application layer never imports a Postgres type — the hexagonal
/// boundary stays clean (ADR-0009).
///
/// - `Healthy` (0): slot exists, `wal_status in ('reserved'|'extended'|'unreserved')`.
/// - `Reserved` (1): transitional / ambiguous — kept distinct for operators so a
///   flap is visible in the gauge trace.
/// - `Lost` (2): `wal_status='lost'` OR slot missing on reconnect. This is the
///   silent-data-loss signal: WAL between the last client-acked LSN and the new
///   consistent point is gone. The replicator logs `error!` and re-creates +
///   re-snapshots, but the gap is unrecoverable — the metric makes it visible.
/// - `Recreated` (3): the slot was just dropped + re-created with a fresh
///   snapshot (set transiently after recovery, before the next health probe
///   flips it back to `Healthy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotHealth {
    #[default]
    Healthy = 0,
    Reserved = 1,
    Lost = 2,
    Recreated = 3,
}

impl SlotHealth {
    /// Render as the integer Prometheus gauge value (matches the `#[repr(u8)]`).
    #[inline]
    #[must_use]
    pub fn as_gauge_int(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_gauge_int`]. Unknown discriminants collapse to
    /// `Healthy` (a future PG `wal_status` variant we don't model yet should
    /// not crash metrics rendering — `ponytail:` add a new variant when one
    /// shows up in the field).
    #[inline]
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Reserved,
            2 => Self::Lost,
            3 => Self::Recreated,
            _ => Self::Healthy,
        }
    }
}

use async_trait::async_trait;

use nostos_domain::{
    Lsn, Predicate, Principal, ReplicationEvent, SessionId, SyncSession, TenantScope,
};

/// The outcome of attempting to deliver an event to a session.
///
/// The router uses this to maintain drop/latency accounting — an honest
/// throughput number must report drops, not hide them (see
/// `BENCHMARK-METHODOLOGY.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDecision {
    /// The event was accepted by the session's sink.
    Delivered,
    /// The session's bounded buffer was full, so this event was dropped to
    /// protect the router from head-of-line blocking. Counted, not silent.
    Dropped,
}

/// A delivery target for one session — implemented by the infra layer (a tokio
/// channel per WebSocket connection) and by test doubles (a recording sink).
///
/// Implementations decide their own backpressure strategy. The production
/// `TokioEventSink` drops when its bounded channel is full; the test
/// `RecordingSink` never drops (capacity is unlimited).
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Attempt to deliver one event. Non-blocking from the router's POV —
    /// returns promptly with a [`DeliveryDecision`].
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision;

    /// The highest LSN the *client* has acknowledged applying (via an ACK
    /// frame). `None` means "this sink does not track acks" (test doubles) or
    /// "no ack received yet." Read by [`SessionStore::min_acked_lsn`] to drive
    /// the ack-driven replication-slot advance (ADR-0009).
    #[inline]
    fn last_acked_lsn(&self) -> Option<Lsn> {
        None
    }

    /// The highest LSN *delivered* into this sink's buffer (whether or not the
    /// client acked it). `None` for sinks that don't track it. Diagnostic —
    /// exposes the delivered-vs-acked lag.
    #[inline]
    fn last_delivered_lsn(&self) -> Option<Lsn> {
        None
    }
}

/// One push doorbell hint — the minimal routing tuple the router emits per
/// matched account (ADR-0037 §2/§4). Silent doorbells deliberately carry NO
/// row data: push is a wake-up trigger, never a data channel; the client's
/// durable LSN checkpoint is the correctness mechanism, so a missed or stale
/// push loses nothing.
///
/// Two extensions ride the same tuple (plan 2.4):
/// - `account_id` empty = a TENANT-WIDE hint (ADR-0037 §1 amendment): the
///   coalescer expands it to that tenant's registered tokens whose accounts
///   are offline at send time — the killed-app case the matched set cannot
///   doorbell.
/// - `payload` = the event's tuple bytes, attached ONLY when the table's push
///   config is a visible template. It exists for in-process `{column}`
///   interpolation and never reaches a provider except as the interpolated
///   title/body (the operator's visible-push opt-in, ADR-0037 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushHint {
    /// The table the matching event landed on (the per-table template /
    /// priority config key for the rails).
    pub table: String,
    /// The matched session's tenant — `""` when the session is anonymous.
    pub tenant_id: String,
    /// The matched session's account — the push-token registry lookup key.
    /// Empty for a tenant-wide hint (see the type doc).
    pub account_id: String,
    /// The event's LSN — bounds staleness in-rail and anchors the
    /// push-LSN→client-ack correlation surface.
    pub lsn: Lsn,
    /// The event's tuple bytes, present only for visible-configured tables
    /// (template interpolation input — see the type doc).
    pub payload: Option<Vec<u8>>,
}

/// Per-table push template (ADR-0037 §2/§4, plan 2.4): a content-free
/// doorbell, or a visible notification with optional single-column
/// interpolation. Static interpolation only — no expression language, no
/// scheduling; that is the marketing-platform layer nostos explicitly does
/// not build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushTemplate {
    /// Doorbell: at most `{table, lsn}` transits the provider.
    Silent,
    /// Visible notification; `{col}` placeholders in title/body interpolate
    /// the triggering event's column values at send time.
    Visible { title: String, body: String },
}

/// The compiled per-table push configuration (ADR-0037 §1 amendment + §2):
/// which tables doorbell fully-offline accounts, their templates, and the
/// tenant column used to target those hints. Parsed by the composition root
/// from `NOSTOS_PUSH_TABLES` (`nostos-server/src/main.rs`) — no env reads in
/// this layer. Default (empty) = no tenant-wide hints; the matched-account
/// path (plan 1.3) runs unchanged.
#[derive(Debug, Clone, Default)]
pub struct PushTables {
    /// The operator's tenant column (`NOSTOS_TENANT_COLUMN`): the fan-out
    /// reads its value from each event's payload (via the caller's column
    /// extractor) to stamp tenant-wide hints — the row's own tenant, so
    /// hints fire even when NO session matched. `None` degrades to the
    /// matched sessions' distinct tenants (best effort).
    pub tenant_column: Option<String>,
    /// table → template. A table absent here never triggers a tenant-wide
    /// hint (one HashMap miss is the whole hot-loop cost).
    pub tables: std::collections::HashMap<String, PushTemplate>,
}

impl PushTables {
    /// The table's template, if any.
    #[inline]
    #[must_use]
    pub fn get(&self, table: &str) -> Option<&PushTemplate> {
        self.tables.get(table)
    }
}

/// The push doorbell rail (ADR-0037 §1/§4) — the driven-side port behind FCM /
/// APNs / Web Push. Push relevance is derived from the same predicate pass
/// that feeds WS fan-out; there is no parallel push-subscription registry.
///
/// `notify` is called **once per matched-account signal** — not per event, not
/// per session: the router collapses an event's matching sessions to one hint
/// per account, and the rail's own coalescer further debounces bursts
/// in-rail (digest window).
///
/// # Non-blocking contract
///
/// Same shape as [`OpLogWriter`]: the caller is the fan-out loop's matched-set
/// drain, so an implementation MUST return promptly — enqueue internally
/// (bounded channel, drop-on-full with a counter) and let a background task do
/// the rail I/O. The 833k ops/sec hot loop must not gain a network
/// round-trip. `()` return: delivery accounting is the implementation's,
/// surfaced via [`Metrics`].
#[async_trait]
pub trait PushNotifier: Send + Sync {
    /// Fire one doorbell hint. Fire-and-forget.
    async fn notify(&self, hint: PushHint);
}

/// The default push rail: sends nothing. Push stays entirely off — no token
/// registry, no rails — when the composition root has nothing to wire (no
/// rail env configured and no `NOSTOS_PUSH_TABLES`), so the bench baseline
/// and fake-mode deploys pay nothing. The real consumer is the `PushRouter`
/// coalescer (infra, plan 2.4).
pub struct NoopNotifier;

#[async_trait]
impl PushNotifier for NoopNotifier {
    async fn notify(&self, _hint: PushHint) {}
}

/// Why an atomic add was rejected by [`SessionStore::try_add_below_cap`].
///
/// Surfacing this from the store (rather than checking `len` then `add` in the
/// caller) is what closes the check-then-act race: the count and the insert
/// happen under one critical section in the store, so concurrent connects can't
/// each read a stale count and overshoot the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreRejection {
    /// Accepting the session would exceed `cap`. The caller (SessionManager)
    /// maps this to [`ConnectError::DeviceCapReached`].
    #[error("concurrent device cap reached ({cap})")]
    CapExceeded { cap: u64 },
}

/// A live set of sync sessions, indexed for fast predicate evaluation.
///
/// The contract is intentionally minimal: add/remove sessions, and — the hot
/// path — find the candidate sessions whose predicate *might* match an event.
/// `candidates_for` is expected to prune aggressively (by `predicate.table` at
/// minimum) so the router evaluates filters against a small candidate set.
//
// `len` has no companion `is_empty`: every caller compares against a cap
// (`SessionManager::connect`) or echoes the count for metrics, so an
// `is_empty` would be unused. Allow the lint rather than carry dead API.
#[allow(clippy::len_without_is_empty)]
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Register a session with its delivery sink. The store indexes it by
    /// `predicate.table`.
    ///
    /// Prefer [`Self::try_add_below_cap`] when the caller is enforcing a
    /// concurrent-device cap — that method is atomic, this one is not.
    async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>);

    /// Atomically insert a session *only if* the live count is below `cap`.
    ///
    /// The count-check and the insert happen under one critical section, so
    /// concurrent connects cannot each read a stale count and overshoot the cap
    /// (the TOCTOU that the separate `len().await` + `add().await` sequence has).
    /// Returns the inserted session's id, or `CapExceeded` if the store is full.
    ///
    /// `cap = u64::MAX` means "no cap" (the unlimited / Enterprise path).
    async fn try_add_below_cap(
        &self,
        session: SyncSession,
        sink: Arc<dyn EventSink>,
        cap: u64,
    ) -> Result<SessionId, StoreRejection>;

    /// Remove a session by id (connection closed / dropped).
    async fn remove(&self, id: SessionId);

    /// Return the sessions whose `predicate.table` matches `event`'s table,
    /// paired with their sinks. The router then runs `Predicate::matches` on
    /// each to decide delivery.
    ///
    /// Implementations should index by table for O(1) pruning. Returning all
    /// sessions on every event is a correctness-preserving but slow fallback.
    async fn candidates_for(&self, event: &ReplicationEvent) -> Vec<SessionCandidate>;

    /// Total number of live sessions (for metrics / dashboards).
    async fn len(&self) -> usize;

    /// The minimum `last_acked_lsn` across all live sessions, or `None` when no
    /// session has acknowledged anything yet (or the store is empty).
    ///
    /// This is the safe-to-flush LSN: Postgres's replication slot must not
    /// advance past it, or a reconnect would skip events the slowest client
    /// never confirmed (silent data loss). See ADR-0009.
    async fn min_acked_lsn(&self) -> Option<Lsn>;

    /// The `(SessionId, Lsn)` of the live session with the smallest acked LSN
    /// — the slowest consumer. Used by the WAL-bloat eviction policy
    /// ([`crate::EvictionPolicy`]) to target the single session holding back
    /// the slot. Returns `None` if no session has acked (or the store is empty).
    ///
    /// Default: derived from a full scan (implementations with an index may
    /// override). Never panics.
    async fn slowest_session(&self) -> Option<(SessionId, Lsn)> {
        None
    }

    /// Presence (ADR-0037 §4): does `account_id` have at least one live
    /// session in the store? The push router suppresses the doorbell for
    /// online accounts — an online client gets the data over its socket, so a
    /// push would only double-signal it.
    ///
    /// "Online" means **store membership** — never socket liveness (an evicted
    /// session's zombie sink may still be alive in the transport, but it is no
    /// longer in the store and must count as offline) and never the sink's
    /// [`DeliveryDecision`] (a `Dropped`-but-registered slow client is still
    /// online: its socket is draining, and pushing it would double-signal a
    /// client that is catching up over the socket).
    ///
    /// Default: `false` (treat as unknown/offline → push). The failure
    /// direction is deliberate: a spurious push is a harmless nudge (sync
    /// reconciles; doorbell semantics), while a swallowed push hides a change.
    /// Implementations with an account index override this.
    async fn account_online(&self, _account_id: &str) -> bool {
        false
    }
}

/// A session + its sink, returned by [`SessionStore::candidates_for`].
///
/// Carrying the predicate alongside lets the router evaluate filters without a
/// second lookup; carrying the principal lets the push path (ADR-0037) derive
/// the `(tenant, account)` of a matched session without a second store lookup.
#[derive(Clone)]
pub struct SessionCandidate {
    pub id: SessionId,
    pub predicate: Predicate,
    /// The session's authenticated identity, or `None` for the anonymous /
    /// unauthenticated path. Threaded from [`SyncSession`] by the store —
    /// before ADR-0037 the store discarded it here.
    pub principal: Option<Principal>,
    pub sink: Arc<dyn EventSink>,
}

/// Source of replication events — the driven-side port a replicator implements.
///
/// The production `PgReplicator` reads Postgres logical replication (WAL →
/// `pgoutput`); the benchmark `FakeReplicator` generates synthetic events. Both
/// implement this trait, so the fan-out loop is identical.
#[async_trait]
pub trait ReplicatorStream: Send {
    /// Block until the next replication event is available, or return `None`
    /// when the stream is permanently exhausted (clean shutdown).
    async fn next_event(&mut self) -> Option<ReplicationEvent>;

    /// Advance the source's durable-progress cursor to `lsn`, declaring that
    /// all events up to (and including) `lsn` have been acknowledged by every
    /// live consumer. The `PgReplicator` forwards this to Postgres's
    /// `confirmed_flush_lsn` (ack-driven slot advance, ADR-0009); the
    /// `FakeReplicator` no-ops (no slot to advance). Default: no-op, so test
    /// doubles and the fake don't have to implement it.
    #[inline]
    async fn advance_progress(&mut self, _lsn: Lsn) {}
}

/// Authenticates a `/sync` connection's bearer token into a [`Principal`].
///
/// The transport calls this BEFORE upgrading the WebSocket: a `None` result
/// means reject (HTTP 401, no upgrade); `Some(principal)` flows into the
/// session so the server can enforce the predicate against it (ADR-0010,
/// ADR-0011). Implementations:
/// - `AllowAnonymous` (infra) — mints [`Principal::anonymous`] for every
///   connection; the OSS self-host dev default (`NOSTOS_SYNC_AUTH=none`).
/// - `SupabaseJwtAuth` (infra) — HS256-verifies a Supabase JWT and lifts `sub`.
///
/// Defined here (not in nostos-cloud) so `nostos-server` can authenticate without
/// depending on the control-plane crate — `nostos-cloud`'s `JwtVerifier` lives
/// behind an HTTP cookie/bearer path that the WS transport doesn't share.
#[async_trait]
pub trait SyncAuth: Send + Sync {
    /// Resolve a bearer token to a principal, or `None` if unauthenticated.
    async fn authenticate(&self, token: &str) -> Option<Principal>;
}

/// Applies client-submitted writes to the source database (ADR-0013 v1).
///
/// This is the driven-side port that turns Nostos's read-only sync socket into
/// a bidirectional one: a client sends a `Write` frame, the transport hands it
/// here, and the adapter upserts/deletes the row in the *source* Postgres. The
/// resulting row change then flows back out through the normal replication
/// path (`ReplicatorStream` → `FanOutService`) — so a write is confirmed to
/// the writer AND fanned out to every subscriber, including the writer itself
/// (where the idempotent apply is a no-op). LWW by WAL order.
///
/// Implementations:
/// - `PgWriteBack` (infra, feature `pg`) — the real adapter: identifier
///   validation + table allowlist + parameterized SQL against the source PG.
/// - `NoWriteBack` (infra) — the fake-mode stub: returns `Backend` for every
///   call (the FakeReplicator has no database to write to).
/// - test doubles — record calls for unit tests.
///
/// **Trust boundary:** every implementation MUST validate the `table` against
/// an allowlist and the identifiers against a strict regex BEFORE any SQL is
/// built, and MUST bind values as parameters (`$1…$n`) — never string-interpolate
/// them. Authenticated clients can still attempt injection; the allowlist +
/// regex + parameters are defense-in-depth. See ADR-0013.
///
/// **Tenant scoping (ADR-0018):** the `tenant` parameter, when `Some`, is the
/// server-computed [`TenantScope`] for this write — never the client's own
/// claim (the transport derives it from the authenticated [`Principal`] via
/// [`Principal::tenant_scope`], the same seam the read path uses). An
/// implementation that honors it MUST: (1) force-stamp the tenant column in
/// an upserted payload to `tenant.value`, overwriting any client-supplied
/// value; (2) refuse to let an upsert change ownership of a row that already
/// belongs to a different tenant (reject, don't silently no-op — see
/// `PgWriteBack`); (3) scope a delete to `tenant.value` so a client can never
/// delete another tenant's row; (4) scope a patch to `tenant.value` AND
/// force-stamp the tenant column in the patched payload, so a client can
/// neither target another tenant's row nor mutate its own row's tenant
/// ownership. `None` means no tenant enforcement is active
/// (anonymous/single-tenant deploys) — behavior is unchanged from pre-ADR-0018.
#[async_trait]
pub trait WriteBack: Send + Sync {
    /// Upsert one row: `payload_json` is a JSON object of column → value, the
    /// same tuple-image shape the read path delivers. LWW by WAL order. The
    /// `pk` is the row's primary-key value (v1 convention: pk column is `id`).
    ///
    /// # Errors
    /// - [`WriteBackError::TableNotAllowed`] if `table` is not in the allowlist.
    /// - [`WriteBackError::InvalidPayload`] if `payload_json` is not a JSON
    ///   object, or contains a column name that fails identifier validation.
    /// - [`WriteBackError::Forbidden`] if `tenant` is `Some` and the target row
    ///   already exists under a different tenant (ADR-0018).
    /// - [`WriteBackError::Backend`] for any underlying database error.
    async fn upsert(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError>;

    /// Delete by primary key, scoped to `tenant` when `Some` (ADR-0018). A
    /// missing row — or, when tenant-scoped, a row that belongs to a
    /// different tenant and therefore cannot be observed to exist — is a
    /// success (idempotent), so a redelivery of a delete after the row is
    /// already gone does not surface an error to the client. A row that DOES
    /// exist but under a different tenant is a [`WriteBackError::Forbidden`]
    /// rejection, not a silent no-op (see `PgWriteBack`'s doc for the
    /// existence-disclosure trade-off this implies).
    ///
    /// # Errors
    /// - [`WriteBackError::TableNotAllowed`] if `table` is not in the allowlist.
    /// - [`WriteBackError::Forbidden`] if `tenant` is `Some` and the row exists
    ///   under a different tenant (ADR-0018).
    /// - [`WriteBackError::Backend`] for any underlying database error.
    async fn delete(
        &self,
        table: &str,
        pk: &str,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError>;

    /// Patch (column-level UPDATE) one existing row: `payload_json` is a JSON
    /// object of only the columns to change. Columns absent from the payload
    /// are left untouched — unlike an upsert, a patch NEVER inserts. Matches
    /// PowerSync's PATCH op-type (P3 parity). The `pk` identifies the row
    /// (v1 convention: pk column is `id`).
    ///
    /// A patch of a row that does not exist is a success (idempotent) — so a
    /// redelivered patch after the row is gone does not surface an error. A
    /// row that DOES exist but under a different tenant is a
    /// [`WriteBackError::Forbidden`] rejection, not a silent no-op (same
    /// existence-disclosure trade-off as [`Self::delete`]). "Deletes always
    /// win": a delete that races ahead of a patch on the same pk makes the
    /// patch's 0-rows-affected result an idempotent success.
    ///
    /// # Errors
    /// - [`WriteBackError::TableNotAllowed`] if `table` is not in the allowlist.
    /// - [`WriteBackError::InvalidPayload`] if `payload_json` is not a JSON
    ///   object, contains a column name that fails identifier validation, or
    ///   carries no columns to set.
    /// - [`WriteBackError::Forbidden`] if `tenant` is `Some` and the row exists
    ///   under a different tenant (ADR-0018).
    /// - [`WriteBackError::Backend`] for any underlying database error.
    async fn patch(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError>;

    /// Atomic increment of one numeric column on an existing row (ADR-0030
    /// Decision 1): `payload_json` is `{"field":"<col>","delta":<i64>}`.
    /// `PgWriteBack` emits `UPDATE … SET <field> = <field> + $delta WHERE
    /// id=$pk`, so Postgres serializes concurrent increments — no client
    /// read-modify-write, no lost update. The `pk` identifies the row (v1
    /// convention: pk column is `id`).
    ///
    /// A increment of a row that does not exist is a success (idempotent —
    /// 0 rows affected); a row that exists under a different tenant is a
    /// [`WriteBackError::Forbidden`] rejection, never a silent no-op (same
    /// existence-disclosure trade-off as [`Self::patch`]). The field may not
    /// be the primary-key column or, when tenant-scoped, the tenant column.
    ///
    /// # Errors
    /// - [`WriteBackError::TableNotAllowed`] if `table` is not in the allowlist.
    /// - [`WriteBackError::InvalidPayload`] if `payload_json` is not a JSON
    ///   object, lacks `field`/`delta`, the field fails identifier validation,
    ///   or `delta` is not an integer.
    /// - [`WriteBackError::Forbidden`] if `tenant` is `Some` and the row exists
    ///   under a different tenant (ADR-0018).
    /// - [`WriteBackError::Backend`] for any underlying database error.
    async fn increment(
        &self,
        table: &str,
        pk: &str,
        payload_json: &str,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<(), WriteBackError>;
}

/// Why a [`WriteBack`] call failed. Surfaced to the client as the `error`
/// string in a `WriteResult{ok:false}` frame.
///
/// The variants map 1:1 to the three things that can go wrong: the table
/// isn't writable (allowlist), the payload is malformed (not an object / bad
/// column name), or the database itself errored. The error strings are
/// user-facing (they go on the wire), so they carry no internal detail beyond
/// the category — `Backend` wraps the underlying message but the adapter is
/// responsible for not leaking connection strings.
#[derive(Debug, thiserror::Error)]
pub enum WriteBackError {
    /// The table is not in the `NOSTOS_WRITE_TABLES` allowlist. The allowlist
    /// is the first line of defense: a table not explicitly writable can never
    /// reach the SQL builder, so its name can never be interpolated.
    #[error("table not writable: {0}")]
    TableNotAllowed(String),
    /// The payload JSON is not a JSON object, or one of its keys fails the
    /// identifier regex (`^[a-z_][a-z0-9_]*$`). Both are client-controlled, so
    /// both are validated before any SQL is constructed.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    /// A tenant-scoped write (ADR-0018) targeted a row that exists under a
    /// DIFFERENT tenant than the authenticated principal's. The write is
    /// rejected outright — never silently applied and never silently
    /// dropped — so the client learns its write did not take effect.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The underlying database errored (connection, syntax the validator
    /// should have caught, constraint violation, …). The wrapped string is the
    /// backend's message; adapters MUST scrub it of secrets before returning.
    #[error("backend: {0}")]
    Backend(String),
}

/// Persists replication events to a durable op-log so a reconnecting client
/// can replay missed ops (including DELETEs) from its checkpoint instead of
/// re-snapshotting — the in-window resume path (ADR-0025 slice 2+).
/// Snapshot-reconcile (slice 1) remains the fallback for long gaps /
/// first-connect / epoch mismatch.
///
/// # Non-blocking contract (LOAD-BEARING)
///
/// `append` MUST return promptly without performing inline I/O on the caller's
/// path. The caller is the `FanOutService::run` loop, where at the 833k
/// ops/sec headline the per-event budget is ~1.2µs; a Postgres round-trip is
/// ~0.5–2ms — inline I/O would stall the loop, starve the bounded session
/// sinks, and flip deliveries from `Delivered` to `Dropped` (silently breaking
/// the 0% drop headline). An implementation batches internally and flushes off
/// the caller's path (e.g. a background task with its own bounded queue).
///
/// A full internal buffer drops the entry (best-effort: snapshot-reconcile
/// preserves correctness for the affected gap) and counts it via the impl's
/// own metrics — `append` returns `()` because the fan-out loop does not act
/// on op-log drop decisions; it is fire-and-forget.
#[async_trait]
pub trait OpLogWriter: Send + Sync {
    /// Append one event to the durable op-log. Non-blocking — see the trait
    /// doc for the contract. `()` return: drop/flush-failure accounting is
    /// internal, surfaced via [`Metrics`] (`oplog_dropped` / `oplog_flush_failed`).
    async fn append(&self, event: &ReplicationEvent);

    /// Graceful shutdown: drain the in-flight batch + any pending channel
    /// entries, then end the flush task. Call once during server shutdown
    /// (after the replicator/fan-out stop) so a SIGTERM doesn't drop the last
    /// ≤BATCH_MAX entries mid-INSERT (ADR-0025 slice-6 follow-up; correctness
    /// is still covered by slice-1 reconcile if this isn't called). Default
    /// no-op for backends with nothing to drain.
    async fn shutdown(&self) {}
}

/// Reads a table's current rows as a one-shot snapshot, delivered to a
/// freshly-subscribing session as `Insert` events BEFORE live fan-out — so a
/// client that connects to an already-populated table sees pre-existing rows
/// immediately (PowerSync parity), not nothing-until-the-first-mutation.
///
/// The transport calls this once per subscribe (after registering the session,
/// before spawning the writer task) and delivers each returned event to THAT
/// session's sink only. A failed snapshot is non-fatal: the transport logs it
/// and continues with live fan-out, so the client still receives subsequent
/// mutations (it just may not see rows that already existed).
///
/// `base_lsn` is the LSN floor the snapshot events MUST exceed. The caller
/// passes the session's seeded acked LSN (from `subscribe.resume_lsn`, or 0 for
/// a fresh client) so the per-session sink's LSN gate
/// (`TokioEventSink::deliver` drops events with `lsn <= acked_lsn` when
/// `acked != 0`, plus a dedup ring that drops exact LSN duplicates) does NOT
/// swallow the snapshot. Implementations MUST stamp each returned event with a
/// UNIQUE LSN strictly greater than `base_lsn`.
///
/// # Trust boundary
/// `table` is CLIENT-CONTROLLED (it arrives in the subscribe frame).
/// Implementations MUST validate it against a strict identifier regex BEFORE
/// any SQL is built, and MUST only ever interpolate it as a quoted identifier —
/// same discipline as [`WriteBack`]. Snapshot reads values (never writes them),
/// so there is no value-binding injection surface here; the table name is the
/// only client-controlled string that reaches SQL.
///
/// ponytail: no tenant-predicate scoping in v1 — anonymous / single-tenant
/// only. The snapshot SELECT is unfiltered, so a multi-tenant deploy must NOT
/// wire a `SnapshotSource` until this is upgraded to take the server-injected
/// [`TenantScope`] (mirrors the read-path predicate injection — ADR-0011). The
/// upgrade is: pass `Option<TenantScope>` through `snapshot`, append a
/// `WHERE "<tenant_col>" = $1` clause, bind the principal's tenant value.
#[async_trait]
pub trait SnapshotSource: Send + Sync {
    /// Read every row of `table` as an `Insert` event, each stamped with a
    /// unique LSN strictly greater than `base_lsn`. The payload of each event
    /// MUST match the streaming path's tuple-image shape (ADR-0019) so the
    /// client's idempotent apply (`upsert by pk`) treats a snapshot row exactly
    /// like a streamed insert.
    ///
    /// # Errors
    /// - [`SnapshotError::InvalidTable`] if `table` fails the identifier regex.
    /// - [`SnapshotError::Backend`] for any underlying database error
    ///   (connection, prepare, query). The transport logs and continues.
    async fn snapshot(
        &self,
        table: &str,
        base_lsn: Lsn,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError>;
}

/// Why a [`SnapshotSource::snapshot`] call failed. Surfaced to the transport,
/// which logs the error and continues with live fan-out (a failed snapshot is
/// NOT fatal — the client still receives subsequent mutations). Variants mirror
/// [`WriteBackError`]'s categories: the table name was invalid, or the database
/// errored. There is no payload/validation variant because snapshot reads
/// values (never writes them) — there is no client-supplied payload to reject.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The table name failed the strict identifier regex
    /// (`^[a-z_][a-z0-9_]*$`). The name is client-controlled (from the
    /// subscribe frame), so it is validated before any SQL is built.
    #[error("invalid snapshot table identifier: {0}")]
    InvalidTable(String),
    /// The underlying database errored (connection, prepare, query). The
    /// wrapped string is the backend's message; adapters MUST scrub it of
    /// secrets (connection strings) before returning — same discipline as
    /// [`WriteBackError::Backend`].
    #[error("snapshot backend: {0}")]
    Backend(String),
}

// ---------------------------------------------------------------------------
// Op-log replay (ADR-0025 slice 4b — reconnect resume without a full snapshot).
// ---------------------------------------------------------------------------

/// Why an [`OpLogSource`] call failed. The op-log read is a single tenant-scoped
/// SELECT on the server-internal `cairn_oplog` table, so there is no
/// invalid-table category (mirror of [`SnapshotError`] minus `InvalidTable`).
#[derive(Debug, thiserror::Error)]
pub enum OpLogError {
    /// The underlying database errored (connection, prepare, query). The
    /// transport logs it and falls back to the snapshot path — replay is an
    /// optimization; snapshot-reconcile (slice 1) is the correctness floor.
    #[error("oplog backend: {0}")]
    Backend(String),
}

/// Replay the persisted op-log for reconnect resume (ADR-0025 slice 4b).
///
/// On subscribe, when `client_epoch == server_epoch` and the client's
/// `resume_lsn` is within the retained op-log window, the transport replays the
/// missed ops (INSERTs/UPDATEs/DELETEs from the offline gap) to the fresh sink
/// — catching up WITHOUT a full snapshot. The client dedups per-row by lsn
/// (ADR-0025 slice 4a), so the concurrent live fan-out + replay overlap is safe.
/// Slice-1 snapshot-reconcile remains the safety net for the batch-lag tail and
/// gaps that aged out past the retention window.
#[async_trait]
pub trait OpLogSource: Send + Sync {
    /// Replay op-log entries for `tenant_id` with `lsn > after_lsn`, in lsn
    /// order. Upserts reconstitute as `Insert` RowOps — under the client's
    /// per-row lsn gate (slice 4a), Insert ≡ Update (the client applies by pk).
    ///
    /// # Errors
    /// [`OpLogError::Backend`] on any database failure.
    async fn replay_after(
        &self,
        tenant_id: &str,
        after_lsn: u64,
    ) -> Result<Vec<ReplicationEvent>, OpLogError>;

    /// The lowest `lsn` still retained in the op-log (the window tail, advanced
    /// by compaction — ADR-0025 slice 5). `resume_lsn < tail` ⇒ the client's
    /// offline gap aged out ⇒ the transport falls back to the snapshot path.
    ///
    /// # Errors
    /// [`OpLogError::Backend`] on any database failure.
    async fn window_tail(&self) -> Result<u64, OpLogError>;
}

// ---------------------------------------------------------------------------
// Schema discovery (WS1 — Flutter PowerSync-style redesign, Option-C).
// ---------------------------------------------------------------------------

/// One column in a synced table's schema, reported by [`SchemaSource`] so the
/// client can auto-build its typed tables. Transport DTO, not a domain
/// invariant (it exists to be serialized to the client) — lives here next to
/// the port, mirroring [`SnapshotError`]'s placement.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaColumn {
    /// Column name (`pg_attribute.attname`).
    pub name: String,
    /// Postgres type OID (`atttypid`), as `i32` to match nostos's wire
    /// convention (OIDs are always < 2^31; see `snapshot_source.rs`).
    pub pg_oid: i32,
    /// SQLite column affinity (`"TEXT"` | `"INTEGER"` | `"REAL"`) for the
    /// client's typed table. Mirrors the JSON token shape nostos emits so the
    /// wire value stores without coercion.
    pub affinity: String,
}

/// One synced table's schema.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaTable {
    /// nostos's canonical table identifier — bare name for `public` (e.g.
    /// `tasks`), else `schema.name`. Matches subscribe-frame / `WireFrame`
    /// `table` so the client keys its typed table correctly.
    pub name: String,
    /// Real primary-key column names (`pg_index.indisprimary`), NOT a hardcoded
    /// `"id"`. May be empty if the table has no replica identity.
    pub primary_key: Vec<String>,
    pub columns: Vec<SchemaColumn>,
}

/// The full schema of a publication, returned by [`SchemaSource::fetch`] and
/// served by nostos-server's `GET /schema`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaDescriptor {
    /// The publication name (`PgReplicatorConfig::publication`).
    pub publication: String,
    pub tables: Vec<SchemaTable>,
}

/// Why a [`SchemaSource::fetch`] call failed. Mirrors [`SnapshotError`] minus
/// the `InvalidTable` variant — there is no client-controlled table name on
/// this path (the publication name is server config, not a frame), so there is
/// no identifier to validate.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The underlying database errored (connection, catalog query). Adapters
    /// MUST scrub the wrapped string of secrets (connection strings) — same
    /// discipline as [`SnapshotError::Backend`].
    #[error("schema backend: {0}")]
    Backend(String),
}

/// Read the publication's typed schema (tables/columns/SQLite-affinity) so a
/// client can auto-build its typed tables (WS1). The Flutter SDK's default is
/// to fetch this on connect rather than hand-write a `Schema` (the headline DX
/// win over PowerSync).
///
/// The schema-side sibling of [`SnapshotSource`]: same port/adapter shape,
/// backed by `PgSchemaSource` under `NOSTOS_REPLICATOR=pg`. ponytail: no tenant
/// scoping in v1 — the schema is publication-wide metadata (the SET of synced
/// tables), not tenant-specific rows; row isolation is the read-path
/// predicate's job (ADR-0011/0018). `GET /schema` is v1-unauthenticated for the
/// same reason — v2: add auth at the route layer if a managed deploy wants it.
#[async_trait]
pub trait SchemaSource: Send + Sync {
    /// Fetch the full publication schema.
    ///
    /// # Errors
    /// [`SchemaError::Backend`] for any underlying database error (connection,
    /// catalog query). The server logs and returns 503.
    async fn fetch(&self) -> Result<SchemaDescriptor, SchemaError>;
}

/// Estimated size of one replicated table, for the `all`-mode startup warning
/// (ADR-0031: an operator on `sync_mode = "all"` with a huge table gets no
/// scoping — warn at boot rather than let them find out from an OOM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStat {
    pub table: String,
    /// `None` when the source cannot estimate (e.g. Postgres `reltuples = -1`
    /// on a never-analyzed table). Render as "unknown", never as a number.
    pub estimated_rows: Option<u64>,
}

/// Row-count estimates for the synced tables, cheap enough to call at boot.
/// Reuses [`SchemaError`] — same backend, same failure shape as
/// [`SchemaSource`].
#[async_trait]
pub trait TableStatsSource: Send + Sync {
    /// # Errors
    /// [`SchemaError::Backend`] for any underlying database error.
    async fn table_stats(&self) -> Result<Vec<TableStat>, SchemaError>;
}

/// Aggregate throughput/accounting counters, updated by the fan-out loop and
/// read by the `/metrics` endpoint. Lock-free (atomics); rendered to
/// Prometheus text by the server.
///
/// Kept here (not infra) so the application's `FanOutService` owns the updates
/// and the server merely reads — the metrics reflect what the use-case did.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Events whose predicate matched at least one session.
    pub matched: AtomicU64,
    /// Events accepted by a session sink.
    pub delivered: AtomicU64,
    /// Events dropped (full buffer / closed sink / dedup hit).
    pub dropped: AtomicU64,
    /// Delivery tasks that *faulted* (panicked / were cancelled) — a server-side
    /// problem, NOT slow-client backpressure. Counted separately from `dropped`
    /// so a panic is never mis-attributed as a client drop in the "0% drops"
    /// moat figure or in `/metrics`. See `FanOutService::fan_out`.
    pub faulted: AtomicU64,
    /// Current live session count (gauge, not counter).
    pub sessions: AtomicUsize,
    /// Replication-slot health gauge (see [`SlotHealth`]). Set by `PgReplicator`
    /// from `pg_replication_slots.wal_status` on every (re)connect. `Lost` is
    /// the operator-actionable signal that nostos silently dropped WAL while
    /// offline — ADR-0009.
    pub slot_wal_status: AtomicU8,
    /// Current WAL lsn − slot `restart_lsn` (bytes). 0 when unknown / slot
    /// missing. Gauge of how much WAL PG is retaining for the slot.
    pub replication_lag_bytes: AtomicU64,
    /// Monotonic counter: number of times the slot was dropped + re-created
    /// from a missing/lost state. Each increment implies a potential silent
    /// data-loss window — alert on any increase.
    pub slot_recreated_total: AtomicU64,
    /// Op-log entries dropped because the writer's bounded buffer was full
    /// (ADR-0025 slice 2). A non-zero value means the in-window resume path
    /// degraded to snapshot-reconcile for some gaps — correct, but a capacity
    /// signal. Alert on sustained increase.
    pub oplog_dropped: AtomicU64,
    /// Op-log batch flushes that failed (PG error / connection lost). The
    /// batch's entries are lost to the op-log → affected resume gaps fall back
    /// to snapshot-reconcile. Correctness preserved; alert on any increase.
    pub oplog_flush_failed: AtomicU64,
    /// Monotonic epoch counter bumped on every logical-replication slot
    /// (re)creation (the single chute in `ensure_slot_and_publication`'s
    /// fresh-create block). The reconnect-resume gate compares the client's
    /// last-seen epoch to this: a mismatch means the slot's lineage broke
    /// (drop+recreate) and the client cannot backfill from the dead op-stream
    /// → must full-snapshot. ADR-0025 slice 3 (signal) / slice 4 (gate).
    pub slot_epoch: AtomicU64,
    /// Rows swept by op-log compaction (ADR-0025 slice 5): collapse duplicates
    /// to the latest op per `(table_name, pk)` + age out rows past the
    /// retention window. Monotonic; a sustained increase is the expected
    /// steady state under write load, not an alert — but a flat-zero under
    /// load means the compactor isn't running.
    pub oplog_compacted_rows: AtomicU64,
    /// Push doorbell hints enqueued into the bounded channel after the
    /// matched-set drain (ADR-0037 §4, plan 1.3) — one per matched OFFLINE
    /// account per event; online accounts are suppressed at enqueue time.
    pub push_enqueued: AtomicU64,
    /// Push hints dropped because the bounded channel was full (or its
    /// consumer gone). Doorbell semantics: a dropped hint loses nothing —
    /// the client's durable LSN checkpoint is the correctness mechanism.
    /// Alert on sustained increase (the coalescer, plan 2.4, can't keep up).
    /// Also bumped by the coalescer's own inbound channel when it is full
    /// (same failure class: a hint dropped before send).
    pub push_dropped: AtomicU64,
    /// Push sends the rails accepted (2xx) — plan 3.2. Last-mile delivery
    /// stays best-effort; the client's LSN ack is the proof.
    pub push_sent: AtomicU64,
    /// Push sends that failed terminally or exhausted their retry (rail
    /// `Fatal`/`TransientRetryable`) — plan 3.2.
    pub push_failed: AtomicU64,
    /// Token rows pruned after a rail reported the target gone (APNs 410 /
    /// FCM `UNREGISTERED` / Web Push 404-410) or an owner deregistered —
    /// plan 3.2.
    pub push_pruned: AtomicU64,
    /// Highest doorbell LSN pushed per account (ADR-0037 §5 delivery
    /// observability — the push-LSN→client-ack correlation surface). Keyed
    /// by account id so `/metrics` can render it next to session acked-LSN.
    ///
    /// ponytail: unbounded map — one entry per pushed account, so the
    /// ceiling is the distinct-account count; bound with an LRU if a
    /// deploy ever shows unbounded account churn.
    pub push_last_lsn: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
}

impl Metrics {
    /// Construct a zeroed metrics handle.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot all counters as a plain struct (for `/metrics` rendering).
    #[inline]
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            matched: self.matched.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            faulted: self.faulted.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            slot_wal_status: SlotHealth::from_u8(self.slot_wal_status.load(Ordering::Relaxed)),
            replication_lag_bytes: self.replication_lag_bytes.load(Ordering::Relaxed),
            slot_recreated_total: self.slot_recreated_total.load(Ordering::Relaxed),
            oplog_dropped: self.oplog_dropped.load(Ordering::Relaxed),
            oplog_flush_failed: self.oplog_flush_failed.load(Ordering::Relaxed),
            slot_epoch: self.slot_epoch.load(Ordering::Relaxed),
            oplog_compacted_rows: self.oplog_compacted_rows.load(Ordering::Relaxed),
            push_enqueued: self.push_enqueued.load(Ordering::Relaxed),
            push_dropped: self.push_dropped.load(Ordering::Relaxed),
            push_sent: self.push_sent.load(Ordering::Relaxed),
            push_failed: self.push_failed.load(Ordering::Relaxed),
            push_pruned: self.push_pruned.load(Ordering::Relaxed),
        }
    }

    /// Set the slot-health gauge. Called by `PgReplicator` from the slot probe.
    #[inline]
    pub fn set_slot_health(&self, health: SlotHealth) {
        self.slot_wal_status
            .store(health.as_gauge_int(), Ordering::Relaxed);
    }

    /// Set the WAL-lag gauge (bytes). 0 means "unknown".
    #[inline]
    pub fn set_replication_lag(&self, lag_bytes: u64) {
        self.replication_lag_bytes
            .store(lag_bytes, Ordering::Relaxed);
    }

    /// Increment the slot-recreated counter. Called once per drop+recreate
    /// recovery. Each bump is a potential silent-data-loss window.
    #[inline]
    pub fn record_slot_recreate(&self) {
        self.slot_recreated_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the slot-epoch counter. Called once per slot (re)creation — the
    /// single chute in `ensure_slot_and_publication`'s fresh-create block
    /// (covers both first-create and Lost-recreate). Each bump starts a new
    /// slot lineage; any in-flight client whose last-seen epoch predates it
    /// must full-snapshot on reconnect (cannot backfill from a dead lineage).
    /// ADR-0025 slice 3.
    #[inline]
    pub fn record_slot_epoch_bump(&self) {
        self.slot_epoch.fetch_add(1, Ordering::Relaxed);
    }

    /// Add `n` rows swept by op-log compaction (ADR-0025 slice 5). Called once
    /// per compaction tick with the count of rows collapsed + aged out.
    #[inline]
    pub fn record_oplog_compacted(&self, n: u64) {
        self.oplog_compacted_rows.fetch_add(n, Ordering::Relaxed);
    }

    /// Record the highest LSN doorbelled to one account (plan 3.2 — the
    /// push-LSN→client-ack correlation surface). Called by the push router
    /// at send time; monotonically overwritten, never cleared.
    #[inline]
    pub fn record_push_lsn(&self, account_id: &str, lsn: u64) {
        if let Ok(mut map) = self.push_last_lsn.lock() {
            map.insert(account_id.to_string(), lsn);
        }
    }
}

/// A point-in-time read of [`Metrics`] (plain values, safe to format/serialize).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub matched: u64,
    pub delivered: u64,
    pub dropped: u64,
    /// Delivery tasks that faulted (panicked/cancelled) — distinct from
    /// `dropped` (slow-client backpressure). See [`Metrics::faulted`].
    pub faulted: u64,
    pub sessions: usize,
    pub slot_wal_status: SlotHealth,
    pub replication_lag_bytes: u64,
    pub slot_recreated_total: u64,
    pub oplog_dropped: u64,
    pub oplog_flush_failed: u64,
    /// Current slot epoch (bumped on every slot create/recreate). ADR-0025.
    pub slot_epoch: u64,
    /// Rows swept by op-log compaction (ADR-0025 slice 5).
    pub oplog_compacted_rows: u64,
    /// Push hints enqueued / dropped at the fan-out enqueue site
    /// (ADR-0037 §4, plan 1.3). See [`Metrics::push_enqueued`].
    pub push_enqueued: u64,
    pub push_dropped: u64,
    /// Push sends accepted / failed, and token rows pruned (plan 3.2).
    pub push_sent: u64,
    pub push_failed: u64,
    pub push_pruned: u64,
}
