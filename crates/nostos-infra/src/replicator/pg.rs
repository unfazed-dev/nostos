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
//! (ADR-0009). WAL-bloat protection for a permanently-silent client
//! (`max_slot_wal_keep_size` / age-based advance) is deferred — see ADR-0016.
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

use nostos_application::ports::ReplicatorStream;
use nostos_domain::{Lsn, ReplicationEvent as NostosEvent, RowOp};

/// The concrete pgoutput monomorphization we use everywhere in this module:
/// text values (no binary), no streaming large txns.
type Event = PgEvent<BinaryValueTraitOff, StreamingValueTraitOff>;

/// Cached relation metadata, keyed by the OID pgoutput sends with each row op.
///
/// pgoutput sends a `Relation` message once per relation (table) before the
/// first change to it, then refers to rows by OID. We must remember the
/// OID→table-name mapping (and the PK column positions) to decode row ops.
#[derive(Debug, Clone)]
struct RelationMeta {
    /// `<namespace>.<name>`, e.g. `public.tasks`. We strip the `public.` prefix
    /// on emit so predicates (which use bare table names like `tasks`) match.
    qualified_name: String,
    /// Indices of columns flagged as part of the replica identity / primary key.
    /// Used to extract a string PK for the `RowOp`.
    pk_indices: Vec<usize>,
    /// Column names in tuple order — used to build the JSON-ish payload.
    columns: Vec<String>,
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
        }
    }

    /// Ensure the replication slot + publication exist and the stream is open.
    ///
    /// Idempotent: safe to call on every reconnect. Uses a plain SQL control
    /// connection (`tokio_postgres`) to run the DDL, then opens the replication
    /// connection (`pgwire_replication`).
    ///
    /// # Errors
    /// Connection or SQL errors bubble up as [`PgReplicatorError`].
    pub async fn ensure_connected(&mut self) -> Result<(), PgReplicatorError> {
        // 1. Control-plane connection: ensure publication + slot, resolve start LSN.
        let start_lsn = self.ensure_slot_and_publication().await?;

        // 2. Replication connection.
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

    /// Pre-seed `relations` from `pg_class`/`pg_attribute` so row decoding works
    /// without waiting for (or relying on) a stream Relation message.
    ///
    /// For each table in the publication we capture: oid, namespace-qualified
    /// name, column names in order, and the PK column indices. Stream Relation
    /// messages later just refresh this — but having it up-front means a fresh
    /// replication connection to an existing slot decodes rows immediately.
    async fn bootstrap_relations_from_catalog(&mut self, sql: &tokio_postgres::Client) {
        // All columns in publication order, with attnum + PK flag.
        // PK detection: pg_index.indkey is an int2vector; we test membership of
        // each column's attnum against the primary-key index's indkey via = ANY.
        // Cast oid::int so tokio-postgres deserializes it as a plain i32.
        let rows = match sql
            .query(
                "SELECT c.oid::int, n.nspname, c.relname, a.attname, \
                        (a.attnum = ANY (coalesce(i.indkey::int2[], ARRAY[]::int2[]))) AS is_pk \
                 FROM pg_publication_tables pt \
                 JOIN pg_class c ON c.relname = pt.tablename \
                 JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = pt.schemaname \
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
                 WHERE pt.pubname = $1 \
                 ORDER BY c.oid, a.attnum",
                &[&self.cfg.publication],
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "could not bootstrap relations from catalog; relying on stream Relation messages");
                return;
            }
        };

        // Group rows by oid (sorted by oid, then attnum).
        let mut by_oid: std::collections::BTreeMap<i32, (String, Vec<(String, bool)>)> =
            std::collections::BTreeMap::new();
        for row in rows {
            let oid: i32 = row.get(0);
            let nsp: String = row.get(1);
            let rel: String = row.get(2);
            let attname: String = row.get(3);
            let is_pk: bool = row.get(4);
            let qualified = if nsp == "public" || nsp.is_empty() {
                rel
            } else {
                format!("{nsp}.{rel}")
            };
            let entry = by_oid.entry(oid).or_insert_with(|| (qualified, Vec::new()));
            entry.1.push((attname, is_pk));
        }

        for (oid, (qualified, cols)) in by_oid {
            let columns: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
            let pk_indices: Vec<usize> = cols
                .iter()
                .enumerate()
                .filter(|(_, (_, is_pk))| *is_pk)
                .map(|(i, _)| i)
                .collect();
            let pk_indices = if pk_indices.is_empty() {
                vec![0]
            } else {
                pk_indices
            };
            self.relations.insert(
                oid,
                RelationMeta {
                    qualified_name: qualified,
                    pk_indices,
                    columns,
                },
            );
        }
        debug!(
            relations = self.relations.len(),
            "bootstrapped relations from catalog"
        );
    }

    /// Create publication + slot if absent, resolve the start LSN.
    ///
    /// Preference: explicit `cfg.start_lsn` → slot's `confirmed_flush_lsn` →
    /// slot's `restart_lsn` → `pg_current_wal_lsn()`. This is the exact
    /// pattern from `pgwire-replication`'s own `checkpointed` example.
    async fn ensure_slot_and_publication(&mut self) -> Result<PgLsn, PgReplicatorError> {
        let dsn = format!(
            "host={} port={} user={} password={} dbname={}",
            self.cfg.host, self.cfg.port, self.cfg.user, self.cfg.password, self.cfg.database
        );
        let (sql, conn) = tokio_postgres::connect(&dsn, NoTls)
            .await
            .map_err(|e| PgReplicatorError::ControlPlane(e.to_string()))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        // Publication (best-effort — may already exist).
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

        // Slot (best-effort — may already exist).
        if let Err(e) = sql
            .batch_execute(&format!(
                "SELECT * FROM pg_create_logical_replication_slot('{}', 'pgoutput');",
                self.cfg.slot
            ))
            .await
        {
            debug!(error = %e, "could not create slot (may already exist)");
        }

        // Explicit start LSN wins.
        if let Some(explicit) = self.cfg.start_lsn {
            self.seed_resume(explicit);
            return Ok(PgLsn::from_u64(explicit.raw()));
        }

        // Otherwise resume from the slot's last confirmed flush.
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

        // Fresh slot: start from the current WAL head.
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
                // ponytail: xid is i32 from pg; clamp negatives to 0 (they never
                // occur for real transactions). u32::try_from is infallible after max(0).
                self.current_txn = Some(u64::from(
                    u32::try_from(begin.transaction_id.max(0)).unwrap_or(0),
                ));
                None
            }
            BaseEvent::Commit(_) => {
                self.current_txn = None;
                None
            }
            BaseEvent::Insert(ins) => {
                self.full_row_op(ins.oid, &ins.data, nostos_domain::Operation::Insert, lsn)
            }
            BaseEvent::Update(upd) => {
                self.full_row_op(upd.oid, &upd.data, nostos_domain::Operation::Update, lsn)
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
        // PK = columns whose flags bit 0 (REPLICA_IDENTITY) is set, OR the first
        // column if none are flagged (defensive default).
        let columns: Vec<String> = rel.columns.iter().map(|c| c.name.clone()).collect();
        let pk_indices: Vec<usize> = rel
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.flags & 0x1 != 0)
            .map(|(i, _)| i)
            .collect();
        let pk_indices = if pk_indices.is_empty() {
            vec![0]
        } else {
            pk_indices
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
        op: nostos_domain::Operation,
        lsn: Lsn,
    ) -> Option<NostosEvent> {
        let meta = self.relations.get(&oid)?;
        let table = meta.qualified_name.clone();
        let pk = pk_string(meta, data);
        let payload = Bytes::from(tuple_to_json_payload(meta, data));
        let row = match op {
            nostos_domain::Operation::Insert => RowOp::Insert { table, pk, payload },
            nostos_domain::Operation::Update => RowOp::Update { table, pk, payload },
            nostos_domain::Operation::Delete => {
                // Deletes have no full tuple — shouldn't reach here.
                RowOp::Delete { table, pk }
            }
        };
        Some(self.stamp(row, lsn))
    }

    /// Build a `RowOp::Delete` from just the PK/old-key tuple.
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
        let pk = match old {
            Some(
                pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::PrimaryKeyTupleData(tuple)
                | pgoutput::events::base::tuple_data::OldDataOrPrimaryKeyTupleData::OldTupleData(tuple),
            ) => pk_string(meta, tuple),
            None => "0".to_string(),
        };
        Some(self.stamp(RowOp::Delete { table, pk }, lsn))
    }
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

/// Render a tuple as a small JSON object `{ "col": "value", ... }` for the
/// payload. This keeps the payload debuggable and (later) lets the wire codec
/// and predicate extractor pull named columns out without re-parsing pgoutput.
fn tuple_to_json_payload(meta: &RelationMeta, data: &TupleData<BinaryValueTraitOff>) -> Vec<u8> {
    // Manual JSON build to avoid a serde_json dependency in this hot path;
    // tuples are small (a handful of columns).
    let mut out = String::from('{');
    for (i, col) in meta.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, col);
        out.push_str("\":\"");
        if let Some(TupleDataColumn::Value(v)) = data.get(i) {
            json_escape_into(&mut out, v);
        }
        out.push('"');
    }
    out.push('}');
    out.into_bytes()
}

/// Minimal JSON string escaping (in-place append).
fn json_escape_into(out: &mut String, s: &str) {
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
                    error!(error = %e, "PgReplicator error; will reconnect after backoff");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libpq_url_with_credentials() {
        let cfg = PgReplicatorConfig::from_url(
            "postgresql://cairn:cairn@localhost:5433/cairn",
            "cairn_slot",
            "cairn_pub",
        )
        .unwrap();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5433);
        assert_eq!(cfg.user, "cairn");
        assert_eq!(cfg.password, "cairn");
        assert_eq!(cfg.database, "cairn");
        assert_eq!(cfg.slot, "cairn_slot");
        assert_eq!(cfg.publication, "cairn_pub");
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
    fn tuple_renders_to_json_payload() {
        use pgoutput::events::base::tuple_data::TupleDataColumn;
        let meta = RelationMeta {
            qualified_name: "tasks".into(),
            pk_indices: vec![0],
            columns: vec!["id".into(), "title".into()],
        };
        let data: TupleData<BinaryValueTraitOff> = vec![
            TupleDataColumn::Value("abc".to_string()),
            TupleDataColumn::Value("hello".to_string()),
        ];
        let payload = String::from_utf8(tuple_to_json_payload(&meta, &data)).unwrap();
        assert_eq!(payload, "{\"id\":\"abc\",\"title\":\"hello\"}");
    }
}
