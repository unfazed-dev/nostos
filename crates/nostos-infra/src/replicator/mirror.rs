//! `MirrorReplicator` — channel-fed replication for the desktop-sidecar
//! mirror-ingest topology (B2; arxa docs/plans/doorbell-decision-2026-08-29.md
//! section 4; ADR-00NN).
//!
//! The studio engine is the single writer; nostos is a read-model replica. The
//! engine's mirror-out POSTs row events to the sidecar's `/ingest` route,
//! which calls into the [`MirrorHandle`] — the handle records the row into an
//! in-memory buffer (the SNAPSHOT truth) and forwards the event over a channel
//! (the STREAM). The stream half, [`MirrorReplicator`], plugs into the exact
//! same [`ReplicatorStream`] seam as `PgReplicator` and `FakeReplicator`
//! (the adapter-swap payoff main.rs:8-11 advertises), so fan-out, push hints,
//! and the WS transport run unchanged.
//!
//! The buffer half implements [`SnapshotSource`] so a freshly-subscribing
//! client sees pre-ingest rows (PowerSync parity) — phase-1 sized: approvals
//! are ephemeral and few, so an in-memory BTreeMap is the whole store.
//!
//! LSN discipline: the `/ingest` caller (a server-side counter) stamps events
//! before the handle sees them; a sidecar restart resets the counter and the
//! buffer, and clients re-subscribe via the wire's epoch semantics. Durable
//! sequencing is deferred with the full-mirror phases.

use std::collections::BTreeMap;
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
            },
            MirrorReplicator { rx },
        )
    }

    /// Record one event into the snapshot buffer, then forward it to the
    /// live stream. Recording happens even if the stream half is gone (a
    /// shutdown race must not corrupt the snapshot truth).
    pub fn ingest(&self, event: ReplicationEvent) {
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
        let _ = self.tx.send(event);
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
        let buffer = self.buffer.lock().expect("mirror buffer poisoned");
        let mut events = Vec::new();
        let mut lsn = base_lsn
            .0
            .checked_add(1)
            .ok_or_else(|| SnapshotError::Backend("snapshot LSN overflow".into()))?;
        if let Some(rows) = buffer.get(table) {
            for (pk, payload) in rows {
                if let Some(scope) = tenant {
                    // Read-path tenant parity (ADR-0011/0018): the row must
                    // carry the tenant column with the principal's value.
                    // Fail-closed: a row without the column is NOT visible.
                    let matches = serde_json::from_slice::<serde_json::Value>(payload)
                        .ok()
                        .and_then(|v| {
                            v.get(scope.column)
                                .and_then(|c| c.as_str())
                                .map(|s| s == scope.value)
                        })
                        .unwrap_or(false);
                    if !matches {
                        continue;
                    }
                }
                events.push(ReplicationEvent::new(
                    Lsn::new(lsn),
                    RowOp::Insert {
                        table: table.to_string(),
                        pk: pk.clone(),
                        payload: payload.clone(),
                    },
                ));
                lsn = lsn
                    .checked_add(1)
                    .ok_or_else(|| SnapshotError::Backend("snapshot LSN overflow".into()))?;
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(table: &str, pk: &str, payload: &str, lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.to_string(),
                pk: pk.to_string(),
                payload: Bytes::from(payload.to_string()),
            },
        )
    }

    fn delete(table: &str, pk: &str, lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Delete {
                table: table.to_string(),
                pk: pk.to_string(),
                old_payload: None,
            },
        )
    }

    #[tokio::test]
    async fn ingested_events_yield_in_order() {
        let (handle, mut replicator) = MirrorHandle::open();
        handle.ingest(upsert("approvals", "a1", "{}", 1));
        handle.ingest(upsert("approvals", "a2", "{}", 2));
        let e1 = replicator.next_event().await.unwrap();
        let e2 = replicator.next_event().await.unwrap();
        assert_eq!(e1.op.pk(), "a1");
        assert_eq!(e2.op.pk(), "a2");
    }

    #[tokio::test]
    async fn dropped_handle_ends_the_stream_cleanly() {
        let (handle, mut replicator) = MirrorHandle::open();
        handle.ingest(upsert("approvals", "a1", "{}", 1));
        drop(handle);
        assert!(replicator.next_event().await.is_some());
        assert!(replicator.next_event().await.is_none());
    }

    #[tokio::test]
    async fn buffer_tracks_upserts_and_deletes() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert("approvals", "a1", "{\"id\":\"a1\"}", 1));
        handle.ingest(upsert("approvals", "a2", "{\"id\":\"a2\"}", 2));
        handle.ingest(upsert("approvals", "a1", "{\"id\":\"a1\",\"v\":2}", 3));
        handle.ingest(delete("approvals", "a2", 4));
        let rows = handle.buffered_rows("approvals");
        assert_eq!(rows.len(), 1, "upsert overwrote a1, delete removed a2");
        assert_eq!(rows[0].0, "a1");
    }

    #[tokio::test]
    async fn snapshot_returns_buffered_rows_above_base_lsn() {
        let (handle, _replicator) = MirrorHandle::open();
        handle.ingest(upsert("approvals", "b", "{}", 10));
        handle.ingest(upsert("approvals", "a", "{}", 20));
        let snap = handle
            .snapshot("approvals", Lsn::new(5), None)
            .await
            .unwrap();
        assert_eq!(snap.len(), 2);
        // pk order, every LSN unique and strictly above the floor.
        assert_eq!(snap[0].op.pk(), "a");
        assert!(snap[0].lsn.0 > 5);
        assert!(snap[1].lsn.0 > snap[0].lsn.0);
        // Delivered as Insert (the client's idempotent apply treats a
        // snapshot row exactly like a streamed insert — port contract).
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
        handle.ingest(upsert("approvals", "mine", "{\"org_id\":\"acme\"}", 1));
        handle.ingest(upsert("approvals", "theirs", "{\"org_id\":\"other\"}", 2));
        handle.ingest(upsert("approvals", "bare", "{}", 3));
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
}
