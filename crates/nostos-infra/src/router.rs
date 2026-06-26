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

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;

use nostos_application::ports::{DeliveryDecision, EventSink};
use nostos_domain::ReplicationEvent;

/// An `EventSink` backed by a bounded tokio channel.
///
/// Cloning a `TokioEventSink` clones the `Sender` (cheap) so the store can hold
/// one and the router another. The `Receiver` lives in the transport task.
pub struct TokioEventSink {
    tx: mpsc::Sender<ReplicationEvent>,
    /// Lifetime open-flag, flipped to false when the transport task ends.
    open: AtomicBool,
}

impl TokioEventSink {
    /// Create a sink and its draining receiver. `buffer` is the bounded depth.
    #[must_use]
    pub fn channel(buffer: usize) -> (Self, mpsc::Receiver<ReplicationEvent>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        let sink = Self {
            tx,
            open: AtomicBool::new(true),
        };
        (sink, rx)
    }

    /// Mark this sink as closed (transport task ended). Further deliveries
    /// return `Dropped`.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }
}

#[async_trait]
impl EventSink for TokioEventSink {
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
        if !self.open.load(Ordering::Acquire) {
            return DeliveryDecision::Dropped;
        }
        // `try_send` is non-blocking — the whole point. A full buffer → drop.
        match self.tx.try_send(event) {
            Ok(()) => DeliveryDecision::Delivered,
            Err(mpsc::error::TrySendError::Full(_)) => DeliveryDecision::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.open.store(false, Ordering::Release);
                DeliveryDecision::Dropped
            }
        }
    }

    #[inline]
    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
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
        assert!(!sink.is_open());
    }

    #[tokio::test]
    async fn receiver_dropped_marks_closed() {
        let (sink, rx) = TokioEventSink::channel(8);
        drop(rx);
        // After receiver is gone, sender reports closed → drop.
        assert_eq!(sink.deliver(ev(1)).await, DeliveryDecision::Dropped);
        assert!(!sink.is_open());
    }
}
