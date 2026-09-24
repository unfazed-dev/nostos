//! Health + metrics endpoints (ADR: operability, T1-6/T1-7): `GET /healthz`,
//! `GET /schema`, `GET /metrics`, and the metadata gate they share with `/rules`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use nostos_application::ports::{Metrics, SchemaDescriptor, SessionStore};
use nostos_infra::transport::SyncRouterState;
use tracing::warn;

/// `GET /healthz` — liveness/readiness. Returns the live session count and
/// replicator-driver liveness, both cheap to read (O(1) atomic). A load
/// balancer polls this to decide whether to route traffic. When the
/// replicator→fan-out driver has EXITED (stream end — audit 2026-08-17 M6)
/// the server is a zombie: it accepts `/sync` and serves snapshots but
/// delivers no live events. The endpoint then answers 503 `"degraded"`
/// so the LB drains it. (A driver PANIC is not folded in — the tokio panic
/// hook already screams on stderr; the exit path is the silent one.)
pub(crate) async fn healthz(
    State(state): State<SyncRouterState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let sessions = state.manager.session_count().await;
    let driver_dead = state
        .driver_dead
        .as_ref()
        .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed));
    let (code, status, driver) = if driver_dead {
        (StatusCode::SERVICE_UNAVAILABLE, "degraded", "dead")
    } else {
        (StatusCode::OK, "ok", "live")
    };
    (
        code,
        Json(serde_json::json!({
            "status": status,
            "sessions": sessions,
            "replicator_driver": driver,
        })),
    )
}

/// `GET /schema` — the publication's typed schema (WS1): tables, columns, and
/// SQLite affinities, so the Flutter SDK can auto-build typed tables without a
/// hand-written `Schema`. v1 is unauthenticated (schema is publication-wide
/// metadata, not tenant-scoped rows; row isolation is the read-path predicate's
/// job — ADR-0011/0018). Returns 404 when no `SchemaSource` is wired (the fake
/// / no-`pg` path) and 503 on a transient backend error.
pub(crate) async fn schema(
    State(state): State<SyncRouterState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<SchemaDescriptor>, StatusCode> {
    // `/schema` is a CLIENT endpoint (auto-schema), so it gates on SYNC auth,
    // not the admin token — a real client already holds a bearer token, an
    // operator does not necessarily. Off unless NOSTOS_PROTECT_METADATA is set,
    // because gating it breaks `nostos pull` and any Flutter app on an older SDK.
    if metadata_protected() && !sync_authenticated(&state, &headers).await {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let src = state.schema_source.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    src.fetch().await.map(Json).map_err(|e| {
        warn!(error = %e, "schema fetch failed");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

/// Boot-time resolution of `--protect-metadata` / `NOSTOS_PROTECT_METADATA`,
/// published for the two handlers that gate on it.
///
/// A `static` rather than a `SyncRouterState` field because this is
/// server-level policy, and rather than a per-request `env::var` because that
/// re-read would silently treat a typo as "off". Clap validates the value once
/// at startup; this only carries the answer.
pub(crate) static PROTECT_METADATA: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn metadata_protected() -> bool {
    PROTECT_METADATA.load(std::sync::atomic::Ordering::Relaxed)
}

/// Does this request carry a token the *sync* auth adapter accepts?
///
/// Honest limitation: on an `AllowAnonymous` deployment this returns `true` for
/// everyone, because that adapter accepts the empty token by design. The gate
/// is therefore only meaningful where sync auth is actually configured — which
/// is the same population that has tenant separation to protect. Boot logs say
/// so plainly rather than implying protection that isn't there.
async fn sync_authenticated(state: &SyncRouterState, headers: &axum::http::HeaderMap) -> bool {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    state.auth.authenticate(token).await.is_some()
}

/// `GET /metrics` — Prometheus text exposition format, hand-rolled (the
/// workspace intentionally stays dependency-free for metrics per the
/// ponytail-audit cuts). Counters are aggregate throughput from the fan-out
/// service; the sessions gauge is snapshotted from the store.
pub(crate) async fn metrics_handler(metrics: Arc<Metrics>, store: Arc<dyn SessionStore>) -> String {
    let snap = metrics.snapshot();
    let sessions = store.len().await;
    // Per-account last-pushed-LSN (plan 3.2 — the push-LSN→client-ack
    // correlation surface), rendered next to the session gauges so an
    // operator can see whether a doorbelled device actually caught up.
    // Label values are escaped (account ids are external data).
    let last_pushed: String = metrics
        .push_last_lsn
        .lock()
        .map(|map| {
            map.iter().fold(String::new(), |mut acc, (account, lsn)| {
                let escaped = account.replace('\\', "\\\\").replace('"', "\\\"");
                let _ = std::fmt::Write::write_fmt(
                    &mut acc,
                    format_args!("cairn_push_last_lsn{{account=\"{escaped}\"}} {lsn}\n"),
                );
                acc
            })
        })
        .unwrap_or_default();
    format!(
        "# HELP cairn_events_matched_total Events whose predicate matched ≥1 session.\n\
         # TYPE cairn_events_matched_total counter\n\
         cairn_events_matched_total {matched}\n\
         # HELP cairn_events_delivered_total Events accepted by a session sink.\n\
         # TYPE cairn_events_delivered_total counter\n\
         cairn_events_delivered_total {delivered}\n\
         # HELP cairn_events_dropped_total Events dropped (full buffer / dedup / closed).\n\
         # TYPE cairn_events_dropped_total counter\n\
         cairn_events_dropped_total {dropped}\n\
         # HELP cairn_events_superseded_total Events that replaced a still-waiting frame for the same row in a sink's conflating overflow (ADR-0045). NOT loss — every frame is a complete row image, so the client converges from the newer one alone. Subtract from attempted alongside dropped when computing a drop rate from frame counts.\n\
         # TYPE cairn_events_superseded_total counter\n\
         cairn_events_superseded_total {superseded}\n\
         # HELP cairn_events_faulted_total Delivery tasks that faulted (panicked / cancelled) — a server fault, NOT slow-client backpressure. Kept distinct from cairn_events_dropped_total so a panic is never mis-attributed as a client drop in the 0%-drops figure. Alert on any increase.\n\
         # TYPE cairn_events_faulted_total counter\n\
         cairn_events_faulted_total {faulted}\n\
         # HELP cairn_live_sessions Current live sync sessions.\n\
         # TYPE cairn_live_sessions gauge\n\
         cairn_live_sessions {sessions}\n\
         # HELP cairn_slot_wal_status Replication-slot health gauge (0=healthy, 1=reserved, 2=lost, 3=recreated). 'lost' means silent-data-loss risk; alert on cairn_slot_recreated_total. ADR-0009.\n\
         # TYPE cairn_slot_wal_status gauge\n\
         cairn_slot_wal_status {slot_wal_status}\n\
         # HELP cairn_replication_lag_bytes Current WAL lsn minus slot restart_lsn, in bytes. 0 when slot is missing/unknown.\n\
         # TYPE cairn_replication_lag_bytes gauge\n\
         cairn_replication_lag_bytes {replication_lag_bytes}\n\
         # HELP cairn_slot_recreated_total Number of times the replication slot was dropped + re-created from a missing/lost state. Each increment is a potential silent-data-loss window; alert on any increase.\n\
         # TYPE cairn_slot_recreated_total counter\n\
         cairn_slot_recreated_total {slot_recreated_total}\n\
         # HELP cairn_oplog_dropped_total Op-log entries dropped (writer buffer full). The resume path falls back to snapshot-reconcile for the gap. ADR-0025.\n\
         # TYPE cairn_oplog_dropped_total counter\n\
         cairn_oplog_dropped_total {oplog_dropped}\n\
         # HELP cairn_oplog_flush_failed_total Op-log batch flushes that failed (PG error / connection lost). Batch lost; resume falls back to snapshot-reconcile. ADR-0025.\n\
         # TYPE cairn_oplog_flush_failed_total counter\n\
         cairn_oplog_flush_failed_total {oplog_flush_failed}\n\
         # HELP cairn_slot_epoch Monotonic epoch bumped on every replication-slot (re)creation. A client whose last-seen epoch differs must full-snapshot (cannot backfill from a recreated slot's dead lineage). ADR-0025.\n\
         # TYPE cairn_slot_epoch gauge\n\
         cairn_slot_epoch {slot_epoch}\n\
         # HELP cairn_oplog_compacted_rows_total Rows swept by op-log compaction (collapse duplicates to latest op per (table_name, pk) + age out rows past the retention window). ADR-0025 slice 5.\n\
         # TYPE cairn_oplog_compacted_rows_total counter\n\
         cairn_oplog_compacted_rows_total {oplog_compacted_rows}\n\
         # HELP cairn_push_enqueued_total Push doorbell hints enqueued (one per matched offline account; online accounts suppressed at enqueue time). ADR-0037.\n\
         # TYPE cairn_push_enqueued_total counter\n\
         cairn_push_enqueued_total {push_enqueued}\n\
         # HELP cairn_push_dropped_total Push hints dropped (bounded channel full / consumer gone). Doorbell semantics: a dropped hint loses nothing — the durable LSN checkpoint reconciles. ADR-0037.\n\
         # TYPE cairn_push_dropped_total counter\n\
         cairn_push_dropped_total {push_dropped}\n\
         # HELP cairn_push_sent_total Push sends the rails accepted (2xx). Last-mile delivery stays best-effort; the client's LSN ack is the proof. ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_sent_total counter\n\
         cairn_push_sent_total {push_sent}\n\
         # HELP cairn_push_failed_total Push sends that failed terminally or exhausted their retry (rail fatal/transient). ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_failed_total counter\n\
         cairn_push_failed_total {push_failed}\n\
         # HELP cairn_push_pruned_total Push-token rows pruned (rail reported the target gone, or the owner deregistered). ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_pruned_total counter\n\
         cairn_push_pruned_total {push_pruned}\n\
         # HELP cairn_push_last_lsn Highest doorbell LSN pushed per account — correlate against session acked-LSN to see whether a doorbelled device actually caught up. ADR-0037 plan 3.2.\n\
         # TYPE cairn_push_last_lsn gauge\n\
         {last_pushed}",
        matched = snap.matched,
        delivered = snap.delivered,
        dropped = snap.dropped,
        superseded = snap.superseded,
        faulted = snap.faulted,
        sessions = sessions,
        slot_wal_status = snap.slot_wal_status.as_gauge_int(),
        replication_lag_bytes = snap.replication_lag_bytes,
        slot_recreated_total = snap.slot_recreated_total,
        oplog_dropped = snap.oplog_dropped,
        oplog_flush_failed = snap.oplog_flush_failed,
        slot_epoch = snap.slot_epoch,
        oplog_compacted_rows = snap.oplog_compacted_rows,
        push_enqueued = snap.push_enqueued,
        push_dropped = snap.push_dropped,
        push_sent = snap.push_sent,
        push_failed = snap.push_failed,
        push_pruned = snap.push_pruned,
    )
}
