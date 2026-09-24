#[cfg(doc)]
use super::scope::SubscribeRejection;
use super::scope::{build_predicate, build_stream_predicate, replay_admits};
use super::session::SubscribeRequest;
use super::MAX_TABLES_PER_SOCKET;
use crate::router::TokioEventSink;
use crate::wire::{
    encode_snapshot_boundary, encode_snapshot_boundary_for_stream, encode_stream_error,
};
use nostos_application::ports::{EventSink, OpLogSource, SnapshotSource};
use nostos_application::{ActiveRuleset, SessionManager};
use nostos_domain::{compose_sync_epoch, ColumnValue, Principal, SessionId, SyncSession};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Why a subscribe was rejected. Non-fatal for mid-session subscribes (the
/// socket keeps serving its existing tables); FATAL for the first subscribe
/// (the socket is closed — see `run_session`).
#[derive(Debug)]
pub(super) enum SubscribeReject {
    /// The predicate could not be built — rules denial (`NotSynced`/
    /// `MissingClaim`, ADR-0031) or a `where_sql` compile failure (ADR-0012).
    /// Carries [`SubscribeRejection`]'s rendered message.
    Rejected(String),
    /// Per-socket table cap exceeded (`MAX_TABLES_PER_SOCKET`) — DoS guard.
    CapExceeded,
    /// Global concurrent-device cap reached (`SessionManager`).
    DeviceCapReached,
}

/// Per-socket multi-table subscription state (D1/ADR-0022). One socket owns
/// ONE shared `TokioEventSink` (one channel, one `acked_lsn`, one writer task —
/// ADR-0009's single global checkpoint) and N single-predicate `SyncSession`s
/// registered against it. `candidates_for` is table-indexed, so each session
/// receives only its own table's events; `min_acked_lsn` folds the shared
/// sink's single `last_acked_lsn` across the N sessions (= the same value N
/// times = the socket's checkpoint).
///
/// `synthetic_cursor` is the load-bearing correctness fix for multi-table
/// snapshot-on-subscribe: `PgSnapshotter` stamps snapshot LSNs as `base+1+i`
/// PER TABLE (snapshot_source.rs), so on a shared sink table B's snapshot LSN
/// range collides with table A's and the sink's dedup ring (router.rs) drops it
/// as duplicates. The cursor is seeded from `resume_lsn` and advanced by each
/// snapshot's row count, so every event across all tables gets a distinct LSN.
pub(super) struct SocketSubs {
    /// Every registered session id (one per subscribed table) — disconnected
    /// en masse when the socket closes.
    pub(super) ids: Vec<SessionId>,
    /// Subscribed table names — drives the per-socket cap + idempotent repeat.
    pub(super) tables: HashSet<String>,
    /// Monotonic snapshot-LSN allocator; passed as `base_lsn` to each snapshot.
    pub(super) synthetic_cursor: u64,
    /// Active sync streams by client-chosen id (P5). A separate namespace
    /// from `tables`, but counted TOGETHER against `MAX_TABLES_PER_SOCKET`
    /// (each lazy add = one snapshot SELECT — design §2 Caps). `name`/`table`
    /// are retained so the rules-reload check (ADR-0031 D3) detects a
    /// stream-definition or table-decision change under a live socket.
    pub(super) streams: HashMap<String, StreamSub>,
}

/// One active sync stream on a socket (P5).
pub(super) struct StreamSub {
    pub(super) session: SessionId,
    pub(super) name: String,
    pub(super) table: String,
}

/// The `(epoch, rules_checksum)` pair to advertise on `resume_info` for a
/// subscribe (ADR-0031 D2). Shared by the advertise site (`run_session`) and
/// `register_subscribe`'s resume gate so the two can never drift apart: what
/// a client is told to persist is byte-for-byte what its next resume is
/// judged against.
pub(super) fn resume_advertisement(
    client_sent_checksum: bool,
    server_epoch: u64,
    rules_checksum: u64,
) -> (u64, Option<u64>) {
    if client_sent_checksum {
        (server_epoch, Some(rules_checksum))
    } else {
        (compose_sync_epoch(server_epoch, rules_checksum), None)
    }
}

/// Register one table subscription on the socket's shared sink: predicate
/// build, per-socket cap + idempotency checks, `SessionManager::connect`, and
/// snapshot-on-subscribe. Called for the first subscribe (pre-split, in
/// `run_session`) and every subsequent one (post-split, in the reader task).
/// Returns `Err` WITHOUT registering on any rejection. Critical sections on
/// `subs` are short and never span an `.await`; access is serialized anyway
/// (one reader task; the first subscribe runs before the reader is spawned).
#[allow(clippy::too_many_arguments)] // 10 params is the genuine subscribe surface; a param-struct would obscure the call sites.
pub(super) async fn register_subscribe(
    req: &SubscribeRequest,
    subs: &Arc<Mutex<SocketSubs>>,
    manager: &Arc<SessionManager>,
    snapshotter: Option<&Arc<dyn SnapshotSource>>,
    server_epoch: u64,
    oplog_reader: Option<&Arc<dyn OpLogSource>>,
    sink_concrete: &Arc<TokioEventSink>,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<(), SubscribeReject> {
    // Cap + idempotent-repeat check (short lock, no await).
    {
        let s = subs.lock().await;
        if s.tables.contains(&req.table) {
            debug!(table = %req.table, "subscribe for already-subscribed table: no-op");
            return Ok(());
        }
        if s.tables.len() >= MAX_TABLES_PER_SOCKET {
            return Err(SubscribeReject::CapExceeded);
        }
    }

    let predicate = build_predicate(req, principal, tenant_column, ruleset)
        .map_err(|rejection| SubscribeReject::Rejected(rejection.to_string()))?;
    // The op-log replay gate needs the same predicate the live path filters
    // on; `predicate` is moved into the session on the next line.
    let replay_pred = predicate.clone();
    let session = SyncSession::new_authenticated(predicate, principal.clone());
    // Derive the type-erased clone the store holds; `sink_concrete` stays the
    // concrete handle for snapshot delivery below.
    let sink_dyn: Arc<dyn EventSink> = Arc::clone(sink_concrete) as Arc<dyn EventSink>;
    let id = manager
        .connect(session, sink_dyn)
        .await
        .map_err(|e| match e {
            // Distinct rejections: the deployment being full and ONE account
            // being at its own ceiling are different operator problems, and
            // collapsing them hides which one is happening (audit finding 2).
            nostos_application::session::ConnectError::PrincipalCapReached { .. } => {
                SubscribeReject::Rejected(e.to_string())
            }
            nostos_application::session::ConnectError::DeviceCapReached { .. } => {
                SubscribeReject::DeviceCapReached
            }
        })?;

    // ── Op-log replay-on-reconnect (ADR-0025 slice 4b). When the client's
    //    epoch matches the server's current slot epoch AND its resume_lsn is
    //    within the retained op-log window, replay the offline gap from
    //    `cairn_oplog` to the fresh sink and SKIP the snapshot. The client
    //    dedups per-row by lsn (slice 4a), so the concurrent live fan-out +
    //    replay overlap is safe. Live fan-out started at `manager.connect`
    //    above. Any decline (epoch mismatch, aged-out resume, empty/failed
    //    replay, no reader) falls through to the snapshot path below — slice-1
    //    reconcile is the correctness floor.
    let client_epoch = req.client_epoch.unwrap_or(0);
    // ADR-0031 D2: a D2 client sent `rules_checksum` on Subscribe, so its
    // epoch and checksum are compared independently against the raw
    // `server_epoch` and the active ruleset's checksum — same slot epoch but
    // a rules edit still forces a snapshot.
    //
    // ponytail: pre-D2 clients get the rules checksum folded into the
    // advertised epoch, so their logs cannot distinguish a slot recreate from
    // a rules edit. Ceiling: log-level attribution only for old clients.
    // Upgrade path: drop the fallback once the SDK floor is D2-or-newer.
    let (want_epoch, want_checksum) = resume_advertisement(
        req.client_rules_checksum.is_some(),
        server_epoch,
        ruleset.checksum(),
    );
    let epoch_and_checksum_match =
        client_epoch == want_epoch && req.client_rules_checksum == want_checksum;
    if epoch_and_checksum_match && !req.table.is_empty() {
        if let (Some(reader), Some(resume)) = (oplog_reader, req.resume_lsn) {
            let in_window = matches!(reader.window_tail().await, Ok(tail) if resume >= tail);
            if in_window {
                match reader
                    .replay_after(principal.tenant_id.as_str(), resume)
                    .await
                {
                    Ok(events) => {
                        let total = events.len();
                        let mut count = 0_usize;
                        for ev in events {
                            // AUTHORIZATION, not an optimization: `cairn_oplog`
                            // is keyed by tenant ALONE, so without this gate a
                            // resume widens scope past the ruleset that the
                            // live path enforces (see `replay_admits`).
                            if !replay_admits(&replay_pred, &ev) {
                                continue;
                            }
                            // Backpressure-aware (slice-1): the bounded sink
                            // mustn't truncate the replay. Live `deliver` +
                            // replay `deliver_awaiting` share the FIFO channel;
                            // slice-4a's per-row lsn gate dedups the overlap.
                            let _ = sink_concrete.deliver_awaiting(ev).await;
                            count += 1;
                        }
                        // Count AFTER filtering: a replay that was non-empty but
                        // filtered to nothing must still fall through to the
                        // snapshot, or the client gets neither and silently
                        // keeps a gap.
                        if count > 0 {
                            debug!(
                                table = %req.table, resume, count, total,
                                "op-log replay delivered (epoch match, in-window); skipping snapshot"
                            );
                            // Record the session on the socket (same bookkeeping
                            // as the snapshot path's tail, minus the synthetic-
                            // cursor advance — replay events carry REAL lsns,
                            // not synthetic ones, so they don't consume the
                            // cursor's space).
                            {
                                let mut s = subs.lock().await;
                                s.ids.push(id);
                                s.tables.insert(req.table.clone());
                            }
                            return Ok(());
                        }
                        debug!(
                            table = %req.table, resume, total,
                            "op-log replay held nothing this session may see; falling back to snapshot"
                        );
                    }
                    Err(e) => warn!(
                        table = %req.table, error = %e,
                        "op-log replay failed; falling back to snapshot"
                    ),
                }
            } else {
                debug!(
                    table = %req.table, resume,
                    "resume_lsn aged out of op-log window; snapshot"
                );
            }
        }
    }

    // Snapshot-on-subscribe for THIS table only. base_lsn is the socket's
    // frontier-aware synthetic cursor (see `snapshot_base_lsn`) so cross-table
    // snapshot LSN ranges never collide on the shared sink's dedup ring AND a
    // mid-session subscribe after acked live traffic is not dropped by the
    // sink's own ack gate. A failed snapshot is non-fatal: the client still
    // gets live fan-out.
    let snapshot_base = {
        let s = subs.lock().await;
        snapshot_base_lsn(s.synthetic_cursor, sink_concrete)
    };
    let delivered = if let Some(snap) = snapshotter {
        match snap
            .snapshot(
                &req.table,
                nostos_domain::Lsn::new(snapshot_base),
                // Tenant-scoped exactly like the live read path: the same
                // `Principal::tenant_scope` seam, so a multi-tenant
                // subscriber's snapshot can never widen past its tenant
                // (audit 2026-08-17: the unscoped call leaked every
                // tenant's rows on subscribe).
                principal.tenant_scope(tenant_column),
            )
            .await
        {
            Ok(events) => {
                // Snapshot-reconcile boundary (ADR-0014 offline-delete fix):
                // bracket the snapshot's rows with begin/end control frames so
                // the client can reap local PKs absent from the snapshot (rows
                // hard-deleted server-side while the client was offline). The
                // boundaries travel the SAME FIFO channel as the rows
                // (`sink_concrete` → writer → WS) so the writer can't reorder
                // them relative to the rows (ADR-0025 hole #2: two channels let
                // the writer's `select!` land begin after early rows → those
                // rows never drain → reaped at end). A full sink buffer drops
                // the boundary (best-effort, like a write-ack): the client keeps
                // its stale rows but stays consistent (no partial reconcile).
                let _ = sink_concrete.deliver_control(encode_snapshot_boundary(&req.table, true));
                let count = events.len();
                for ev in events {
                    // ADR-0025 residual: backpressure-aware delivery so the
                    // snapshot is never truncated by sink backpressure (a
                    // dropped row would let `end` reap a pk the server still
                    // has). Live fan-out keeps best-effort `deliver`.
                    let _ = sink_concrete.deliver_awaiting(ev).await;
                }
                let _ = sink_concrete.deliver_control(encode_snapshot_boundary(&req.table, false));
                debug!(table = %req.table, count, "snapshot-on-subscribe delivered");
                count
            }
            // A table over the snapshotter's row cap is REFUSED, not quietly
            // shortened. Every other snapshot error keeps the historical
            // warn-and-continue (the client still gets live fan-out), but a
            // cap breach is a configuration mistake the operator must see:
            // continuing here would hand the client a short first sync that
            // is indistinguishable from a complete one — the same shape as
            // the stream-snapshot scope bypass (audit finding 7).
            Err(e @ nostos_application::ports::SnapshotError::TooLarge { .. }) => {
                manager.disconnect(id).await;
                return Err(SubscribeReject::Rejected(e.to_string()));
            }
            Err(e) => {
                warn!(
                    table = %req.table, error = %e,
                    "snapshot-on-subscribe failed; continuing with live fan-out"
                );
                0
            }
        }
    } else {
        0
    };

    // Advance the cursor past the rows we just delivered + record the session.
    // The base may have come from the sink FRONTIER (above the old cursor), so
    // set — don't increment.
    {
        let mut s = subs.lock().await;
        s.synthetic_cursor = snapshot_base.saturating_add(delivered as u64);
        s.ids.push(id);
        s.tables.insert(req.table.clone());
    }
    Ok(())
}

/// Convert a `subscribe_stream` `params` object into typed bind values (P5
/// §1). JSON scalars map to the obvious leaf; null/array/object params are
/// rejected — a stream param is always a scalar (the grammar's literal
/// position has no composite shape).
fn stream_params_to_values(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<HashMap<String, ColumnValue>, String> {
    let mut out = HashMap::with_capacity(params.len());
    for (k, v) in params {
        let value = match v {
            serde_json::Value::String(s) => ColumnValue::text(s),
            serde_json::Value::Bool(b) => ColumnValue::boolean(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    ColumnValue::number(i)
                } else {
                    // u64 beyond i64::MAX or a float literal: f64 is the
                    // common numeric supertype. Precision loss for huge u64s
                    // is JSON's documented reality, not a nostos choice.
                    ColumnValue::float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            _ => return Err(format!("param '{k}' must be a string, number, or boolean")),
        };
        out.insert(k.clone(), value);
    }
    Ok(out)
}

/// Register a sync stream on this socket (P5 design §2): look up the
/// server-defined stream, bind its `:param` placeholders value-level (never
/// textual — Decision 2), wrap rules+tenant exactly like a table subscribe
/// (Decision 3), register ONE `SyncSession` per stream instance via
/// `SessionManager::connect` (same store/sink path as `register_subscribe`),
/// then take a targeted per-stream snapshot (§3) delivered on the same FIFO
/// channel with the stream id on the boundary frames.
///
/// EVERY reject is non-fatal: the caller maps `Err` into a `stream_error`
/// frame and the socket stays up (design §1). Idempotent `id` reuse retires
/// the old session AFTER the new one registers — both live briefly on the
/// same table shard and the sink's LSN dedup ring tolerates the overlap.
#[allow(clippy::too_many_arguments)] // same genuine subscribe surface as register_subscribe
pub(super) async fn register_stream(
    id: &str,
    name: &str,
    params: &serde_json::Map<String, serde_json::Value>,
    subs: &Arc<Mutex<SocketSubs>>,
    manager: &Arc<SessionManager>,
    snapshotter: Option<&Arc<dyn SnapshotSource>>,
    sink_concrete: &Arc<TokioEventSink>,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<(), String> {
    let stream = ruleset
        .stream(name)
        .ok_or_else(|| format!("unknown stream '{name}'"))?;
    let values = stream_params_to_values(params)?;
    let bound = nostos_domain::predicate_compile::bind_params(&stream.template, &values)
        .map_err(|e| format!("stream '{name}': {e}"))?;
    let table = stream.table.clone();

    // Cap + idempotent-replace check (short lock, no await — mirrors
    // register_subscribe).
    let replaced = {
        let s = subs.lock().await;
        let replacing = s
            .streams
            .get(id)
            .map(|old| (old.session, old.table.clone()));
        if replacing.is_none() && s.tables.len() + s.streams.len() >= MAX_TABLES_PER_SOCKET {
            return Err(format!(
                "per-socket subscription cap ({MAX_TABLES_PER_SOCKET}) reached"
            ));
        }
        replacing
    };

    // `snapshot_expr` is `rules ∧ bound` — the SAME ruleset scope live fan-out
    // enforces. Passing the raw `bound` to the snapshot instead was audit
    // finding 7: the stream's first sync skipped the ruleset's row scope while
    // every subsequent live row honoured it. `bound` is MOVED here (no clone):
    // the snapshot must not have a second, unscoped copy to reach for.
    let (predicate, snapshot_expr) =
        build_stream_predicate(&table, bound, principal, tenant_column, ruleset)
            .map_err(|rejection| rejection.to_string())?;
    let session = SyncSession::new_authenticated(predicate, principal.clone());
    let sink_dyn: Arc<dyn EventSink> = Arc::clone(sink_concrete) as Arc<dyn EventSink>;
    let session_id = manager
        .connect(session, sink_dyn)
        .await
        .map_err(|e| e.to_string())?;

    if let Some((old_session, _)) = replaced {
        manager.disconnect(old_session).await;
    }

    // Live fan-out starts at registration, BEFORE the snapshot query (design
    // §3): the client's per-row LSN gate + idempotent upsert make the overlap
    // safe — the same argument as op-log replay. The base is frontier-aware
    // (`snapshot_base_lsn`) so a stream added after acked live traffic is not
    // dropped by the sink's own ack gate.
    let snapshot_base = {
        let s = subs.lock().await;
        snapshot_base_lsn(s.synthetic_cursor, sink_concrete)
    };
    let delivered = if let Some(snap) = snapshotter {
        match snap
            .snapshot_stream(
                &table,
                &snapshot_expr,
                nostos_domain::Lsn::new(snapshot_base),
                principal.tenant_scope(tenant_column),
            )
            .await
        {
            Ok(events) => {
                // Same FIFO discipline as table snapshots (ADR-0025 hole #2),
                // with the stream id on the boundary frames (wire §1).
                let _ = sink_concrete
                    .deliver_control(encode_snapshot_boundary_for_stream(&table, id, true));
                let count = events.len();
                for ev in events {
                    // Backpressure-aware: a dropped snapshot row would let the
                    // client's `end` reap a pk the server still has.
                    let _ = sink_concrete.deliver_awaiting(ev).await;
                }
                let _ = sink_concrete
                    .deliver_control(encode_snapshot_boundary_for_stream(&table, id, false));
                debug!(table = %table, stream = %id, count, "stream snapshot delivered");
                count
            }
            // Cap breach is loud on the stream path too — but here the wire
            // already has a per-stream error frame, so the stream is torn
            // down and the client is told which stream died and why, rather
            // than being left with a silently short one.
            Err(e @ nostos_application::ports::SnapshotError::TooLarge { .. }) => {
                let _ = sink_concrete.deliver_control(encode_stream_error(id, &e.to_string()));
                manager.disconnect(session_id).await;
                return Ok(());
            }
            Err(e) => {
                // A failed stream snapshot is non-fatal (design §3).
                warn!(table = %table, stream = %id, error = %e,
                    "stream snapshot failed; continuing with live fan-out");
                0
            }
        }
    } else {
        0
    };

    {
        let mut s = subs.lock().await;
        // Set (not increment): the base may have come from the sink frontier.
        s.synthetic_cursor = snapshot_base.saturating_add(delivered as u64);
        s.streams.insert(
            id.to_string(),
            StreamSub {
                session: session_id,
                name: name.to_string(),
                table,
            },
        );
    }
    Ok(())
}

/// Drop a stream by its client-chosen id (P5 §1). Unknown id = idempotent
/// no-op. v1 leaves local rows in place — eviction is separate.
pub(super) async fn unregister_stream(
    id: &str,
    subs: &Arc<Mutex<SocketSubs>>,
    manager: &Arc<SessionManager>,
) {
    let removed = { subs.lock().await.streams.remove(id) };
    if let Some(sub) = removed {
        // Remove the stream's session from the store — fan-out resolves
        // candidates from the store per event, so removal stops delivery for
        // every event processed after this point. Do NOT close the sink: the
        // stream session shares the socket-wide `TokioEventSink` with the base
        // subscriptions and every other stream (`register_stream` clones
        // `sink_concrete`), so `sink.close()` here killed ALL later delivery
        // on the socket — including a re-registered stream's own snapshot
        // (caught by the P5 demo: phase-5 re-parameterize delivered 0 rows
        // after phase-4 unsubscribe).
        manager.disconnect(sub.session).await;
        debug!(stream = %id, table = %sub.table, session = %sub.session, "stream unsubscribed");
    } else {
        debug!(stream = %id, "unsubscribe_stream for unknown id: no-op");
    }
}

/// The base LSN for a mid-session snapshot on a LIVE socket (ADR-0022
/// multi-table + P5 sync streams): `max(synthetic cursor, acked, delivered)`.
/// The sink's `admit` gate drops any frame at or below the acked cursor and
/// the dedup ring drops exact re-deliveries — so a snapshot stamped from a
/// STALE synthetic cursor after live traffic was acked would be silently
/// dropped by the server itself before the client ever saw it (caught by the
/// P5 demo: a lazy stream add after live rows had acked delivered zero rows).
/// Stamping above the socket frontier keeps every snapshot row deliverable.
/// The narrow synthetic-vs-WAL collision window documented in
/// `snapshot_source.rs` (resuming-client ponytail) applies here unchanged —
/// this is exactly the resuming-client shape.
fn snapshot_base_lsn(subs_cursor: u64, sink: &TokioEventSink) -> u64 {
    let acked = sink
        .last_acked_lsn()
        .map_or(0, nostos_application::Lsn::raw);
    let delivered = sink
        .last_delivered_lsn()
        .map_or(0, nostos_application::Lsn::raw);
    subs_cursor.max(acked).max(delivered)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{req, stream_ruleset, toggles_rules};
    use super::*;
    use bytes::Bytes;
    use nostos_application::ports::SessionStore;
    use nostos_domain::{Lsn, RowOp};
    use nostos_domain::{PredicateExpr, ReplicationEvent, SyncMode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// A canned op-log reader for the reconnect-resume branch tests (ADR-0025
    /// slice 4b). `replay_calls` distinguishes "replay attempted + empty"
    /// (case e) from "replay never reached" (b, c, d).
    struct MockOpLog {
        events: Vec<ReplicationEvent>,
        tail: u64,
        replay_calls: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl nostos_application::ports::OpLogSource for MockOpLog {
        async fn replay_after(
            &self,
            _tenant: &str,
            _after: u64,
        ) -> Result<Vec<ReplicationEvent>, nostos_application::ports::OpLogError> {
            self.replay_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.events.clone())
        }
        async fn window_tail(&self) -> Result<u64, nostos_application::ports::OpLogError> {
            Ok(self.tail)
        }
    }

    /// Payload must be decodable JSON: `cairn_oplog` stores the row image as
    /// JSONB, and `replay_admits` fails an undecodable payload CLOSED, so a
    /// non-JSON fixture would silently exercise the reject path.
    fn ev(lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "tasks".into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(br#"{"id":"x"}"#),
            },
        )
    }

    /// A replay event carrying a real JSON payload, so the predicate has
    /// columns to evaluate (the op log stores the row image as JSON).
    fn ev_json(table: &str, lsn: u64, json: &str) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.into(),
                pk: lsn.to_string(),
                payload: Bytes::copy_from_slice(json.as_bytes()),
            },
        )
    }

    /// Same as [`ev`] but on a caller-chosen table — the op log is keyed by
    /// tenant only, so a replay can surface rows from ANY table the tenant
    /// wrote, including ones this session never subscribed to.
    fn ev_on(table: &str, lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(br#"{"id":"x"}"#),
            },
        )
    }

    /// Build the register_subscribe harness. `snapshotter` is `None`, so the
    /// snapshot path delivers nothing — the replay-vs-snapshot observable is
    /// whether the sink received events (+ the replay-call counter).
    #[allow(clippy::unused_async)] // sync body; kept async so call sites read uniformly with the awaited setup.
    async fn harness() -> (
        Arc<Mutex<SocketSubs>>,
        Arc<SessionManager>,
        Arc<TokioEventSink>,
        crate::router::SinkReceiver,
    ) {
        let subs = Arc::new(Mutex::new(SocketSubs {
            ids: Vec::new(),
            tables: HashSet::new(),
            synthetic_cursor: 0,
            streams: HashMap::new(),
        }));
        let store: Arc<dyn SessionStore> = Arc::new(crate::store::InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let (sink, rx) = TokioEventSink::channel(16);
        (subs, manager, Arc::new(sink), rx)
    }

    // (a) epoch match + resume ≥ tail + non-empty replay → replay delivers.
    #[tokio::test]
    async fn replay_delivers_on_epoch_match_in_window() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        // Legacy (no rules_checksum) client: the gate compares the composed
        // fallback, so the request must carry the composed value to match.
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert!(rx.try_recv().is_err(), "replay delivered exactly one event");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    /// Replay must honor the SAME authorization the live path does.
    ///
    /// The live path filters every event through the session predicate
    /// (`FanOutService::fan_out` -> `predicate.matches`), which carries the
    /// ruleset scope AND the tenant clause. The replay path reads
    /// `cairn_oplog` keyed by tenant alone (`replay_after(tenant, lsn)`) and
    /// hands rows straight to the socket sink, whose `admit` gate checks only
    /// open/acked/dedup — never the predicate, never the table. So a reconnect
    /// can hand a client rows from a table its own ruleset refuses to sync: a
    /// direct `subscribe` to `notes` here is rejected `NotSynced`, but a
    /// `tasks` resume delivers it anyway.
    ///
    /// `calls == 1` is load-bearing: without it a broken epoch gate would skip
    /// replay entirely and the empty-sink assertion would pass vacuously.
    #[tokio::test]
    async fn replay_never_delivers_rows_from_an_unsynced_table() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        // The tenant's op log holds a `notes` row. `notes` is not in the
        // ruleset, so `decide` answers `DeniedTable` for it.
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev_on("notes", 10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let rules = ActiveRuleset::compile(&toggles_rules("tasks", true, None)).unwrap();
        register_subscribe(
            &SubscribeRequest {
                table: "tasks".into(),
                filters: Vec::new(),
                where_sql: None,
                resume_lsn: Some(5),
                client_epoch: Some(1),
                client_rules_checksum: Some(rules.checksum()),
            },
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &rules,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "replay must actually run, else this test proves nothing",
        );
        assert!(
            rx.try_recv().is_err(),
            "replay leaked a row from `notes` — a table this ruleset does not sync \
             and a direct subscribe would reject",
        );
    }

    /// Replay honors the ROW scope, not just the table name.
    ///
    /// Distinguishes a real fix from a table-only one: both stop the
    /// `notes` leak, but only re-applying the predicate stops a client whose
    /// ruleset says `status = 'open'` from receiving `status = 'closed'` rows
    /// on reconnect. `tenant_column` is `None` here, so the ONLY thing that can
    /// reject the closed row is the rules scope itself.
    ///
    /// The in-scope row must still arrive — a gate that dropped everything
    /// would also pass a "leaked nothing" assertion.
    #[tokio::test]
    async fn replay_applies_the_rules_scope_not_just_the_table() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![
                ev_json("tasks", 10, r#"{"status":"closed"}"#),
                ev_json("tasks", 11, r#"{"status":"open"}"#),
            ],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let rules =
            ActiveRuleset::compile(&toggles_rules("tasks", true, Some("status = 'open'"))).unwrap();
        register_subscribe(
            &SubscribeRequest {
                table: "tasks".into(),
                filters: Vec::new(),
                where_sql: None,
                resume_lsn: Some(5),
                client_epoch: Some(1),
                client_rules_checksum: Some(rules.checksum()),
            },
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &rules,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "replay must actually run, else this test proves nothing",
        );
        match rx.try_recv() {
            Ok(crate::router::SinkMsg::Event(e)) => assert_eq!(
                e.op.pk(),
                "11",
                "the in-scope (status=open) row is the one delivered",
            ),
            other => panic!("expected the in-scope row to be delivered, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "replay leaked a status='closed' row past a ruleset scoped to status='open'",
        );
    }

    // (b) epoch mismatch → snapshot (replay never reached).
    #[tokio::test]
    async fn snapshot_on_epoch_mismatch() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req("tasks", Some(1), Some(5)),
            &subs,
            &manager,
            None,
            2,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "snapshot path (snapshotter None) delivers nothing"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // (c) resume < tail (aged out of the op-log window) → snapshot.
    #[tokio::test]
    async fn snapshot_when_resume_aged_out() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 100, // resume 5 < tail 100 → aged out
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    // (d) no oplog reader wired → snapshot.
    #[tokio::test]
    async fn snapshot_when_no_reader() {
        let (subs, manager, sink, mut rx) = harness().await;
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            None,
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // (e) replay returns empty → fall back to snapshot (replay WAS attempted).
    #[tokio::test]
    async fn snapshot_when_replay_empty() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: Vec::new(),
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        register_subscribe(
            &req(
                "tasks",
                Some(compose_sync_epoch(1, ActiveRuleset::all_mode().checksum())),
                Some(5),
            ),
            &subs,
            &manager,
            None,
            1,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ActiveRuleset::all_mode(),
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "empty replay → snapshot, nothing delivered"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "replay was attempted, just empty"
        );
        assert!(subs.lock().await.tables.contains("tasks"));
    }

    // ADR-0031 D2 — the resume gate compares epoch and checksum
    // independently for a client that sent `rules_checksum` on Subscribe.

    // (f) D2 client, epoch AND checksum both match current server state →
    // replay (not snapshot).
    #[tokio::test]
    async fn d2_client_replays_when_epoch_and_checksum_match() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let ruleset = ActiveRuleset::all_mode();
        let req = SubscribeRequest {
            table: "tasks".into(),
            filters: Vec::new(),
            where_sql: None,
            resume_lsn: Some(5),
            client_epoch: Some(7),
            client_rules_checksum: Some(ruleset.checksum()),
        };
        register_subscribe(
            &req,
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // (g) D2 client, same slot epoch but the active ruleset checksum has
    // moved on (a rules edit, no slot recreate) → snapshot. This is the
    // entire point of D2: pre-D2 epoch-only comparison would have replayed.
    #[tokio::test]
    async fn d2_client_snapshots_when_only_checksum_differs() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let old_checksum = ActiveRuleset::all_mode().checksum();
        let current_ruleset = ActiveRuleset::compile(&toggles_rules("tasks", true, None)).unwrap();
        assert_ne!(
            old_checksum,
            current_ruleset.checksum(),
            "test needs two distinguishable rulesets"
        );
        let req = SubscribeRequest {
            table: "tasks".into(),
            filters: Vec::new(),
            where_sql: None,
            resume_lsn: Some(5),
            client_epoch: Some(7),
            client_rules_checksum: Some(old_checksum),
        };
        register_subscribe(
            &req,
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &current_ruleset,
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "checksum mismatch → snapshot, nothing delivered"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0, "replay never attempted");
    }

    // (h) legacy (pre-D2) client, no `rules_checksum` on the wire — the
    // composed-epoch fallback still forces a snapshot when the rules changed,
    // exactly as it would have folded a slot recreate in before D2.
    #[tokio::test]
    async fn legacy_client_snapshots_on_rules_change() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let current_ruleset = ActiveRuleset::compile(&toggles_rules("tasks", true, None)).unwrap();
        let stale_composed = compose_sync_epoch(7, ActiveRuleset::all_mode().checksum());
        register_subscribe(
            &req("tasks", Some(stale_composed), Some(5)),
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &current_ruleset,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    // (i) legacy client, nothing changed (same slot epoch, same rules) →
    // still replays — the fallback must not regress ADR-0025 for clients that
    // never adopt D2.
    #[tokio::test]
    async fn legacy_client_replays_when_nothing_changed() {
        let (subs, manager, sink, mut rx) = harness().await;
        let calls = Arc::new(AtomicU64::new(0));
        let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
            events: vec![ev(10)],
            tail: 0,
            replay_calls: Arc::clone(&calls),
        });
        let principal = Principal::new("acct", "tenant-acme");
        let ruleset = ActiveRuleset::all_mode();
        let composed = compose_sync_epoch(7, ruleset.checksum());
        register_subscribe(
            &req("tasks", Some(composed), Some(5)),
            &subs,
            &manager,
            None,
            7,
            Some(&reader),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(crate::router::SinkMsg::Event(_))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // (j) what `resume_info` advertises is exactly what the gate accepts on
    // the client's next reconnect — both the D2 and the legacy path. Exercises
    // `resume_advertisement` directly (the single source of truth for both
    // sites) rather than re-deriving the formula by hand, so this test only
    // fails if the two call sites actually diverge.
    #[tokio::test]
    async fn advertised_values_match_gate_values() {
        let ruleset = ActiveRuleset::all_mode();
        let principal = Principal::new("acct", "tenant-acme");

        // D2 path.
        {
            let (subs, manager, sink, mut rx) = harness().await;
            let calls = Arc::new(AtomicU64::new(0));
            let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
                events: vec![ev(10)],
                tail: 0,
                replay_calls: Arc::clone(&calls),
            });
            let (adv_epoch, adv_checksum) = resume_advertisement(true, 7, ruleset.checksum());
            let req = SubscribeRequest {
                table: "tasks".into(),
                filters: Vec::new(),
                where_sql: None,
                resume_lsn: Some(5),
                client_epoch: Some(adv_epoch),
                client_rules_checksum: adv_checksum,
            };
            register_subscribe(
                &req,
                &subs,
                &manager,
                None,
                7,
                Some(&reader),
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
            assert!(matches!(
                rx.recv().await,
                Some(crate::router::SinkMsg::Event(_))
            ));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }

        // Legacy path.
        {
            let (subs, manager, sink, mut rx) = harness().await;
            let calls = Arc::new(AtomicU64::new(0));
            let reader: Arc<dyn nostos_application::ports::OpLogSource> = Arc::new(MockOpLog {
                events: vec![ev(10)],
                tail: 0,
                replay_calls: Arc::clone(&calls),
            });
            let (adv_epoch, adv_checksum) = resume_advertisement(false, 7, ruleset.checksum());
            assert_eq!(adv_checksum, None, "legacy path omits the checksum key");
            register_subscribe(
                &req("tasks", Some(adv_epoch), Some(5)),
                &subs,
                &manager,
                None,
                7,
                Some(&reader),
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
            assert!(matches!(
                rx.recv().await,
                Some(crate::router::SinkMsg::Event(_))
            ));
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }

    // ---- P5 sync streams (docs/plans/p5-sync-streams-design.md §2/§3/§6) ----

    /// A `SnapshotSource` fake for stream tests: serves rows from memory,
    /// evaluating the bound predicate with the SAME `PredicateExpr::matches`
    /// semantics as live fan-out and applying the tenant scope the way the
    /// pg adapter's appended tenant clause does (the port contract).
    struct FakeStreamSnapshotter {
        rows: Vec<(&'static str, Vec<(&'static str, ColumnValue)>)>,
    }

    #[async_trait::async_trait]
    impl SnapshotSource for FakeStreamSnapshotter {
        async fn snapshot(
            &self,
            table: &str,
            _base_lsn: Lsn,
            _tenant: Option<nostos_domain::TenantScope<'_>>,
        ) -> Result<Vec<ReplicationEvent>, nostos_application::ports::SnapshotError> {
            panic!("table snapshot unused in stream tests (table {table})")
        }

        async fn snapshot_stream(
            &self,
            table: &str,
            predicate: &PredicateExpr,
            base_lsn: Lsn,
            tenant: Option<nostos_domain::TenantScope<'_>>,
        ) -> Result<Vec<ReplicationEvent>, nostos_application::ports::SnapshotError> {
            let mut out = Vec::new();
            for (pk, cols) in &self.rows {
                let view = |name: &str| {
                    cols.iter()
                        .find(|(c, _)| *c == name)
                        .map(|(_, v)| v.clone())
                };
                // The pg adapter appends `AND "tenant_col"::text = $k` — the
                // fake applies the same restriction in memory.
                let tenant_ok = match tenant {
                    Some(scope) => view(scope.column) == Some(ColumnValue::text(scope.value)),
                    None => true,
                };
                if tenant_ok && predicate.matches(view) {
                    let mut map = serde_json::Map::new();
                    for (k, v) in cols {
                        let jv = match v {
                            ColumnValue::Text(s) => serde_json::Value::String(s.clone()),
                            ColumnValue::Number(n) => serde_json::Value::from(*n),
                            ColumnValue::Float(f) => serde_json::Value::from(*f),
                            ColumnValue::Bool(b) => serde_json::Value::from(*b),
                            ColumnValue::Param(_) | ColumnValue::Any => serde_json::Value::Null,
                        };
                        map.insert((*k).to_string(), jv);
                    }
                    let lsn = base_lsn.raw() + 1 + out.len() as u64;
                    out.push(ReplicationEvent::new(
                        Lsn::new(lsn),
                        RowOp::Insert {
                            table: table.to_string(),
                            pk: (*pk).to_string(),
                            payload: serde_json::to_vec(&map).unwrap().into(),
                        },
                    ));
                }
            }
            Ok(out)
        }
    }

    fn stream_params(v: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn stream_registers_and_snapshots_matching_rows_only() {
        let (subs, manager, sink, mut rx) = harness().await;
        let snap: Arc<dyn SnapshotSource> = Arc::new(FakeStreamSnapshotter {
            rows: vec![
                (
                    "l1",
                    vec![
                        ("owner_id", ColumnValue::text("u1")),
                        ("priority", ColumnValue::number(5)),
                    ],
                ),
                (
                    "l2",
                    vec![
                        ("owner_id", ColumnValue::text("u2")),
                        ("priority", ColumnValue::number(9)),
                    ],
                ),
                (
                    "l3",
                    vec![
                        ("owner_id", ColumnValue::text("u1")),
                        ("priority", ColumnValue::number(1)),
                    ],
                ),
            ],
        });
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner AND priority >= :min");
        let principal = Principal::new("acct", "tenant-acme");
        register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1", "min": 3})),
            &subs,
            &manager,
            Some(&snap),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();

        // begin(s1) → row l1 → end(s1): only the matching row, boundaries
        // tagged with the stream id.
        let begin = rx.recv().await.unwrap();
        let crate::router::SinkMsg::Control(bytes) = begin else {
            panic!("expected begin control frame, got {begin:?}")
        };
        assert_eq!(
            crate::wire::decode_snapshot_boundary(&bytes),
            Some(("lists".to_string(), Some("s1".to_string()), true))
        );
        let row = rx.recv().await.unwrap();
        let crate::router::SinkMsg::Event(ev) = row else {
            panic!("expected row event, got {row:?}")
        };
        let RowOp::Insert { pk, .. } = &ev.op else {
            panic!("expected insert")
        };
        assert_eq!(pk, "l1");
        let end = rx.recv().await.unwrap();
        let crate::router::SinkMsg::Control(bytes) = end else {
            panic!("expected end control frame, got {end:?}")
        };
        assert_eq!(
            crate::wire::decode_snapshot_boundary(&bytes),
            Some(("lists".to_string(), Some("s1".to_string()), false))
        );
        assert!(rx.try_recv().is_err(), "exactly one row matched");

        // Bookkeeping + cursor advance (1 snapshot row).
        let s = subs.lock().await;
        assert!(s.streams.contains_key("s1"));
        assert_eq!(s.synthetic_cursor, 1);
        drop(s);
        assert_eq!(manager.session_count().await, 1);
    }

    #[tokio::test]
    async fn stream_unknown_name_rejected() {
        let (subs, manager, sink, _rx) = harness().await;
        let ruleset = ActiveRuleset::all_mode();
        let principal = Principal::new("acct", "tenant-acme");
        let err = register_stream(
            "s1",
            "nope",
            &stream_params(&serde_json::json!({})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("unknown stream must reject");
        assert!(err.contains("unknown stream 'nope'"), "got: {err}");
        assert_eq!(manager.session_count().await, 0, "no session on reject");
    }

    #[tokio::test]
    async fn stream_bad_params_rejected() {
        let (subs, manager, sink, _rx) = harness().await;
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner AND priority >= :min");
        let principal = Principal::new("acct", "tenant-acme");

        // Missing :min.
        let err = register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("missing param must reject");
        assert!(
            err.contains("missing value for placeholder :min"),
            "got: {err}"
        );

        // Extra param (the tenant-escape shape: never silently passed).
        let err = register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1", "min": 3, "org_id": "tenant-b"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("extra param must reject");
        assert!(err.contains("unexpected param"), "got: {err}");

        // Non-scalar param.
        let err = register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1", "min": [3]})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("array param must reject");
        assert!(
            err.contains("must be a string, number, or boolean"),
            "got: {err}"
        );

        assert_eq!(manager.session_count().await, 0, "no sessions on rejects");
    }

    #[tokio::test]
    async fn stream_on_rules_denied_table_rejected() {
        let (subs, manager, sink, _rx) = harness().await;
        // toggles mode with `lists` sync=false → fail-closed (design §5).
        let mut rules = toggles_rules("lists", false, None);
        rules.streams = vec![nostos_domain::StreamRule {
            name: "lists".into(),
            table: "lists".into(),
            template: "owner_id = :owner".into(),
        }];
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let err = register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("stream on a denied table must reject");
        assert!(err.contains("not synced"), "got: {err}");
        assert_eq!(manager.session_count().await, 0);
    }

    #[tokio::test]
    async fn stream_tenant_column_escape_yields_zero_rows() {
        // The unit-level abuse gate (e2e §6 item 3 does it over real PG):
        // `org_id = :org` bound to another tenant's id ANDs against the
        // principal's tenant clause — the impossible predicate, zero rows.
        let (subs, manager, sink, mut rx) = harness().await;
        let snap: Arc<dyn SnapshotSource> = Arc::new(FakeStreamSnapshotter {
            rows: vec![
                ("t1", vec![("org_id", ColumnValue::text("tenant-acme"))]),
                ("t2", vec![("org_id", ColumnValue::text("tenant-b"))]),
            ],
        });
        let ruleset = stream_ruleset("by_org", "tasks", "org_id = :org");
        let principal = Principal::new("acct", "tenant-acme");
        register_stream(
            "s1",
            "by_org",
            &stream_params(&serde_json::json!({"org": "tenant-b"})),
            &subs,
            &manager,
            Some(&snap),
            &sink,
            &principal,
            Some("org_id"),
            &ruleset,
        )
        .await
        .unwrap();

        // begin + end arrive (snapshot ran), ZERO rows between them — never
        // an interpolation error, never another tenant's row.
        let first = rx.recv().await.unwrap();
        assert!(
            matches!(first, crate::router::SinkMsg::Control(_)),
            "begin frame first"
        );
        let second = rx.recv().await.unwrap();
        assert!(
            matches!(second, crate::router::SinkMsg::Control(_)),
            "end frame second — no rows"
        );
        assert!(rx.try_recv().is_err());

        let s = subs.lock().await;
        assert!(s.streams.contains_key("s1"));
    }

    /// Audit finding 7: the stream's INITIAL SNAPSHOT must enforce the same
    /// ruleset scope live fan-out does.
    ///
    /// `register_stream` used to hand the raw bound template to
    /// `snapshot_stream` while the session predicate carried `rules ∧ bound ∧
    /// tenant`. So under a table scope the first sync over-delivered exactly
    /// that scope's worth of rows, and every row after it was filtered
    /// correctly — a leak that only exists in the first frame, which is why
    /// reading the live path proves nothing about it.
    ///
    /// `l2` is the load-bearing row: the stream template admits it
    /// (`owner_id = u1`) and only the RULES scope (`status = 'open'`) hides
    /// it. Before the fix it is delivered; after, it is not.
    #[tokio::test]
    async fn stream_snapshot_applies_the_rules_scope_not_just_the_template() {
        let (subs, manager, sink, mut rx) = harness().await;
        let snap: Arc<dyn SnapshotSource> = Arc::new(FakeStreamSnapshotter {
            rows: vec![
                (
                    "l1",
                    vec![
                        ("owner_id", ColumnValue::text("u1")),
                        ("status", ColumnValue::text("open")),
                    ],
                ),
                (
                    "l2",
                    vec![
                        ("owner_id", ColumnValue::text("u1")),
                        ("status", ColumnValue::text("archived")),
                    ],
                ),
                (
                    "l3",
                    vec![
                        ("owner_id", ColumnValue::text("u2")),
                        ("status", ColumnValue::text("open")),
                    ],
                ),
            ],
        });
        // Toggles mode so the table carries a REAL scope (the `all`-mode
        // helper decides `Allow(Any)`, which cannot catch this bug).
        let ruleset = ActiveRuleset::compile(&nostos_domain::SyncRules {
            version: nostos_domain::RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![nostos_domain::TableRule {
                table: "lists".into(),
                sync: true,
                scope: Some("status = 'open'".into()),
            }],
            hand: Vec::new(),
            streams: vec![nostos_domain::StreamRule {
                name: "mine".into(),
                table: "lists".into(),
                template: "owner_id = :owner".into(),
            }],
        })
        .unwrap();

        register_stream(
            "s1",
            "mine",
            &stream_params(&serde_json::json!({ "owner": "u1" })),
            &subs,
            &manager,
            Some(&snap),
            &sink,
            &Principal::new("acct", "tenant-acme"),
            None,
            &ruleset,
        )
        .await
        .expect("stream registers");

        // begin boundary
        assert!(matches!(
            rx.try_recv().unwrap(),
            crate::router::SinkMsg::Control(_)
        ));

        let mut delivered = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::router::SinkMsg::Event(ev) = msg {
                delivered.push(ev.op.pk().to_string());
            }
        }
        assert_eq!(
            delivered,
            vec!["l1".to_string()],
            "snapshot must apply the rules scope: l2 is owner-matched but \
             status='archived' (rules-hidden), l3 is another owner"
        );
    }

    #[tokio::test]
    async fn unsubscribe_stream_removes_session_and_unknown_id_is_noop() {
        let (subs, manager, sink, _rx) = harness().await;
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner");
        let principal = Principal::new("acct", "tenant-acme");
        register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert_eq!(manager.session_count().await, 1);

        unregister_stream("s1", &subs, &manager).await;
        assert_eq!(manager.session_count().await, 0, "session disconnected");
        assert!(!subs.lock().await.streams.contains_key("s1"));

        // Regression (P5 demo phase 5): the stream session shares the
        // socket-wide sink — unsubscribe must NOT close it, or every later
        // delivery on the socket (base subs, re-registered streams) drops.
        assert_eq!(
            sink.deliver_control(vec![0]),
            nostos_application::ports::DeliveryDecision::Delivered,
            "shared socket sink must stay open after stream unsubscribe"
        );

        // Unknown id: idempotent no-op, no error, no panic.
        unregister_stream("s1", &subs, &manager).await;
        unregister_stream("never-seen", &subs, &manager).await;
    }

    #[tokio::test]
    async fn stream_id_reuse_replaces_session() {
        let (subs, manager, sink, _rx) = harness().await;
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner");
        let principal = Principal::new("acct", "tenant-acme");
        for owner in ["u1", "u2"] {
            register_stream(
                "s1",
                "lists",
                &stream_params(&serde_json::json!({"owner": owner})),
                &subs,
                &manager,
                None,
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            manager.session_count().await,
            1,
            "the replaced session was disconnected, not leaked"
        );
        assert_eq!(subs.lock().await.streams.len(), 1);
    }

    /// Regression (caught by the P5 demo): a stream added AFTER the socket
    /// has acked live traffic must still deliver its snapshot — the sink's
    /// `admit` gate drops frames at or below the acked cursor, so the
    /// snapshot base must ride the socket frontier, not the stale synthetic
    /// cursor.
    #[tokio::test]
    async fn stream_snapshot_after_acked_live_traffic_still_delivers() {
        let (subs, manager, sink, mut rx) = harness().await;
        let snap: Arc<dyn SnapshotSource> = Arc::new(FakeStreamSnapshotter {
            rows: vec![("l9", vec![("owner_id", ColumnValue::text("u1"))])],
        });
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner");
        let principal = Principal::new("acct", "tenant-acme");

        // Simulate live traffic already delivered + acked at LSN 500.
        //
        // `seed_acked_lsn` sets BOTH cursors, which is what "delivered and
        // acked" actually means on a live socket. A bare `record_ack(500)`
        // used to work here only because acks were unvalidated — the sink now
        // clamps an ack to what it delivered, so acking 500 having delivered
        // nothing correctly registers as 0 and this fixture would be
        // simulating a state no real client can reach.
        sink.seed_acked_lsn(Lsn::new(500));

        register_stream(
            "s1",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1"})),
            &subs,
            &manager,
            Some(&snap),
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();

        // begin → row → end: the row's synthetic LSN (501) clears the ack
        // gate (500). Without the frontier-aware base it would be stamped 1
        // and dropped invisibly.
        let mut saw_row = false;
        let mut end_seen = false;
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
        {
            match msg {
                crate::router::SinkMsg::Event(ev) => {
                    assert!(
                        ev.lsn.raw() > 500,
                        "snapshot row must stamp above the acked frontier"
                    );
                    saw_row = true;
                }
                crate::router::SinkMsg::Control(bytes) => {
                    let (_, _, begin) = crate::wire::decode_snapshot_boundary(&bytes).unwrap();
                    if !begin {
                        end_seen = true;
                        break;
                    }
                }
            }
        }
        assert!(saw_row, "snapshot row delivered after acked live traffic");
        assert!(end_seen, "end boundary delivered");
    }

    #[tokio::test]
    async fn stream_cap_counts_tables_and_streams_together() {
        let (subs, manager, sink, _rx) = harness().await;
        let ruleset = stream_ruleset("lists", "lists", "owner_id = :owner");
        let principal = Principal::new("acct", "tenant-acme");
        // Fill to the cap with streams alone.
        for i in 0..MAX_TABLES_PER_SOCKET {
            register_stream(
                &format!("s{i}"),
                "lists",
                &stream_params(&serde_json::json!({"owner": "u1"})),
                &subs,
                &manager,
                None,
                &sink,
                &principal,
                None,
                &ruleset,
            )
            .await
            .unwrap();
        }
        let err = register_stream(
            "one-too-many",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u1"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .expect_err("33rd subscription must reject");
        assert!(err.contains("cap"), "got: {err}");

        // An existing id still replaces under a full cap (not a growth).
        register_stream(
            "s0",
            "lists",
            &stream_params(&serde_json::json!({"owner": "u2"})),
            &subs,
            &manager,
            None,
            &sink,
            &principal,
            None,
            &ruleset,
        )
        .await
        .unwrap();
        assert_eq!(subs.lock().await.streams.len(), MAX_TABLES_PER_SOCKET);
    }
}
