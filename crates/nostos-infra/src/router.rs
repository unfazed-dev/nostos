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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use nostos_application::ports::{DeliveryDecision, EventSink};
use nostos_domain::ReplicationEvent;

/// Default per-session buffer depth. Overridable via `NOSTOS_SESSION_BUFFER`.
pub const DEFAULT_SESSION_BUFFER: usize = 1024;

/// An `EventSink` backed by a bounded tokio channel.
///
/// Cloning a `TokioEventSink` clones the `Sender` (cheap) so the store can hold
/// one and the router another. The `Receiver` lives in the transport task.
pub struct TokioEventSink {
    tx: mpsc::Sender<ReplicationEvent>,
    /// Lifetime open-flag, flipped to false when the transport task ends.
    open: Arc<AtomicBool>,
    /// Counters for observability (exported via metrics in the server).
    delivered: AtomicU64,
    dropped: AtomicU64,
}

impl TokioEventSink {
    /// Create a sink and its draining receiver. `buffer` is the bounded depth.
    #[must_use]
    pub fn channel(buffer: usize) -> (Self, mpsc::Receiver<ReplicationEvent>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        let sink = Self {
            tx,
            open: Arc::new(AtomicBool::new(true)),
            delivered: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        };
        (sink, rx)
    }

    /// Mark this sink as closed (transport task ended). Further deliveries
    /// return `Dropped`.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    #[inline]
    #[must_use]
    pub fn delivered_count(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    #[inline]
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl EventSink for TokioEventSink {
    async fn deliver(&self, event: ReplicationEvent) -> DeliveryDecision {
        if !self.open.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return DeliveryDecision::Dropped;
        }
        // `try_send` is non-blocking — the whole point. A full buffer → drop.
        match self.tx.try_send(event) {
            Ok(()) => {
                self.delivered.fetch_add(1, Ordering::Relaxed);
                DeliveryDecision::Delivered
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                DeliveryDecision::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.open.store(false, Ordering::Release);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                DeliveryDecision::Dropped
            }
        }
    }

    #[inline]
    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

/// A clonable handle the store keeps for a session. Wraps an `Arc<dyn EventSink>`
/// so the router and store share one sink without owning the concrete type.
#[derive(Clone)]
pub struct SessionSinkHandle {
    sink: Arc<dyn EventSink>,
}

impl SessionSinkHandle {
    #[inline]
    #[must_use]
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        Self { sink }
    }

    #[inline]
    #[must_use]
    pub fn sink(&self) -> Arc<dyn EventSink> {
        Arc::clone(&self.sink)
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
        assert_eq!(sink.delivered_count(), 2);
        assert_eq!(sink.dropped_count(), 1);

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
