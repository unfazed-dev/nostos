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

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

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
}

/// A session + its sink, returned by [`SessionStore::candidates_for`].
///
/// Carrying the predicate alongside lets the router evaluate filters without a
/// second lookup.
#[derive(Clone)]
pub struct SessionCandidate {
    pub id: SessionId,
    pub predicate: Predicate,
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
    /// Current live session count (gauge, not counter).
    pub sessions: AtomicUsize,
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
            sessions: self.sessions.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time read of [`Metrics`] (plain values, safe to format/serialize).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub matched: u64,
    pub delivered: u64,
    pub dropped: u64,
    pub sessions: usize,
}
