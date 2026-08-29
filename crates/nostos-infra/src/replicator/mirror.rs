//! `MirrorReplicator` — channel-fed replication for the desktop-sidecar
//! mirror-ingest topology (B2; arxa docs/plans/doorbell-decision-2026-08-29.md
//! section 4; ADR-0042).
//!
//! The studio engine is the single writer; nostos is a read-model replica. The
//! engine's mirror-out POSTs row events to the sidecar's `/ingest` route,
//! which calls into the [`MirrorHandle`] — the handle stamps a server-side
//! LSN, records the row into an in-memory buffer (the SNAPSHOT truth) and
//! forwards the event over a channel (the STREAM). The stream half,
//! [`MirrorReplicator`], plugs into the exact same [`ReplicatorStream`] seam
//! as `PgReplicator` and `FakeReplicator` (the adapter-swap payoff
//! main.rs:8-11 advertises), so fan-out, push hints, and the WS transport run
//! unchanged.
//!
//! The buffer half implements [`SnapshotSource`] so a freshly-subscribing
//! client sees pre-ingest rows (PowerSync parity) — phase-1 sized: approvals
//! are ephemeral and few, so an in-memory BTreeMap is the whole store.
//!
//! LSN discipline: ONE allocator (`next_lsn`) serves BOTH live ingest and
//! snapshot bands, and a snapshot RESERVES its band from the same counter —
//! so a snapshot's synthetic LSNs can never collide with (and cause the
//! per-session dedup/gate to drop) a live event, the exact corner
//! `snapshot_source.rs::rows_to_events` carries as a ponytail for the pg
//! path. A sidecar restart resets the counter and the buffer; clients
//! re-subscribe via the wire's epoch semantics. Durable sequencing is
//! deferred with the full-mirror phases.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::ident::validate_ident;
use nostos_application::ports::{ReplicatorStream, SnapshotError, SnapshotSource};
use nostos_domain::{Lsn, ReplicationEvent, RowOp};

/// The ingest-side half: clone per `/ingest` request handler state.
#[derive(Clone)]
pub struct MirrorHandle {
    tx: mpsc::UnboundedSender<ReplicationEvent>,
    /// table -> pk -> payload — the snapshot materialization, applied at
    /// ingest time so a subscriber that arrives later still sees the rows.
    buffer: Arc<Mutex<BTreeMap<String, BTreeMap<String, Bytes>>>>,
    /// The single LSN allocator for live events AND snapshot bands (see the
    /// module doc). Starts at 1; monotonically increasing.
    next_lsn: Arc<AtomicU64>,
}

impl MirrorHandle {
    /// Open a mirror: the handle goes to the ingest route, the replicator
    /// into the fan-out driver.
    #[must_use]
    pub fn open() -> (Self, MirrorReplicator) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                buffer: Arc::new(Mutex::new(BTreeMap::new())),
                next_lsn: Arc::new(AtomicU64::new(1)),
            },
            MirrorReplicator { rx },
        )
    }

    /// Stamp, record, and forward one row operation. The LSN comes from the
    /// shared allocator; recording happens even if the stream half is gone
    /// (a shutdown race must not corrupt the snapshot truth). Returns the
    /// stamped event (the route echoes the LSN back to the writer).
    pub fn ingest(&self, op: RowOp) -> ReplicationEvent {
        let lsn = Lsn::new(self.next_lsn.fetch_add(1, Ordering::Relaxed));
        let event = ReplicationEvent::new(lsn, op);
        {
            let mut buffer = self.buffer.lock().expect("mirror buffer poisoned");
            let table = match &event.op {
                RowOp::Insert { table, .. }
                | RowOp::Update { table, .. }
                | RowOp::Delete { table, .. } => table.clone(),
            };
            let rows = buffer.entry(table).or_default();
            match event.op.clone() {
                RowOp::Insert { pk, payload, .. } | RowOp::Update { pk, payload, .. } => {
                    rows.insert(pk, payload);
                }
                RowOp::Delete { pk, .. } => {
                    rows.remove(&pk);
                }
            }
        }
        // The stream half may already be dropped during shutdown — fine.
        let _ = self.tx.send(event.clone());
        event
    }

    /// Buffered rows of one table, in pk order. Test/diagnostic seam.
    #[must_use]
    pub fn buffered_rows(&self, table: &str) -> Vec<(String, Bytes)> {
        let buffer = self.buffer.lock().expect("mirror buffer poisoned");
        buffer
            .get(table)
            .map(|rows| rows.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }
    /// Current allocator position. Test/diagnostic seam.
    #[must_use]
    pub fn lsn_position(&self) -> u64 {
        self.next_lsn.load(Ordering::Relaxed)
    }
}

/// The stream half: drains what [`MirrorHandle::ingest`] forwards.
pub struct MirrorReplicator {
    rx: mpsc::UnboundedReceiver<ReplicationEvent>,
}

#[async_trait]
impl ReplicatorStream for MirrorReplicator {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        // `None` = the handle half was dropped: clean shutdown, exactly the
        // port's documented exhausted-stream contract.
        self.rx.recv().await
    }
}

#[async_trait]
impl SnapshotSource for MirrorHandle {
    async fn snapshot(
        &self,
        table: &str,
        base_lsn: Lsn,
        tenant: Option<nostos_domain::principal::TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        // Same trust boundary as the pg snapshotter: the table arrives in the
        // CLIENT's subscribe frame — validate before anything else.
        if let Err(bad) = validate_ident(table) {
            return Err(SnapshotError::InvalidTable(bad));
        }
        let rows = {
            let buffer = self.buffer.lock().expect("mirror buffer poisoned");
            buffer
                .get(table)
                .map(|rows| {
                    rows.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        // Collect first (under the lock), then filter the tenant scope, then
        // reserve EXACTLY the band we need — no allocator slots burned on
        // tenant-filtered-out rows.
        let in_scope: Vec<&(String, Bytes)> = rows
            .iter()
            .filter(|(_, payload)| {
                match tenant {
                    None => true,
                    Some(scope) => {
                        // Read-path tenant parity (ADR-0011/0018): the row
                        // must carry the tenant column with the principal's
                        // value. Fail-closed: a row without the column is
                        // NOT visible.
                        serde_json::from_slice::<serde_json::Value>(payload)
                            .ok()
                            .and_then(|v| {
                                v.get(scope.column)
                                    .and_then(|c| c.as_str())
                                    .map(|s| s == scope.value)
                            })
                            .unwrap_or(false)
                    }
                }
            })
            .collect();
        // Reserve the band AFTER the tenant filter so the size is exact. The
        // band must sit entirely ABOVE the client's floor (`base_lsn`, the
        // per-session sink's gate): if the allocator's current position is
        // still at-or-below the floor, burn a band and take the next one —
        // the counter only grows, so this terminates. Burned bands are
        // harmless: LSNs need monotonicity and uniqueness, not density.
        let count = in_scope.len() as u64;
        if count == 0 {
            return Ok(Vec::new());
        }
        let start = loop {
            let candidate = self.next_lsn.fetch_add(count, Ordering::Relaxed);
            if candidate > base_lsn.0 {
                break candidate;
            }
        };
        Ok(in_scope
            .iter()
            .enumerate()
            .map(|(i, (pk, payload))| {
                ReplicationEvent::new(
                    Lsn::new(start.saturating_add(i as u64)),
                    RowOp::Insert {
                        table: table.to_string(),
                        pk: pk.clone(),
                        payload: payload.clone(),
                    },
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert_op(table: &str, pk: &str, payload: &str) -> RowOp {
        RowOp::Insert {
            table: table.to_string(),
            pk: pk.to_string(),
            payload: Bytes::from(payload.to_string()),
        }
    }

    fn delete_op(table: &str, pk: &str) -> RowOp {
        RowOp::Delete {
            table: table.to_string(),
            pk: pk.to_string(),
            old_payload: None,
        }
    }

    #[tokio::test]
    async fn ingested_events_yield_in_order() {
        let (handle, mut replicator) = MirrorHandle::open();
        let e1 = handle.ingest(upsert_op("approvals", "a1", "{}"));
        let e2 = handle.ingest(upsert_op("approvals", "a2", "{}"));
        assert!(e1.lsn.0 < e2.lsn.0, "stamped LSNs are monotonic");
        let s1 = replicator.next_event().await.unwrap();
        let s2 = replicator.next_event().await.unwrap();
        assert_eq!(s1.op.pk(), "a1");
        assert_eq!(s2.op.pk(), "a2");
        assert_eq!(s1.lsn, e1.lsn, "stream carries the stamped event");
    }

    #[tokio::test]
    async fn dropped_handle_ends_the_stream_cleanly() {
        let (handle, mut replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "a1", "{}"));
        drop(handle);
        assert!(replicator.next_event().await.is_some());
        assert!(replicator.next_event().await.is_none());
    }

    #[tokio::test]
    async fn buffer_tracks_upserts_and_deletes() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "a1", "{}"));
        handle.ingest(upsert_op("approvals", "a2", "{}"));
        handle.ingest(upsert_op("approvals", "a1", "{\"v\":2}"));
        handle.ingest(delete_op("approvals", "a2"));
        let rows = handle.buffered_rows("approvals");
        assert_eq!(rows.len(), 1, "upsert overwrote a1, delete removed a2");
        assert_eq!(rows[0].0, "a1");
    }

    #[tokio::test]
    async fn snapshot_returns_buffered_rows_above_base_lsn() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "b", "{}"));
        handle.ingest(upsert_op("approvals", "a", "{}"));
        let snap = handle
            .snapshot("approvals", Lsn::new(5), None)
            .await
            .unwrap();
        assert_eq!(snap.len(), 2);
        // pk order, every LSN unique and strictly above the floor.
        assert_eq!(snap[0].op.pk(), "a");
        assert!(snap[0].lsn.0 > 5);
        assert!(snap[1].lsn.0 > snap[0].lsn.0);
        // Delivered as Insert: the client's idempotent apply treats a
        // snapshot row exactly like a streamed insert (port contract).
        assert!(matches!(snap[0].op, RowOp::Insert { .. }));
    }

    #[tokio::test]
    async fn snapshot_rejects_an_invalid_table_identifier() {
        let (handle, _replicator) = MirrorHandle::open();
        let err = handle
            .snapshot("approvals; DROP TABLE x", Lsn::ZERO, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SnapshotError::InvalidTable(_)));
    }

    #[tokio::test]
    async fn snapshot_tenant_scope_is_fail_closed() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "mine", "{\"org_id\":\"acme\"}"));
        handle.ingest(upsert_op("approvals", "theirs", "{\"org_id\":\"other\"}"));
        handle.ingest(upsert_op("approvals", "bare", "{}"));
        let scope = nostos_domain::principal::TenantScope::new("org_id", "acme");
        let snap = handle
            .snapshot("approvals", Lsn::ZERO, Some(scope))
            .await
            .unwrap();
        let pks: Vec<_> = snap.iter().map(|e| e.op.pk().to_string()).collect();
        assert_eq!(pks, vec!["mine".to_string()], "fail-closed: {pks:?}");
    }

    #[tokio::test]
    async fn snapshot_of_an_empty_table_is_empty_ok() {
        let (handle, _replicator) = MirrorHandle::open();
        let snap = handle.snapshot("approvals", Lsn::ZERO, None).await.unwrap();
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn snapshot_lsns_never_collide_with_later_ingests() {
        // The corner snapshot_source.rs::rows_to_events carries as a
        // ponytail: a snapshot band must not share LSNs with live events, or
        // the per-session dedup/gate drops the live one.
        let (handle, mut replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "a1", "{}"));
        let snap = handle.snapshot("approvals", Lsn::ZERO, None).await.unwrap();
        assert_eq!(snap.len(), 1);
        let after = handle.ingest(upsert_op("approvals", "a2", "{}"));
        let live = replicator.next_event().await.unwrap(); // a1
        assert_eq!(live.op.pk(), "a1");
        for s in &snap {
            assert_ne!(s.lsn, after.lsn, "snapshot band must not collide");
            assert_ne!(s.lsn, live.lsn);
        }
        assert!(after.lsn.0 > snap[0].lsn.0, "allocator moved past the band");
    }

    #[tokio::test]
    async fn snapshot_clears_a_resuming_client_floor() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert_op("approvals", "a1", "{}"));
        // A resuming client acked far past the allocator's position.
        let snap = handle
            .snapshot("approvals", Lsn::new(1_000_000), None)
            .await
            .unwrap();
        assert!(snap[0].lsn.0 > 1_000_000, "stamped {}", snap[0].lsn.0);
        // And the allocator moved past the band it burned.
        let next = handle.ingest(upsert_op("approvals", "a2", "{}"));
        assert!(
            next.lsn.0 > snap[0].lsn.0,
            "allocator past the burned band: {} vs {}",
            next.lsn.0,
            snap[0].lsn.0
        );
    }
}
