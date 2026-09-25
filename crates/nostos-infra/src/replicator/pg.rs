//! `PgReplicator` — real Postgres logical replication via the wire protocol.
//!
//! This is the adapter that turns Nostos's Week-1 synthetic benchmark into a
//! *real* sync engine: it consumes a live Postgres logical-replication stream
//! (`pgoutput` over the replication wire protocol) and emits the same
//! [`ReplicationEvent`]s the `FakeReplicator` does — so the downstream
//! `FanOutService` and transports are unchanged (that's the hexagonal payoff).
//!
//! ## Pipeline
//!
//! ```text
//!   pgwire_replication::ReplicationClient::recv()
//!        │  ReplicationEvent::XLogData { wal_end, data, .. }
//!        │  (data = raw pgoutput message bytes; byte 0 = message discriminator)
//!        ▼
//!   pgoutput::Event::<Off,Off>::parse(&EventType::from_char(data[0]), &data[1..])
//!        │
//!        ├── Relation → cache oid → (table name, columns) for tuple decoding
//!        ├── Begin    → start a txn (stamp txn_id on subsequent events)
//!        ├── Commit   → close txn, advance checkpoint, update_applied_lsn
//!        ├── Insert/Update/Delete → resolve oid → table, build a Nostos RowOp
//!        └── (Message/Origin/Stream/...) → ignored (not needed for row sync)
//! ```
//!
//! ## LSN checkpointing (the "no data loss, no duplication" contract)
//!
//! Postgres holds WAL until the replication slot's `confirmed_flush_lsn` passes
//! it. We advance that LSN **only after a client ACKs** — the fan-out loop
//! queries the session store's `min_acked_lsn` and calls `advance_progress`,
//! which feeds pgwire-replication's progress atomic (the worker sends the
//! actual standby_status_update wire message on its own schedule). So on
//! crash/reconnect: Postgres replays from the last *client-acknowledged* LSN.
//! Combined with the client's own LSN checkpoint, sync is exactly-once across
//! restarts. This is the "kill criterion" for Phase 1 (ROADMAP).
//!
//! **What we deliberately do NOT do:** advance the slot per-event on XLogData.
//! That was the original bug — it told Postgres we'd applied events the moment
//! they arrived off the wire, before any client received them, so a reconnect
//! silently skipped unacked data. The ack-driven model here is the fix
//! (ADR-0009). WAL-bloat protection ships alongside it: the application-level
//! `EvictionPolicy` (applied by the fanout loop) disconnects lagging clients,
//! and `max_slot_wal_keep_size_mb` here is the database-level backstop — see
//! ADR-0016 (now shipped).
//!
//! ## Why two "Off" trait params for pgoutput
//!
//! `pgoutput` is a trait-selected generic API: `Event<Binary, Streaming>`.
//! - `BinaryValueTraitOff` → column values decode as `String` (text), not raw
//!   bytes. We store the whole tuple image as opaque payload; text keeps it
//!   debuggable and the wire codec re-hexes it anyway.
//! - `StreamingValueTraitOff` → no in-progress (streaming) large txns; the
//!   simpler struct variants (`InsertWithoutStreamingEnabled`, etc.).
//!
//! Both are zero-cost monomorphizations verified against pgoutput 0.0.7 source.

use std::collections::HashMap;
use std::fmt::Write; // ponytail: single write!() in json_escape for a String push
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use pgoutput::events::base::event::BaseEvent;
use pgoutput::events::base::relation::RelationWithoutStreamingEnabled;
use pgoutput::events::base::tuple_data::{TupleData, TupleDataColumn};
use pgoutput::events::event::{Event as PgEvent, EventType};
use pgoutput::options::{BinaryValueTraitOff, StreamingValueTraitOff};
use pgwire_replication::client::{ReplicationClient, ReplicationEvent};
use pgwire_replication::{Lsn as PgLsn, ReplicationConfig, TlsConfig};
use tokio_postgres::NoTls;
use tracing::{debug, error, info, warn};

use nostos_application::ports::{Metrics, ReplicatorStream, SlotHealth};
use nostos_domain::{Lsn, ReplicationEvent as NostosEvent, RowOp};

use super::typed;

/// The concrete pgoutput monomorphization we use everywhere in this module:
/// text values (no binary), no streaming large txns.
type Event = PgEvent<BinaryValueTraitOff, StreamingValueTraitOff>;

/// Result of [`PgReplicator::probe_slot_health`]. Encodes the missing-vs-lost-
/// vs-healthy trichotomy that `ensure_slot_and_publication` switches on. The
/// `Lost { slot_existed: false }` case is "slot MISSING on connect"; the
/// `slot_existed: true` case is `wal_status='lost'` (the slot row is still
/// there but PG has evicted the WAL it needed). Both are the same data-loss
/// class for our purposes — see the comment in `ensure_slot_and_publication`.
enum SlotProbe {
    /// Slot exists and WAL is retained. `restart_lsn` is carried so we can
    /// report the lag gauge without a second round-trip; the actual start LSN
    /// is resolved by `resolve_resume_lsn` (which prefers `confirmed_flush_lsn`
    /// — the ack-driven boundary, ADR-0009).
    Healthy { restart_lsn: Option<PgLsn> },
    /// Slot MISSING, OR present with `wal_status='lost'`. The retained WAL is
    /// gone; recovery is drop+recreate+resnapshot.
    Lost { slot_existed: bool },
}

/// Cached relation metadata, keyed by the OID pgoutput sends with each row op.
///
/// pgoutput sends a `Relation` message once per relation (table) before the
/// first change to it, then refers to rows by OID. We must remember the
/// OID→table-name mapping (and the PK column positions) to decode row ops.
///
/// `pub(crate)` so the snapshot module (`snapshot.rs`) can reuse the same
/// shape — snapshot rows and streamed rows must decode to byte-identical
/// payloads, so they share this metadata type.
#[derive(Debug, Clone)]
pub(crate) struct RelationMeta {
    /// `<namespace>.<name>`, e.g. `public.tasks`. We strip the `public.` prefix
    /// on emit so predicates (which use bare table names like `tasks`) match.
    pub(crate) qualified_name: String,
    /// Indices of columns flagged as part of the replica identity / primary key.
    /// Used to extract a string pk for the `RowOp`.
    pub(crate) pk_indices: Vec<usize>,
    /// `(column name, Postgres type OID)` in tuple order (ADR-0019) — used to
    /// build the typed JSON payload. The OID drives `typed::append_typed_value`
    /// so e.g. a `bool` column renders as a JSON bool, not a quoted string.
    pub(crate) columns: Vec<(String, i32)>,
}

/// Configuration for [`PgReplicator`].
///
/// Mirrors the connection knobs in `.env.example` (`NOSTOS_PG_*`).
#[derive(Debug, Clone)]
pub struct PgReplicatorConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Logical-replication slot name (created if absent on startup).
    pub slot: String,
    /// Publication name (created if absent on startup).
    pub publication: String,
    /// Resume from this LSN, or `None` to use the slot's last confirmed LSN.
    pub start_lsn: Option<Lsn>,
    /// WAL-bloat backstop: set `max_slot_wal_keep_size` (MB) on the slot via
    /// `ALTER_REPLICATION_SLOT` on startup. `0` = leave Postgres's default
    /// (unbounded). This is the database-level cap; the application-level
    /// eviction policy (`EvictionPolicy`, applied by the fanout loop) is the
    /// first line of defense — this is the last resort that protects the primary
    /// if a client vanishes entirely. See ADR-0016.
    pub max_slot_wal_keep_size_mb: u64,
}

impl PgReplicatorConfig {
    /// Build a config from a standard libpq-style URL
    /// (`postgresql://user:pass@host:port/db`).
    ///
    /// # Errors
    /// Returns an error if the URL is not a parseable postgres URL.
    pub fn from_url(
        url: &str,
        slot: impl Into<String>,
        publication: impl Into<String>,
    ) -> Result<Self, PgReplicatorError> {
        // Minimal libpq URL parse: postgresql://user:pass@host:port/db
        let rest = url
            .strip_prefix("postgresql://")
            .or_else(|| url.strip_prefix("postgres://"))
            .ok_or_else(|| PgReplicatorError::BadUrl("missing postgresql:// prefix".into()))?;

        // Split authority / path on the first '/'.
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        // authority = [user[:pass]@]host[:port]
        let (userinfo, hostport) = authority
            .split_once('@')
            .map_or((None, authority), |(u, h)| (Some(u), h));

        let (user, password) = match userinfo {
            Some(ui) => {
                let (u, p) = ui.split_once(':').unwrap_or((ui, ""));
                (u.to_string(), p.to_string())
            }
            None => ("postgres".to_string(), String::new()),
        };

        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5432)),
            None => (hostport.to_string(), 5432),
        };

        let database = if path.is_empty() {
            "postgres".to_string()
        } else {
            // Strip any trailing query string.
            path.split('?').next().unwrap_or(path).to_string()
        };

        Ok(Self {
            host,
            port,
            user,
            password,
            database,
            slot: slot.into(),
            publication: publication.into(),
            start_lsn: None,
            max_slot_wal_keep_size_mb: 0,
        })
    }
}

/// A real Postgres logical-replication source. Implements [`ReplicatorStream`].
///
/// One of these is constructed at server startup (when `NOSTOS_REPLICATOR=pg`)
/// and driven by [`FanOutService::run`](nostos_application::FanOutService). The
/// next-event loop blocks on the replication stream; on a row change it returns
/// a [`ReplicationEvent`], and on commit it advances the slot's confirmed LSN.
pub struct PgReplicator {
    cfg: PgReplicatorConfig,
    client: Option<ReplicationClient>,
    /// oid → relation metadata. Populated lazily from `Relation` messages.
    relations: HashMap<i32, RelationMeta>,
    /// The current in-progress transaction id (set on Begin, cleared on Commit).
    /// Events within one Postgres txn share this so the client can batch them.
    current_txn: Option<u64>,
    /// Highest WAL position we have SEEN off the wire (for diagnostics and the
    /// `last_confirmed_lsn()` accessor). This is NOT the LSN we've told Postgres
    /// we applied — that is ack-driven via [`Self::advance_progress`].
    last_seen: Lsn,
    /// Highest LSN we have told Postgres we durably applied (via
    /// `update_applied_lsn` in `advance_progress`). On reconnect we resume from
    /// here — that is the exactly-once boundary. Advanced ONLY from client ACKs.
    last_confirmed: Lsn,
    /// Initial-snapshot rows (fresh slot only). Drained by `next_event`
    /// BEFORE the live replication stream is polled, so a client subscribing
    /// to a populated table receives the existing rows first, then live
    /// changes. Empty on restart with an existing slot (no snapshot replay).
    /// See `snapshot.rs` and the module-level "initial snapshot" docs.
    pending_snapshot: std::collections::VecDeque<nostos_domain::ReplicationEvent>,
    /// Optional handle into the server's aggregate metrics. When attached (the
    /// production wiring in nostos-server), the slot-health gauge + recreated
    /// counter + WAL-lag gauge are updated on every (re)connect. `None` keeps
    /// the replicator usable from tests that don't care about metrics.
    metrics: Option<Arc<Metrics>>,
}

impl PgReplicator {
    /// Construct (does not connect — call [`Self::ensure_connected`] or rely on
    /// the first `next_event` to lazily connect).
    #[must_use]
    pub fn new(cfg: PgReplicatorConfig) -> Self {
        Self {
            cfg,
            client: None,
            relations: HashMap::new(),
            current_txn: None,
            last_seen: Lsn::ZERO,
            last_confirmed: Lsn::ZERO,
            pending_snapshot: std::collections::VecDeque::new(),
            metrics: None,
        }
    }

    /// Attach the server-wide aggregate metrics handle. The replicator reports
    /// slot-health (from `pg_replication_slots.wal_status`), WAL-lag, and the
    /// slot-recreated counter into it on every (re)connect, so the silent-
    /// data-loss risk of a missing/`lost` slot is operator-visible (ADR-0009).
    /// Mirrors `FanOutService::with_metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Ensure the replication slot + publication exist and the stream is open.
    ///
    /// Idempotent: safe to call on every reconnect. Uses a plain SQL control
    /// connection (`tokio_postgres`) to run the DDL, then opens the replication
    /// connection (`pgwire_replication`).
    ///
    /// On a FRESH slot (did not exist on connect): the existing rows are snapshotted
    /// via [`snapshot::snapshot_events`] under the slot's exported snapshot and
    /// staged in [`Self::pending_snapshot`]. `next_event` drains those rows before
    /// polling the live stream, so a client subscribing to a populated table gets
    /// the full current state first, then live changes (roadmap Phase 1). On
    /// restart with an EXISTING slot, no snapshot is emitted. All snapshot logic
    /// lives in [`Self::ensure_slot_and_publication`].
    ///
    /// # Errors
    /// Connection or SQL errors bubble up as [`PgReplicatorError`].
    pub async fn ensure_connected(&mut self) -> Result<(), PgReplicatorError> {
        // 1. Control-plane: ensure publication + slot, resolve start LSN, and
        //    (for a fresh slot) capture + stage the initial snapshot.
        let start_lsn = self.ensure_slot_and_publication().await?;

        // 2. Replication connection. `start_lsn` is the slot's consistent point
        //    (fresh slot) or the slot's last confirmed flush (restart).
        let cfg = ReplicationConfig {
            host: self.cfg.host.clone(),
            port: self.cfg.port,
            user: self.cfg.user.clone(),
            password: self.cfg.password.clone(),
            database: self.cfg.database.clone(),
            tls: TlsConfig::disabled(),
            slot: self.cfg.slot.clone(),
            publication: self.cfg.publication.clone(),
            start_lsn,
            stop_at_lsn: None,
            status_interval: Duration::from_secs(1),
            idle_wakeup_interval: Duration::from_secs(30),
            buffer_events: 8192,
        };
        let client = ReplicationClient::connect(cfg)
            .await
            .map_err(|e| PgReplicatorError::Connect(e.to_string()))?;
        self.client = Some(client);
        info!(
            slot = %self.cfg.slot,
            publication = %self.cfg.publication,
            start_lsn = %start_lsn,
            "PgReplicator connected to Postgres logical replication"
        );
        Ok(())
    }

    /// The libpq-style URL for this replicator's config. Used for the snapshot
    /// read connection (which must be separate from both the control-plane and
    /// the replication connections).
    fn pg_url(&self) -> String {
        format!(
            "postgresql://{}:{}@{}:{}/{}",
            self.cfg.user, self.cfg.password, self.cfg.host, self.cfg.port, self.cfg.database
        )
    }

    /// Probe `pg_replication_slots` for the slot's `wal_status` + `restart_lsn`.
    /// This is the cheap, every-(re)connect detection that catches the silent-
    /// data-loss cases (`missing` slot and `wal_status='lost'`). One round-trip
    /// on a control-plane connection; no replication-stream effect.
    ///
    /// `wal_status` values (PG docs):
    /// - `reserved` / `extended`: WAL retained, last LSN known — Healthy.
    /// - `unreserved`: WAL retained but PG may reclaim soon under pressure —
    ///   we treat as Healthy (the lag gauge is the operator signal here).
    /// - `lost`: WAL evicted (`max_slot_wal_keep_size` fired) — data-loss class.
    ///
    /// ponytail: a future PG major version adding a new wal_status variant will
    /// fall through to Healthy here — the lag gauge + recreate counter still
    /// surface trouble, and the explicit match in the trace log names the value.
    async fn probe_slot_health(&self, sql: &tokio_postgres::Client) -> SlotProbe {
        let row = sql
            .query_opt(
                "SELECT wal_status::text, restart_lsn::text \
                 FROM pg_replication_slots WHERE slot_name = $1",
                &[&self.cfg.slot],
            )
            .await;
        match row {
            Ok(Some(row)) => {
                let wal_status: String = row.get(0);
                let restart_text: Option<String> = row.get(1);
                let restart_lsn = restart_text
                    .filter(|s| !s.is_empty())
                    .and_then(|s| PgLsn::parse(&s).ok());
                match wal_status.as_str() {
                    "lost" => {
                        warn!(slot = %self.cfg.slot, "pg_replication_slots.wal_status = 'lost' (WAL evicted; data-loss class)");
                        SlotProbe::Lost { slot_existed: true }
                    }
                    other => {
                        debug!(slot = %self.cfg.slot, wal_status = %other, "slot healthy");
                        SlotProbe::Healthy { restart_lsn }
                    }
                }
            }
            Ok(None) => {
                // No row → slot does not exist. This is the original bug: a
                // previous nostos run created the slot, advanced it ack-driven,
                // then someone (a DB restore, manual drop, or a clean
                // re-provision) made it vanish. Treat as data-loss class.
                warn!(slot = %self.cfg.slot, "replication slot MISSING on connect (will recreate + resnapshot; potential data-loss window)");
                SlotProbe::Lost {
                    slot_existed: false,
                }
            }
            Err(e) => {
                // Probe failed (transient PG error / connection blip). Don't
                // block the slot-creation path — fall through to fresh-create,
                // which will either succeed (slot was actually missing) or
                // surface a real error. We log + treat as missing rather than
                // fail-fast: the fresh-create below is the safe superset.
                warn!(error = %e, slot = %self.cfg.slot, "slot-health probe failed; falling through to create path");
                SlotProbe::Lost {
                    slot_existed: false,
                }
            }
        }
    }

    /// Record the slot-health gauge into `self.metrics`, if attached.
    fn record_health(&self, health: SlotHealth) {
        if let Some(m) = self.metrics.as_ref() {
            m.set_slot_health(health);
        }
    }

    /// Record the slot-recreated counter into `self.metrics`, if attached.
    fn record_recreate(&self) {
        if let Some(m) = self.metrics.as_ref() {
            m.record_slot_recreate();
        }
    }

    /// Bump the slot-epoch counter into `self.metrics`, if attached (ADR-0025).
    fn record_epoch_bump(&self) {
        if let Some(m) = self.metrics.as_ref() {
            m.record_slot_epoch_bump();
        }
    }

    /// Set the gauge to the transient `Recreated` state so a flapping slot
    /// shows up in the metric trace even before the next health probe runs.
    fn mark_recreated_health(&self) {
        self.record_health(SlotHealth::Recreated);
    }

    /// Compute + record `pg_current_wal_lsn() - restart_lsn` as the WAL-lag
    /// gauge (bytes). Cheap (one round-trip) and only worth doing when the slot
    /// is healthy — a missing/lost slot reports the gauge as 0.
    async fn record_lag(&self, sql: &tokio_postgres::Client, restart: PgLsn) {
        let Ok(row) = sql
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await
        else {
            return;
        };
        let now_text: String = row.get(0);
        let Ok(now) = PgLsn::parse(&now_text) else {
            return;
        };
        let lag = now.as_u64().saturating_sub(restart.as_u64());
        if let Some(m) = self.metrics.as_ref() {
            m.set_replication_lag(lag);
        }
    }

    /// Pre-seed `relations` from `pg_class`/`pg_attribute` so row decoding works
    /// without waiting for (or relying on) a stream Relation message.
    ///
    /// For each table in the publication we capture: oid, namespace-qualified
    /// name, `(column name, type OID)` in order, and the PK column indices.
    /// Stream Relation messages later just refresh this — but having it
    /// up-front means a fresh replication connection to an existing slot
    /// decodes rows immediately.
    async fn bootstrap_relations_from_catalog(&mut self, sql: &tokio_postgres::Client) {
        match catalog_relations(sql, &self.cfg.publication).await {
            Ok(relations) => {
                let count = relations.len();
                self.relations.extend(relations);
                debug!(relations = count, "bootstrapped relations from catalog");
            }
            Err(e) => {
                warn!(error = %e, "could not bootstrap relations from catalog; relying on stream Relation messages");
            }
        }
        self.audit_replica_identity(sql).await;
    }

    /// Mechanical enforcement of ADR-0025 F1: every published table must have
    /// `REPLICA IDENTITY FULL`, or live DELETEs (and UPDATE old-rows) arrive
    /// without the tenant column and are silently dropped by tenant-scoped
    /// fan-out. This does NOT fail startup (an app that never deletes still
    /// works), but it screams once per connect with the exact fix SQL —
    /// same loud-error convention as the NOSTOS_WRITE_TABLES allowlist.
    async fn audit_replica_identity(&self, sql: &tokio_postgres::Client) {
        let q = "SELECT n.nspname || '.' || c.relname \
                 FROM pg_publication_tables pt \
                 JOIN pg_class c ON c.relname = pt.tablename \
                 JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = pt.schemaname \
                 WHERE pt.pubname = $1 AND c.relreplident <> 'f' \
                 ORDER BY 1";
        match sql.query(q, &[&self.cfg.publication]).await {
            Ok(rows) if !rows.is_empty() => {
                let tables: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
                for t in &tables {
                    error!(
                        table = %t,
                        "published table lacks REPLICA IDENTITY FULL: live DELETEs will carry \
                         only PK columns, so tenant-scoped delete fan-out SILENTLY DROPS them. \
                         Fix: ALTER TABLE {t} REPLICA IDENTITY FULL;"
                    );
                }
            }
            Ok(_) => {
                debug!("replica-identity audit: all published tables are FULL");
            }
            Err(e) => {
                warn!(error = %e, "could not audit replica identity of published tables");
            }
        }
    }

    /// Create publication + slot if absent, resolve the start LSN. On a FRESH
    /// slot, also capture the initial snapshot (under the slot's exported
    /// snapshot) and stage it in [`Self::pending_snapshot`] for `next_event`
    /// to drain before the live stream.
    ///
    /// Returns the streaming start LSN. The snapshot (if any) is staged as a
    /// side effect on `self.pending_snapshot`.
    ///
    /// ## How the snapshot is obtained (the design decision)
    ///
    /// `pgwire-replication` 0.3.2's `ReplicationClient` only issues
    /// `START_REPLICATION SLOT … LOGICAL` (crate src `client/worker.rs:186`);
    /// it does NOT send `CREATE_REPLICATION_SLOT`, and the crate exposes no
    /// path to the walsender-protocol `CREATE_REPLICATION_SLOT … (SNAPSHOT
    /// 'export')` variant that would return `consistent_point` +
    /// `snapshot_name` (see docs.rs/pgwire-replication/0.3.2 — there is no
    /// slot-creation API; the example flow pre-creates the slot out-of-band).
    /// So for a FRESH slot we create it via the **SQL** function
    /// `pg_create_logical_replication_slot(name, plugin)` inside the SAME
    /// REPEATABLE READ transaction that calls `pg_export_snapshot()`. Both
    /// operations materialize against the transaction's snapshot, so the
    /// exported snapshot's view of the database is exactly the state the slot
    /// will start streaming from (its `consistent_point`). The transaction
    /// stays OPEN across the call so the snapshot id remains importable while
    /// `snapshot::snapshot_events` reads the tables on a second connection
    /// (the snapshot id is only valid until the exporting txn commits — see
    /// the `SET TRANSACTION SNAPSHOT` docs). We commit before returning.
    ///
    /// ## Slot-exists-on-start edge case
    ///
    /// If the slot already exists (restart), we do NOT export a snapshot —
    /// there is no fresh consistent point, and re-emitting the snapshot would
    /// duplicate rows the client already has. The start LSN resolves from the
    /// slot's `confirmed_flush_lsn` → `restart_lsn` → current WAL, unchanged.
    ///
    /// ## Explicit-start-LSN edge case
    ///
    /// If the caller set `cfg.start_lsn`, we honor it verbatim and skip the
    /// snapshot — that path is for resuming at an exact point, not for the
    /// initial sync. (ponytail: if a caller later wants a snapshot AT a
    /// specific LSN, the snapshot-vs-start-LSN interaction needs its own
    /// design; for now they are mutually exclusive.)
    ///
    /// Preference: explicit `cfg.start_lsn` → slot's `confirmed_flush_lsn` →
    /// slot's `restart_lsn` → `pg_current_wal_lsn()`. This is the exact
    /// pattern from `pgwire-replication`'s own `checkpointed` example.
    async fn ensure_slot_and_publication(&mut self) -> Result<PgLsn, PgReplicatorError> {
        let (sql, conn) = tokio_postgres::connect(&self.pg_url(), NoTls)
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        // Publication (best-effort — may already exist). This is the
        // publication-missing edge case: a fresh DB with no publication must
        // still work, and an existing publication must not error.
        if let Err(e) = sql
            .batch_execute(&format!(
                "DO $$ BEGIN \
                 IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{}') THEN \
                 CREATE PUBLICATION {} FOR ALL TABLES; \
                 END IF; \
                 END $$;",
                self.cfg.publication, self.cfg.publication
            ))
            .await
        {
            warn!(error = %e, "could not ensure publication (may already exist)");
        }

        // Pre-seed the relation cache from the catalog so decoding works on a
        // fresh connection even if PG doesn't re-emit Relation messages. This
        // is the fix for "second connection decodes nothing" — pgoutput only
        // sends Relation once per relation per slot, but a *new* replication
        // connection to the same slot starts with an empty client-side cache.
        self.bootstrap_relations_from_catalog(&sql).await;

        // Explicit start LSN wins — skip snapshot (resume-at-exact-LSN path).
        if let Some(explicit) = self.cfg.start_lsn {
            // Make sure the slot exists for START_REPLICATION (best-effort —
            // the resume path may legitimately target a pre-existing slot).
            let _ = sql
                .batch_execute(&format!(
                    "SELECT * FROM pg_create_logical_replication_slot('{}', 'pgoutput');",
                    self.cfg.slot
                ))
                .await;
            self.seed_resume(explicit);
            return Ok(PgLsn::from_u64(explicit.raw()));
        }

        // ── Slot-health probe: distinguish "EXISTS + healthy" (a real restart)
        //    from "MISSING / wal_status='lost'" (silent-data-loss risk). The
        //    pre-fix code treated a missing slot as FRESH and silently resumed
        //    from current WAL, destroying every change that happened while nostos
        //    was offline — a `lost` slot (max_slot_wal_keep_size fired) is the
        //    same data-loss class. We now log CRITICAL + bump the
        //    `slot_recreated_total` counter + re-snapshot, so the loss is
        //    operator-visible instead of silent. Design choice (re-snapshot-
        //    with-loud-warning over fail-fast) keeps the client working while
        //    flagging the risk; matches the manual recovery documented in
        //    example/README.md:194-239. See ADR-0009 (ack-driven LSN — a slot
        //    advanced past unacked LSN is silent data loss on reconnect).
        let probe = self.probe_slot_health(&sql).await;
        match probe {
            SlotProbe::Healthy { restart_lsn } => {
                // Real restart: slot exists with WAL retained. Resolve start
                // LSN from the slot and report the lag gauge.
                self.record_health(SlotHealth::Healthy);
                if let Some(restart) = restart_lsn {
                    self.record_lag(&sql, restart).await;
                }
                return self.resolve_resume_lsn(&sql).await;
            }
            SlotProbe::Lost { slot_existed } => {
                // The slot either is missing on connect or has wal_status='lost'
                // (max_slot_wal_keep_size evicted the retained WAL while nostos
                // was offline). Both are silent-data-loss classes: WAL between
                // the last client-acked LSN and now is gone. We cannot recover
                // it — but we CAN make it visible. Log CRITICAL, bump the
                // counter, then drop (if it still exists) and re-create with a
                // fresh snapshot so the client at least converges on current
                // state instead of silently stalling at the head of the stream.
                error!(
                    slot = %self.cfg.slot,
                    slot_existed,
                    "DATA-LOSS RISK: replication slot was missing or wal_status='lost' \
                     on connect. WAL between the last client-acked LSN and the new \
                     consistent point is unrecoverable. Recreating + re-snapshotting; \
                     alert on nostos_slot_recreated_total and investigate \
                     max_slot_wal_keep_size / nostos downtime. (ADR-0009)"
                );
                self.record_health(SlotHealth::Lost);
                self.record_recreate();
                // Bump the slot epoch AT THE RECREATE-DECISION POINT, not at the
                // end of the fresh-create block below. The reconnect-resume gate
                // (ADR-0025 slice 4) reads `slot_epoch` to decide whether a
                // reconnecting client must full-snapshot; bumping it only after
                // the snapshot + COMMIT (the old placement) left a window where a
                // client reconnecting mid-recreate saw the STALE epoch and the
                // gate misfired. This Lost branch is the single chute reached by
                // BOTH first-create (slot missing) and Lost-recreate, so this is
                // the one bump per slot lineage.
                self.record_epoch_bump();
                if slot_existed {
                    // Drop the invalidated slot so the fresh-create path below
                    // succeeds. pg_drop_replication_slot fails if the slot is
                    // still active — but we are on a control-plane connection
                    // here, not the replication connection (which has not been
                    // opened yet this cycle), so the slot is necessarily
                    // inactive from our point of view.
                    if let Err(e) = sql
                        .query_one("SELECT pg_drop_replication_slot($1)", &[&self.cfg.slot])
                        .await
                    {
                        warn!(error = %e, slot = %self.cfg.slot, "drop of lost/invalidated slot failed; will attempt fresh create anyway");
                    }
                }
                self.mark_recreated_health();
            }
        }

        // ── FRESH slot: create it inside a REPEATABLE READ txn that also
        //    exports a snapshot, and hold that txn open while the snapshot is
        //    read. This is the snapshot-vs-stream exactly-once boundary.
        sql.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;
        let snapshot_name: String = sql
            .query_one("SELECT pg_export_snapshot()", &[])
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?
            .get(0);
        // pg_create_logical_replication_slot returns (slot_name, lsn). The lsn
        // is the slot's consistent point — where streaming will start, and the
        // LSN at which the exported snapshot is consistent.
        let consistent_point: String = sql
            .query_one(
                "SELECT lsn::text FROM pg_create_logical_replication_slot($1, 'pgoutput')",
                &[&self.cfg.slot],
            )
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?
            .get(0);
        let consistent_lsn =
            PgLsn::parse(&consistent_point).map_err(|e| PgReplicatorError::Lsn(e.to_string()))?;

        // Run the snapshot read while the exporting txn is still open. The
        // snapshot id is invalid the moment this txn commits. (See module
        // docs on snapshot.rs for the exactly-once boundary rationale.)
        let snapshot_events = match crate::replicator::snapshot::snapshot_events(
            &self.pg_url(),
            &self.cfg.publication,
            &snapshot_name,
            Lsn::new(consistent_lsn.as_u64()),
        )
        .await
        {
            Ok(events) => {
                info!(
                    slot = %self.cfg.slot,
                    rows = events.len(),
                    consistent_point = %consistent_lsn,
                    "initial snapshot captured; will drain before live stream"
                );
                events
            }
            Err(e) => {
                // Snapshot failure is not fatal: the live stream still
                // delivers changes from the consistent point forward. But
                // existing rows would be missed, so log loudly. We still
                // commit the txn (to release the slot-creation) and proceed.
                warn!(error = %e, "initial snapshot failed; clients will NOT receive pre-existing rows (live stream still active)");
                Vec::new()
            }
        };
        sql.batch_execute("COMMIT")
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;

        // Stage the snapshot rows for `next_event` to drain before the stream.
        self.pending_snapshot.extend(snapshot_events);

        self.seed_resume(Lsn::new(consistent_lsn.as_u64()));

        // NOTE: the slot-epoch bump lives ABOVE, at the recreate-decision point
        // in the `SlotProbe::Lost` arm (not here) — see that comment for why a
        // late bump at end-of-fresh-create left a reconnect-resume gate window.
        Ok(consistent_lsn)
    }

    /// Resolve the streaming start LSN for a RESTART (existing slot):
    /// `confirmed_flush_lsn` → `restart_lsn` → `pg_current_wal_lsn()`.
    async fn resolve_resume_lsn(
        &mut self,
        sql: &tokio_postgres::Client,
    ) -> Result<PgLsn, PgReplicatorError> {
        let row = sql
            .query_one(
                "SELECT confirmed_flush_lsn::text, restart_lsn::text \
                 FROM pg_replication_slots WHERE slot_name = $1",
                &[&self.cfg.slot],
            )
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;
        let confirmed: Option<String> = row.get(0);
        let restart: Option<String> = row.get(1);
        if let Some(s) = confirmed.filter(|s| !s.is_empty()) {
            let lsn = PgLsn::parse(&s).map_err(|e| PgReplicatorError::Lsn(e.to_string()))?;
            self.seed_resume(Lsn::new(lsn.as_u64()));
            return Ok(lsn);
        }
        if let Some(s) = restart.filter(|s| !s.is_empty()) {
            let lsn = PgLsn::parse(&s).map_err(|e| PgReplicatorError::Lsn(e.to_string()))?;
            self.seed_resume(Lsn::new(lsn.as_u64()));
            return Ok(lsn);
        }
        let row = sql
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;
        let now: String = row.get(0);
        let lsn = PgLsn::parse(&now).map_err(|e| PgReplicatorError::Lsn(e.to_string()))?;
        self.seed_resume(Lsn::new(lsn.as_u64()));
        Ok(lsn)
    }

    /// The highest LSN Postgres has been told we applied (ack-driven). Read by
    /// the server for checkpoint/metrics and by tests asserting resume
    /// correctness. This advances ONLY when a client ACKs — never per-event.
    #[must_use]
    pub fn last_confirmed_lsn(&self) -> Lsn {
        self.last_confirmed
    }

    /// The highest WAL position observed off the wire (diagnostic). May be ahead
    /// of `last_confirmed_lsn()` when clients are lagging — that gap is exactly
    /// the WAL Postgres retains for them.
    #[must_use]
    pub fn last_seen_lsn(&self) -> Lsn {
        self.last_seen
    }

    /// Seed both progress cursors at the resume point on (re)connect. The slot
    /// will start streaming from here; `last_confirmed` is what the previous
    /// connection last told Postgres it applied (the exactly-once boundary).
    fn seed_resume(&mut self, lsn: Lsn) {
        self.last_confirmed = lsn;
        self.last_seen = lsn;
    }

    /// Pull the next event off the wire and translate it to a Nostos event.
    ///
    /// Row events (Insert/Update/Delete) return `Some`. Begin/Commit/Relation
    /// and the non-row pgoutput variants are handled internally and the loop
    /// continues — so `next_event` returns the next *row* change (mirroring
    /// `FakeReplicator`'s contract: one row op per call).
    async fn next_row(&mut self) -> Result<Option<NostosEvent>, PgReplicatorError> {
        loop {
            // Pull one wire event. We need the replication client only for the
            // recv; decode happens after the borrow ends so we can mutate self.
            let wire = {
                if self.client.is_none() {
                    self.ensure_connected().await?;
                }
                let client = self
                    .client
                    .as_mut()
                    .expect("ensure_connected must populate client");
                match client.recv().await {
                    Ok(ev) => ev,
                    Err(e) => {
                        error!(error = %e, "replication recv error; will attempt reconnect");
                        self.client = None;
                        return Err(PgReplicatorError::Recv(e.to_string()));
                    }
                }
            };
            let Some(ev) = wire else {
                // Clean end of a bounded stream (e.g. stop_at_lsn reached).
                return Ok(None);
            };
            match ev {
                ReplicationEvent::XLogData { wal_end, data, .. } => {
                    // Decode first (mutates self.relations). We deliberately do
                    // NOT call `update_applied_lsn` here — that was the
                    // silent-data-loss-on-resume bug: it advanced the slot past
                    // events the client had not yet received. The ack-driven
                    // advance now happens in `advance_progress`, called by the
                    // fan-out loop with the min acked LSN across sessions.
                    // Pass wal_end so the decoded event carries its true LSN.
                    let decoded = if data.is_empty() {
                        None
                    } else {
                        self.decode(&data, Lsn::new(wal_end.as_u64()))
                    };
                    // Track the highest WAL position we've SEEN (for diagnostics
                    // and resume bookkeeping) — but do not tell Postgres we've
                    // applied it. That is the caller's job, ack-driven.
                    self.last_seen = Lsn::new(wal_end.as_u64());
                    if let Some(decoded) = decoded {
                        return Ok(Some(decoded));
                    }
                    // Non-row message (relation/begin/commit) — loop.
                }
                ReplicationEvent::Commit { end_lsn, .. } => {
                    self.current_txn = None;
                    // Record the commit boundary but DO NOT advance the slot
                    // here — the commit LSN may be past unconsumed rows for a
                    // slow client. Ack-driven advance in `advance_progress`.
                    self.last_seen = Lsn::new(end_lsn.as_u64());
                }
                ReplicationEvent::KeepAlive { wal_end, .. } => {
                    // Server heartbeat. The pgwire-replication worker handles
                    // sending standby_status_update wire feedback on its own
                    // schedule (status_interval + reply requests), reading the
                    // applied-LSN we set via `update_applied_lsn` in
                    // `advance_progress`. We only record what we've seen.
                    self.last_seen = Lsn::new(wal_end.as_u64());
                }
                // Begin / StoppedAt / Message: transaction boundaries and control
                // frames we don't turn into row ops — loop and pull the next.
                ReplicationEvent::Begin { .. }
                | ReplicationEvent::StoppedAt { .. }
                | ReplicationEvent::Message { .. } => {}
            }
        }
    }

    /// Decode one raw pgoutput message into either a Nostos row event (`Some`)
    /// or `None` for non-row messages (relation/begin/commit/ignored).
    /// `lsn` is this message's WAL position (wal_end from the wire frame).
    fn decode(&mut self, data: &[u8], lsn: Lsn) -> Option<NostosEvent> {
        let disc = data[0];
        let Some(event_type) = EventType::from_char(disc) else {
            debug!(
                discriminator = disc,
                "unknown pgoutput message discriminator"
            );
            return None;
        };
        let parsed = match Event::parse(&event_type, &data[1..]) {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "pgoutput parse error; skipping message");
                return None;
            }
        };
        let PgEvent::Base(base) = parsed else {
            // Message/Origin/Stream/TwoPhase — not row sync; ignore.
            return None;
        };
        match base {
            BaseEvent::Relation(rel) => {
                self.cache_relation(&rel);
                None
            }
            BaseEvent::Begin(begin) => {
                // xid arrives as a SIGNED i32 decode of PG's u32 TransactionId:
                // every xid ≥ 2^31 reads negative (audit 2026-08-17 H2 — the
                // old clamp-to-0 collapsed all such transactions into one
                // "txn 0", silently losing per-transaction atomicity on the
                // client and stalling apply under sustained load). Reinterpret
                // the bits as unsigned to recover the real xid.
                self.current_txn = Some(xid_of(begin.transaction_id));
                None
            }
            BaseEvent::Commit(_) => {
                self.current_txn = None;
                None
            }
            BaseEvent::Insert(ins) => self.full_row_op(
                ins.oid,
                &ins.data,
                None,
                nostos_domain::Operation::Insert,
                lsn,
            ),
            BaseEvent::Update(upd) => {
                // Pass the OLD tuple through (Some under REPLICA IDENTITY
                // FULL) so unchanged-toasted columns in the new tuple can
                // be restored to their real values (M2).
                let old = upd.old_data_or_primary_key.as_ref().and_then(|o| match o {
                    pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::OldTupleData(t) => Some(t),
                    pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::PrimaryKeyTupleData(_) => None,
                });
                self.full_row_op(
                    upd.oid,
                    &upd.data,
                    old,
                    nostos_domain::Operation::Update,
                    lsn,
                )
            }
            BaseEvent::Delete(del) => {
                self.pk_only_op(del.oid, del.old_data_or_primary_key.as_ref(), lsn)
            }
            BaseEvent::Type(_) | BaseEvent::Truncate(_) => None,
        }
    }

    /// Stamp the LSN + current txn id onto a row op.
    fn stamp(&self, op: RowOp, lsn: Lsn) -> NostosEvent {
        let mut ev = NostosEvent::new(lsn, op);
        if let Some(t) = self.current_txn {
            ev = ev.with_txn(t);
        }
        ev
    }

    /// Cache a relation's metadata so subsequent row ops can resolve oid → table.
    fn cache_relation(&mut self, rel: &RelationWithoutStreamingEnabled) {
        // `c.oid` here is pgoutput's field name for the column's *type* OID
        // (the wire protocol's "data type ID"), not a table/object oid —
        // verified against pgoutput 0.0.7's `RelationColumn` source.
        let columns: Vec<(String, i32)> = rel
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.oid))
            .collect();
        // pk_indices authority: the catalog (`pg_index.indkey`, populated by
        // `bootstrap_relations_from_catalog` / `catalog_relations`) is the
        // *primary key*. The Relation message's column flag bit 0 marks the
        // *replica identity*: under DEFAULT that equals the PK (so the flag
        // filter is right), but under `REPLICA IDENTITY FULL` it marks EVERY
        // column — the flag filter would then yield a composite "pk" of all
        // columns, breaking pk matching for every op (ADR-0025 delete-tenant
        // fix enables FULL on tenant tables). Preserve the catalog's pk_indices
        // when present; fall back to the flag heuristic only when bootstrap
        // hasn't populated the entry (DEFAULT identity is the common case where
        // flag == PK, so the fallback stays correct there).
        let catalog_pk = self.relations.get(&rel.oid).map(|e| e.pk_indices.clone());
        let pk_indices = match catalog_pk {
            Some(v) if !v.is_empty() => v,
            _ => {
                let flagged: Vec<usize> = rel
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.flags & 0x1 != 0)
                    .map(|(i, _)| i)
                    .collect();
                if flagged.is_empty() {
                    vec![0]
                } else {
                    flagged
                }
            }
        };
        let qualified_name =
            if rel.relation_namespace.is_empty() || rel.relation_namespace == "public" {
                rel.name.clone()
            } else {
                format!("{}.{}", rel.relation_namespace, rel.name)
            };
        let oid = rel.oid;
        self.relations.insert(
            oid,
            RelationMeta {
                qualified_name,
                pk_indices,
                columns,
            },
        );
        debug!(oid, "cached relation metadata");
    }

    /// Build a full-row `RowOp` (Insert/Update) from a tuple image.
    fn full_row_op(
        &self,
        oid: i32,
        data: &TupleData<BinaryValueTraitOff>,
        old: Option<&TupleData<BinaryValueTraitOff>>,
        op: nostos_domain::Operation,
        lsn: Lsn,
    ) -> Option<NostosEvent> {
        let meta = self.relations.get(&oid)?;
        let table = meta.qualified_name.clone();
        let pk = pk_string(meta, data);
        let payload = Bytes::from(tuple_to_json_payload(meta, data, old));
        let row = match op {
            nostos_domain::Operation::Insert => RowOp::Insert { table, pk, payload },
            nostos_domain::Operation::Update => RowOp::Update { table, pk, payload },
            nostos_domain::Operation::Delete => {
                // Deletes have no full tuple — shouldn't reach here.
                RowOp::Delete {
                    table,
                    pk,
                    old_payload: None,
                }
            }
        };
        Some(self.stamp(row, lsn))
    }

    /// Build a `RowOp::Delete` from the PK/old-key tuple.
    ///
    /// Under `REPLICA IDENTITY FULL`, Postgres ships the full pre-delete row
    /// (`OldTupleData`) — we render it to `old_payload` so the op-log writer can
    /// lift the tenant column for tenant-scoped delete replay (ADR-0025
    /// follow-up). Under `REPLICA IDENTITY DEFAULT` only the PK columns arrive
    /// (`PrimaryKeyTupleData`) — `old_payload` stays `None` and delete replay
    /// falls back to snapshot-reconcile (slice 1).
    fn pk_only_op(
        &self,
        oid: i32,
        old: Option<
            &pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData<BinaryValueTraitOff>,
        >,
        lsn: Lsn,
    ) -> Option<NostosEvent> {
        let meta = self.relations.get(&oid)?;
        let table = meta.qualified_name.clone();
        let (pk, old_payload) = match old {
            Some(pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::OldTupleData(
                tuple,
            )) => (
                pk_string(meta, tuple),
                Some(Bytes::from(tuple_to_json_payload(meta, tuple, None))),
            ),
            Some(pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::PrimaryKeyTupleData(
                tuple,
            )) => (pk_string(meta, tuple), None),
            None => ("0".to_string(), None),
        };
        Some(self.stamp(
            RowOp::Delete {
                table,
                pk,
                old_payload,
            },
            lsn,
        ))
    }
}

/// `(col name, type oid, is_pk)` per column — the intermediate shape
/// `catalog_relations` groups rows into before building `RelationMeta`.
type RawColumn = (String, i32, bool);

/// Read publication table column metadata (oid, namespace-qualified name,
/// `(column name, type OID)` in publication order, PK indices) from
/// `pg_publication_tables`/`pg_attribute`/`pg_index`. Shared by the streaming
/// bootstrap ([`PgReplicator::bootstrap_relations_from_catalog`]) and the
/// initial-snapshot catalog read (`snapshot::snapshot_events`) — ONE query,
/// ONE grouping, so both build [`RelationMeta`] (and therefore render JSON
/// payloads) identically. `pub(crate)` so `snapshot.rs` can call it directly.
///
/// # Errors
/// Bubbles up the underlying `tokio_postgres` query error. Callers decide
/// how to handle it: the streaming bootstrap treats it as non-fatal (falls
/// back to stream `Relation` messages); the snapshot path treats it as fatal
/// (there is no fallback for decoding a `COPY` without column metadata).
pub(crate) async fn catalog_relations(
    sql: &tokio_postgres::Client,
    publication: &str,
) -> Result<std::collections::BTreeMap<i32, RelationMeta>, tokio_postgres::Error> {
    // All columns in publication order, with attnum + type oid + PK flag.
    // PK detection: pg_index.indkey is an int2vector; we test membership of
    // each column's attnum against the primary-key index's indkey via = ANY.
    // Cast oid/atttypid::int so tokio-postgres deserializes them as plain i32.
    let rows = sql
        .query(
            "SELECT c.oid::int, n.nspname, c.relname, a.attname, a.atttypid::int, \
                    (a.attnum = ANY (coalesce(i.indkey::int2[], ARRAY[]::int2[]))) AS is_pk \
             FROM pg_publication_tables pt \
             JOIN pg_class c ON c.relname = pt.tablename \
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = pt.schemaname \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
             LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
             WHERE pt.pubname = $1 \
             ORDER BY c.oid, a.attnum",
            &[&publication],
        )
        .await?;

    // Group rows by oid (sorted by oid, then attnum).
    let mut by_oid: std::collections::BTreeMap<i32, (String, Vec<RawColumn>)> =
        std::collections::BTreeMap::new();
    for row in rows {
        let oid: i32 = row.get(0);
        let nsp: String = row.get(1);
        let rel: String = row.get(2);
        let attname: String = row.get(3);
        let atttypid: i32 = row.get(4);
        let is_pk: bool = row.get(5);
        let qualified = if nsp == "public" || nsp.is_empty() {
            rel
        } else {
            format!("{nsp}.{rel}")
        };
        let entry = by_oid.entry(oid).or_insert_with(|| (qualified, Vec::new()));
        entry.1.push((attname, atttypid, is_pk));
    }

    let mut out = std::collections::BTreeMap::new();
    for (oid, (qualified, cols)) in by_oid {
        let columns: Vec<(String, i32)> = cols.iter().map(|(n, t, _)| (n.clone(), *t)).collect();
        let mut pk_indices: Vec<usize> = cols
            .iter()
            .enumerate()
            .filter(|(_, (_, _, is_pk))| *is_pk)
            .map(|(i, _)| i)
            .collect();
        if pk_indices.is_empty() {
            // Defensive default: no PK flagged (e.g. no replica identity) —
            // fall back to column 0, matching the streaming path's default.
            pk_indices = vec![0];
        }
        out.insert(
            oid,
            RelationMeta {
                qualified_name: qualified,
                pk_indices,
                columns,
            },
        );
    }
    Ok(out)
}

/// Extract the PK value(s) as a single string (comma-joined if composite).
fn pk_string(meta: &RelationMeta, data: &TupleData<BinaryValueTraitOff>) -> String {
    let parts: Vec<String> = meta
        .pk_indices
        .iter()
        .filter_map(|&i| data.get(i))
        .map(|c| match c {
            TupleDataColumn::Value(s) => s.clone(),
            TupleDataColumn::PGNull => "null".to_string(),
            TupleDataColumn::PGUnchangedToastedValue => "?".to_string(),
        })
        .collect();
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(",")
    }
}

/// Render a tuple as a typed JSON object `{ "col": <typed value>, ... }` for
/// the payload (ADR-0019). Every column goes through
/// `typed::append_typed_value`, keyed by its Postgres type OID — the same
/// function `snapshot::build_json_payload` uses, so a streamed row and a
/// snapshot row of identical data render byte-identically.
fn tuple_to_json_payload(
    meta: &RelationMeta,
    data: &TupleData<BinaryValueTraitOff>,
    old: Option<&TupleData<BinaryValueTraitOff>>,
) -> Vec<u8> {
    // Manual JSON build to avoid a serde_json dependency in this hot path;
    // tuples are small (a handful of columns).
    let mut out = String::from('{');
    for (i, (col, type_oid)) in meta.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, col);
        out.push_str("\":");
        let cell: Option<&str> = match data.get(i) {
            Some(TupleDataColumn::Value(v)) => Some(v.as_str()),
            // Unchanged-toasted marker (audit 2026-08-17 M2): PG emits `u`
            // in the NEW tuple of an UPDATE when a toasted column was not
            // modified — rendering it as "" would silently CLOBBER the
            // client's copy of a >2KB value. Under REPLICA IDENTITY FULL
            // (required on synced tables — pg-init sets it) the OLD tuple
            // carries the full pre-update row image, and the value there is
            // definitionally the unchanged one: substitute it. Without FULL
            // there is no local source of truth — keep the "" placeholder
            // (the pre-fix behavior; the real fix there is the wire
            // sentinel, still deferred).
            Some(TupleDataColumn::PGUnchangedToastedValue) => match old.and_then(|t| t.get(i)) {
                Some(TupleDataColumn::Value(v)) => Some(v.as_str()),
                _ => Some(""),
            },
            Some(TupleDataColumn::PGNull) | None => None,
        };
        typed::append_typed_value(&mut out, *type_oid, cell);
    }
    out.push('}');
    out.into_bytes()
}

/// Minimal JSON string escaping (in-place append).
///
/// `pub(crate)` so the snapshot module can build payloads byte-identical to the
/// streaming path's `tuple_to_json_payload`.
pub(crate) fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // ponytail: write! avoids the allocation clippy flags; the trait
                // is imported below for this single use.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

#[async_trait]
impl ReplicatorStream for PgReplicator {
    async fn next_event(&mut self) -> Option<NostosEvent> {
        // Drain the initial snapshot FIRST — these are the pre-existing rows a
        // fresh client must receive before live changes. All snapshot rows are
        // stamped at the slot's consistent point; the live stream begins at
        // exactly that point, so no row is missed or duplicated across the
        // snapshot→stream boundary (see snapshot.rs module docs + the
        // concurrent-writes e2e test). Empty on restart (existing slot).
        if let Some(ev) = self.pending_snapshot.pop_front() {
            return Some(ev);
        }
        loop {
            match self.next_row().await {
                Ok(Some(ev)) => return Some(ev),
                Ok(None) => {
                    // Stream ended cleanly. For a live replication feed this is
                    // unusual; reconnect to keep streaming.
                    if self.ensure_connected().await.is_err() {
                        return None;
                    }
                }
                Err(e) => {
                    // SQLSTATE 55000 (`object_not_in_prerequisite_state`) is
                    // what PG raises when the replication slot is gone or
                    // invalidated mid-stream (e.g. an operator drop, an
                    // ALTER-SLOT, or a `max_slot_wal_keep_size` eviction that
                    // lands between keepalives). The pre-fix loop just retried
                    // `ensure_connected` every 2s forever — but the slot was
                    // gone, so each retry re-created it silently and resumed
                    // from current WAL (silent data loss). Now we detect the
                    // case explicitly: log CRITICAL, set the Lost gauge, bump
                    // the recreate counter here (the actual drop+recreate+re-
                    // snapshot happens inside `ensure_slot_and_publication` on
                    // the next `ensure_connected` call, driven by the slot-
                    // health probe). String-match because pgwire-replication
                    // 0.3.2's recv error type does not expose SQLSTATE
                    // directly (ponytail: if a future version exposes the
                    // SqlState enum, compare against
                    // `SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE` instead).
                    let msg = e.to_string();
                    let is_slot_invalidated = msg.contains("55000")
                        || msg.contains("object_not_in_prerequisite_state")
                        || msg.contains("replication slot")
                        || msg.contains("does not exist");
                    if is_slot_invalidated {
                        error!(
                            error = %e,
                            slot = %self.cfg.slot,
                            "DATA-LOSS RISK: replication slot dropped or invalidated \
                             mid-stream (SQLSTATE 55000 class). Recreating + re-snapshotting \
                             on reconnect; alert on nostos_slot_recreated_total. (ADR-0009)"
                        );
                        self.record_health(SlotHealth::Lost);
                        self.record_recreate();
                    } else {
                        error!(error = %e, "PgReplicator error; will reconnect after backoff");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = self.ensure_connected().await;
                }
            }
        }
    }

    /// Advance Postgres's `confirmed_flush_lsn` to `lsn` — declaring every event
    /// up to here has been acknowledged by all live clients (ADR-0009).
    ///
    /// This feeds pgwire-replication's shared progress atomic; the worker sends
    /// the actual standby_status_update wire message on its own schedule
    /// (status_interval + keepalive replies). We never advance past `lsn`, so a
    /// reconnect replays from the last confirmed point — no silent data loss.
    async fn advance_progress(&mut self, lsn: Lsn) {
        // Monotonic: ignore a lower LSN (can happen if a stale ack races in).
        if lsn.raw() <= self.last_confirmed.raw() {
            return;
        }
        if let Some(client) = self.client.as_ref() {
            client.update_applied_lsn(PgLsn::from_u64(lsn.raw()));
            self.last_confirmed = lsn;
            debug!(confirmed_lsn = %lsn, "advanced replication slot (ack-driven)");
        }
    }
}

/// Errors from the Postgres replicator. Kept as a flat enum; the caller
/// (`next_event`) logs and reconnects rather than surfacing to the fan-out loop.
#[derive(Debug, thiserror::Error)]
pub enum PgReplicatorError {
    #[error("bad postgres URL: {0}")]
    BadUrl(String),
    #[error("control-plane SQL error: {0}")]
    ControlPlane(String),
    #[error("replication connect error: {0}")]
    Connect(String),
    #[error("replication recv error: {0}")]
    Recv(String),
    #[error("LSN parse error: {0}")]
    Lsn(String),
}

/// Recover the real transaction id from pgoutput's SIGNED decode of PG's
/// u32 TransactionId: xids ≥ 2^31 arrive negative; reinterpret the bits,
/// never clamp (clamping collapsed every such transaction into "txn 0" —
/// audit 2026-08-17 H2).
fn xid_of(transaction_id: i32) -> u64 {
    u64::from(transaction_id.cast_unsigned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_xid_is_reinterpreted_unsigned_not_clamped() {
        assert_eq!(xid_of(0), 0);
        assert_eq!(xid_of(42), 42);
        // -1 is the u32::MAX xid; i32::MIN is exactly 2^31. The old clamp
        // turned all of these into 0.
        assert_eq!(xid_of(-1), u64::from(u32::MAX));
        assert_eq!(xid_of(i32::MIN), u64::from(1u32 << 31));
    }

    #[test]
    fn parses_libpq_url_with_credentials() {
        let cfg = PgReplicatorConfig::from_url(
            "postgresql://nostos:nostos@localhost:5433/nostos",
            "nostos_slot",
            "nostos_pub",
        )
        .unwrap();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5433);
        assert_eq!(cfg.user, "nostos");
        assert_eq!(cfg.password, "nostos");
        assert_eq!(cfg.database, "nostos");
        assert_eq!(cfg.slot, "nostos_slot");
        assert_eq!(cfg.publication, "nostos_pub");
    }

    #[test]
    fn parses_url_without_port_and_credentials() {
        let cfg = PgReplicatorConfig::from_url("postgres://localhost/mydb", "s", "p").unwrap();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5432);
        assert_eq!(cfg.user, "postgres");
        assert_eq!(cfg.database, "mydb");
    }

    #[test]
    fn rejects_non_postgres_url() {
        assert!(PgReplicatorConfig::from_url("http://x", "s", "p").is_err());
    }

    #[test]
    fn json_escape_handles_special_chars() {
        let mut out = String::new();
        json_escape_into(&mut out, "a\"b\\c\n");
        assert_eq!(out, "a\\\"b\\\\c\\n");
    }

    #[test]
    fn tuple_renders_to_typed_json_payload() {
        use pgoutput::events::base::tuple_data::TupleDataColumn;
        let meta = RelationMeta {
            qualified_name: "tasks".into(),
            pk_indices: vec![0],
            columns: vec![
                ("id".into(), 2950),     // uuid -> string
                ("title".into(), 25),    // text -> string (passthrough)
                ("priority".into(), 23), // int4 -> number
                ("done".into(), 16),     // bool -> bool
            ],
        };
        let data: TupleData<BinaryValueTraitOff> = vec![
            TupleDataColumn::Value("123e4567-e89b-12d3-a456-426614174000".to_string()),
            TupleDataColumn::Value("hello".to_string()),
            TupleDataColumn::Value("7".to_string()),
            TupleDataColumn::Value("t".to_string()),
        ];
        let payload = String::from_utf8(tuple_to_json_payload(&meta, &data, None)).unwrap();
        assert_eq!(
            payload,
            "{\"id\":\"123e4567-e89b-12d3-a456-426614174000\",\"title\":\"hello\",\"priority\":7,\"done\":true}"
        );
    }

    #[test]
    fn tuple_renders_null_and_toasted_placeholder() {
        use pgoutput::events::base::tuple_data::TupleDataColumn;
        let meta = RelationMeta {
            qualified_name: "tasks".into(),
            pk_indices: vec![0],
            columns: vec![("id".into(), 25), ("body".into(), 25)],
        };
        // PGNull -> JSON null (never a fabricated value); toasted-unchanged ->
        // the "" placeholder (see the ponytail on `tuple_to_json_payload`).
        let data: TupleData<BinaryValueTraitOff> = vec![
            TupleDataColumn::PGNull,
            TupleDataColumn::PGUnchangedToastedValue,
        ];
        let payload = String::from_utf8(tuple_to_json_payload(&meta, &data, None)).unwrap();
        assert_eq!(payload, "{\"id\":null,\"body\":\"\"}".to_string());
    }

    #[test]
    fn toasted_marker_substitutes_the_old_tuple_value() {
        use pgoutput::events::base::tuple_data::TupleDataColumn;
        let meta = RelationMeta {
            qualified_name: "tasks".into(),
            pk_indices: vec![0],
            columns: vec![("id".into(), 25), ("body".into(), 25)],
        };
        // UPDATE touching only a small column: the toasted body arrives as
        // the u marker in the NEW tuple; REPLICA IDENTITY FULLs OLD tuple
        // carries the real (>2KB) value, which is definitionally unchanged.
        let new_data: TupleData<BinaryValueTraitOff> = vec![
            TupleDataColumn::Value("row-1".to_string()),
            TupleDataColumn::PGUnchangedToastedValue,
        ];
        let old_data: TupleData<BinaryValueTraitOff> = vec![
            TupleDataColumn::Value("row-1".to_string()),
            TupleDataColumn::Value("x".repeat(4096)),
        ];
        let payload =
            String::from_utf8(tuple_to_json_payload(&meta, &new_data, Some(&old_data))).unwrap();
        assert_eq!(
            payload,
            format!("{{\"id\":\"row-1\",\"body\":\"{}\"}}", "x".repeat(4096)),
            "the toasted column keeps its real value, never the empty clobber"
        );
    }
}
