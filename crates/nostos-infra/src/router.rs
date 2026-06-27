//! The per-session delivery sink — bounded channel with **drop-on-full**
//! backpressure.
//!
//! This is the honesty mechanism. Each connected client gets a bounded
//! [`mpsc::Sender`] of depth `B` (configured via `NOSTOS_SESSION_BUFFER`).
//! When the router tries to deliver to a client whose buffer is full (a slow
//! or stalled consumer), it **drops** that event for that client and the
//! `deliver()` call returns `DeliveryDecision::Dropped` — counted, not silent.
//!
//! Why drop-and-observe instead of block? A single stalled WebSocket must
//! **never** stall the replication fan-out (head-of-line blocking). PowerSync's
//! full-reprocessing model (their proposal #349) doesn't have this guarantee.
//! See `BENCHMARK-METHODOLOGY.md` §5 for the contract.
//!
//! The receiver half is drained by the transport adapter (one task per
//! WebSocket connection) which serializes events onto the wire.
//!
//! ## The `open` flag and the teardown race
//!
//! `TokioEventSink` carries an `open` flag flipped by [`TokioEventSink::close`].
//! It is not redundant with `mpsc`'s `Closed` signal: in the transport's
//! teardown the session stays registered in the store and the `Receiver` is
//! owned by a completed-but-unreaped drain task, so a concurrent `deliver()`
//! in that window would `try_send` into a buffer nobody will drain. The flag
//! makes such delivers return `Dropped` instead of silently buffering to a
//! dead client.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::mpsc;

use nostos_application::ports::{DeliveryDecision, EventSink};
use nostos_domain::{Lsn, ReplicationEvent};

/// Capacity of the per-session delivered-LSN dedup ring (ADR-0009).
///
/// Events are idempotent at apply (Insert/Update = upsert by pk, Delete is
/// idempotent), so the *primary* exactly-once mechanism is LSN-resume, not a
/// dedup window — a per-session ring does not survive reconnect anyway. This
/// ring is cheap defense-in-depth against intra-connection double-delivery
/// from any fan-out race (a 256-entry ring covers a full burst).
const DEDUP_RING_CAPACITY: usize = 256;

/// An `EventSink` backed by a bounded tokio channel.
///
/// Cloning a `TokioEventSink` clones the `Sender` (cheap) so the store can hold
/// one and the router another. The `Receiver` lives in the transport task.
///
/// Carries three pieces of per-session state beyond the channel:
/// - `open` — lifetime flag (see module doc on the teardown race).
/// - `acked_lsn` — highest LSN the client confirmed via an ACK frame; read by
///   the store's `min_acked_lsn` to drive ack-driven slot advance (ADR-0009).
/// - `delivered_lsn` + `dedup` — highest delivered LSN and a small ring of
///   recently-delivered LSNs (defense-in-depth against double-delivery).
pub struct TokioEventSink {
    tx: mpsc::Sender<ReplicationEvent>,
    /// Lifetime open-flag, flipped to false when the transport task ends.
    open: AtomicBool,
    /// Highest LSN the client ACKed applying. 0 = no ack yet.
    acked_lsn: AtomicU64,
    /// Highest LSN delivered into the buffer. 0 = nothing delivered yet.
    delivered_lsn: AtomicU64,
    /// Ring of recently-delivered LSNs (bounded; std Mutex — scan is ~256
    /// entries and deliveries to one session are already serialized by the
    /// bounded channel, so contention is negligible).
    dedup: Mutex<DedupRing>,
}

/// Fixed-capacity ring of delivered LSNs for intra-connection dedup.
struct DedupRing {
    buf: Box<[u64]>,
    next: usize,
    len: usize,
}

impl DedupRing {
    fn new() -> Self {
        Self {
            buf: vec![0; DEDUP_RING_CAPACITY].into_boxed_slice(),
            next: 0,
            len: 0,
        }
    }

    /// Record `lsn` as delivered and report whether it was already present
    /// (true = duplicate, caller should skip).
    fn record(&mut self, lsn: u64) -> bool {
        let is_dup = self.contains(lsn);
        if !is_dup {
            self.buf[self.next] = lsn;
            self.next = (self.next + 1) % DEDUP_RING_CAPACITY;
            if self.len < DEDUP_RING_CAPACITY {
                self.len += 1;
            }
        }
        is_dup
    }

    fn contains(&self, lsn: u64) -> bool {
        // Linear scan of a 256-entry ring — cheap; called once per deliver.
        self.buf[..self.len].contains(&lsn)
    }
}

impl TokioEventSink {
    /// Create a sink and its draining receiver. `buffer` is the bounded depth.
    #[must_use]
    pub fn channel(buffer: usize) -> (Self, mpsc::Receiver<ReplicationEvent>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        let sink = Self {
            tx,
            open: AtomicBool::new(true),
            acked_lsn: AtomicU64::new(0),
            delivered_lsn: AtomicU64::new(0),
            dedup: Mutex::new(DedupRing::new()),
        };
        (sink, rx)
    }

    /// Mark this sink as closed (transport task ended). Further deliveries
    /// return `Dropped` — see the module doc on the teardown race this closes.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    /// Record a client ACK: the highest LSN the client has applied. Monotonic
    /// (a lower LSN is ignored). Called by the transport's ACK-reader task.
    pub fn record_ack(&self, lsn: Lsn) {
        let new = lsn.raw();
        let mut cur = self.acked_lsn.load(Ordering::Relaxed);
        while new > cur {
            match self.acked_lsn.compare_exchange_weak(
                cur,
                new,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Seed the acked LSN at connect time from the client's resume cursor, so
    /// the slot won't flush past data the reconnecting client has already
    /// applied (and won't re-receive). Used by the transport on resume.
    ///
    /// Also seeds the dedup ring so a resumed session won't re-deliver anything
    /// at or below the resume LSN — the client confirmed it already has those.
    pub fn seed_acked_lsn(&self, lsn: Lsn) {
        let raw = lsn.raw();
        self.acked_lsn.store(raw, Ordering::Release);
        self.delivered_lsn.store(raw, Ordering::Release);
        if let Ok(mut ring) = self.dedup.lock() {
            ring.record(raw);
        }
    }
}

#[async_trait]
impl EventSink for TokioEventSink {
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
        if !self.open.load(Ordering::Acquire) {
            return DeliveryDecision::Dropped;
        }
        let lsn_raw = event.lsn.raw();
        // Resume / ack boundary: if the client already acked past this LSN
        // (incl. on a reconnect that seeded the cursor), don't re-deliver. This
        // is the range guard; the dedup ring below catches exact duplicates
        // above the acked cursor (intra-connection double-delivery).
        let acked = self.acked_lsn.load(Ordering::Acquire);
        if lsn_raw <= acked && acked != 0 {
            return DeliveryDecision::Dropped;
        }
        // Defense-in-depth dedup (ADR-0009): skip if this exact LSN was already
        // delivered to this session (catches fan-out races above the ack cursor).
        if let Ok(mut ring) = self.dedup.lock() {
            if ring.record(lsn_raw) {
                return DeliveryDecision::Dropped;
            }
        }
        // `try_send` is non-blocking — the whole point. A full buffer → drop.
        match self.tx.try_send(event) {
            Ok(()) => {
                self.delivered_lsn.store(lsn_raw, Ordering::Release);
                DeliveryDecision::Delivered
            }
            Err(mpsc::error::TrySendError::Full(_)) => DeliveryDecision::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.open.store(false, Ordering::Release);
                DeliveryDecision::Dropped
            }
        }
    }

    #[inline]
    fn last_acked_lsn(&self) -> Option<Lsn> {
        let v = self.acked_lsn.load(Ordering::Acquire);
        (v != 0).then_some(Lsn::new(v))
    }

    #[inline]
    fn last_delivered_lsn(&self) -> Option<Lsn> {
        let v = self.delivered_lsn.load(Ordering::Acquire);
        (v != 0).then_some(Lsn::new(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_domain::{Lsn, RowOp};

    fn ev(i: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(i),
            RowOp::Insert {
                table: "t".into(),
                pk: i.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    #[tokio::test]
    async fn delivers_until_buffer_full_then_drops() {
        // buffer depth 2 → 3rd send must drop.
        let (sink, mut rx) = TokioEventSink::channel(2);
        assert_eq!(sink.deliver(ev(1)).await, DeliveryDecision::Delivered);
        assert_eq!(sink.deliver(ev(2)).await, DeliveryDecision::Delivered);
        // Buffer full now.
        assert_eq!(sink.deliver(ev(3)).await, DeliveryDecision::Dropped);

        // Drain one → next send succeeds again.
        rx.recv().await.unwrap();
        assert_eq!(sink.deliver(ev(4)).await, DeliveryDecision::Delivered);
    }

    #[tokio::test]
    async fn closed_sink_drops_everything() {
        let (sink, _rx) = TokioEventSink::channel(8);
        sink.close();
        assert_eq!(sink.deliver(ev(1)).await, DeliveryDecision::Dropped);
    }

    #[tokio::test]
    async fn receiver_dropped_marks_closed() {
        let (sink, rx) = TokioEventSink::channel(8);
        drop(rx);
        // After receiver is gone, try_send reports Closed → drop.
        assert_eq!(sink.deliver(ev(1)).await, DeliveryDecision::Dropped);
    }

    #[tokio::test]
    async fn duplicate_lsn_is_dropped_by_dedup_ring() {
        // Same LSN delivered twice → second is a dedup hit (Dropped), even
        // though the buffer has room. The primary exactly-once guard is
        // LSN-resume; this ring is defense-in-depth (ADR-0009).
        let (sink, _rx) = TokioEventSink::channel(8);
        assert_eq!(sink.deliver(ev(5)).await, DeliveryDecision::Delivered);
        assert_eq!(sink.deliver(ev(5)).await, DeliveryDecision::Dropped);
    }

    #[tokio::test]
    async fn ack_advances_acked_lsn_monotonically() {
        let (sink, _rx) = TokioEventSink::channel(8);
        // No ack yet → None.
        assert_eq!(EventSink::last_acked_lsn(&sink), None);
        sink.record_ack(Lsn::new(100));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(100)));
        // Lower ack ignored (monotonic).
        sink.record_ack(Lsn::new(50));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(100)));
        // Higher ack advances.
        sink.record_ack(Lsn::new(200));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(200)));
    }

    #[tokio::test]
    async fn seed_acked_lsn_sets_both_cursors() {
        let (sink, _rx) = TokioEventSink::channel(8);
        sink.seed_acked_lsn(Lsn::new(42));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(42)));
        assert_eq!(EventSink::last_delivered_lsn(&sink), Some(Lsn::new(42)));
        // A resume-seeded sink won't re-receive already-applied LSNs.
        assert_eq!(sink.deliver(ev(42)).await, DeliveryDecision::Dropped);
    }

    #[tokio::test]
    async fn deliver_records_delivered_lsn() {
        let (sink, _rx) = TokioEventSink::channel(8);
        assert_eq!(EventSink::last_delivered_lsn(&sink), None);
        sink.deliver(ev(7)).await;
        assert_eq!(EventSink::last_delivered_lsn(&sink), Some(Lsn::new(7)));
    }
}
