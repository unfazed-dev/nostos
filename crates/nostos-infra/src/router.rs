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
use std::sync::{Arc, Mutex};

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

/// What the per-session sink channel carries: a replication event OR a
/// pre-encoded control frame (snapshot boundary). Sharing ONE FIFO channel for
/// both is what lets the writer preserve `snapshot_begin → rows → snapshot_end`
/// ordering on the wire (ADR-0025 hole #2) — two separate channels let the
/// writer's `select!` reorder them.
#[derive(Debug, Clone)]
pub enum SinkMsg {
    /// A server-originated replication row (deduped + range-guarded by `deliver`).
    /// Shared with every other session's queue — the writer encodes through
    /// `&*event`; never clone the inner event here.
    Event(Arc<ReplicationEvent>),
    /// A pre-encoded control frame (snapshot boundary). NOT deduped — control
    /// frames carry no LSN; they bracket a snapshot burst, ordering is all that
    /// matters.
    Control(Vec<u8>),
}

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
    tx: mpsc::Sender<SinkMsg>,
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
    /// Capacity sheds: events dropped because the buffer was FULL (ADR-0040
    /// loss signal). Deliberately EXCLUDES dedup drops and closed-sink drops
    /// — only capacity loss tells a client its stream has a gap.
    capacity_sheds: AtomicU64,
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
    pub fn channel(buffer: usize) -> (Self, mpsc::Receiver<SinkMsg>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        let sink = Self {
            tx,
            open: AtomicBool::new(true),
            acked_lsn: AtomicU64::new(0),
            delivered_lsn: AtomicU64::new(0),
            dedup: Mutex::new(DedupRing::new()),
            capacity_sheds: AtomicU64::new(0),
        };
        (sink, rx)
    }

    /// Capacity-shed count for this sink (ADR-0040 continuity signal). The
    /// transport's writer loop watches the delta and emits `resync_required`.
    #[must_use]
    pub fn capacity_sheds(&self) -> u64 {
        self.capacity_sheds.load(Ordering::Relaxed)
    }

    /// Mark this sink as closed (transport task ended). Further deliveries
    /// return `Dropped` — see the module doc on the teardown race this closes.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    /// Record a client ACK: the highest LSN the client has applied. Monotonic
    /// (a lower LSN is ignored). Called by the transport's ACK-reader task.
    ///
    /// # Clamped to what was actually delivered
    ///
    /// The ack frame is client-controlled, so a client can name any LSN —
    /// including `u64::MAX`. Nothing checked it against reality, so an ack of
    /// data the client was never sent was accepted verbatim.
    ///
    /// **This is defense-in-depth, not a leak fix — be precise about it.** A
    /// high ack cannot expose another tenant's rows: slot advance folds the
    /// *minimum* acked LSN across all live sessions (`SessionStore::
    /// min_acked_lsn`), so one session's inflated ack can never flush the slot
    /// past data another session still needs. What it *does* is wedge
    /// `admit`'s acked-range guard permanently shut, so the session receives
    /// nothing further and cannot tell why. That is a client harming only
    /// itself — which is exactly the kind of thing that should be impossible
    /// rather than merely unrewarding, because "you can only hurt yourself"
    /// stops being true the moment a sink is shared (one socket's N table
    /// sessions already share this one).
    pub fn record_ack(&self, lsn: Lsn) {
        // Clamp FIRST: you may not acknowledge what you were never sent.
        // `delivered_lsn` is a high-water mark (`fetch_max`), so a snapshot
        // row carrying a lower base LSN than live traffic already delivered
        // cannot drag the ceiling down and clamp a legitimate ack.
        let new = lsn.raw().min(self.delivered_lsn.load(Ordering::Acquire));
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

    /// Deliver a pre-encoded control frame (snapshot boundary) on the SAME FIFO
    /// channel as events. NOT deduped — control frames carry no LSN; their only
    /// invariant is ordering relative to the snapshot rows, which the shared
    /// channel guarantees (ADR-0025 hole #2). Best-effort like `deliver`: a full
    /// buffer drops the boundary (the client keeps stale rows; no partial
    /// reconcile).
    pub fn deliver_control(&self, bytes: Vec<u8>) -> DeliveryDecision {
        if !self.open.load(Ordering::Acquire) {
            return DeliveryDecision::Dropped;
        }
        match self.tx.try_send(SinkMsg::Control(bytes)) {
            Ok(()) => DeliveryDecision::Delivered,
            Err(mpsc::error::TrySendError::Full(_)) => DeliveryDecision::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.open.store(false, Ordering::Release);
                DeliveryDecision::Dropped
            }
        }
    }

    /// Shared admit gate for [`EventSink::deliver`] + [`Self::deliver_awaiting`]:
    /// open check, acked-range guard, dedup ring. Returns the LSN to record on a
    /// successful send, or `None` if the event is dropped (closed / already
    /// acked / dedup hit). Factored so the two delivery paths can't drift on the
    /// gate logic.
    fn admit(&self, event: &ReplicationEvent) -> Option<u64> {
        if !self.open.load(Ordering::Acquire) {
            return None;
        }
        let lsn_raw = event.lsn.raw();
        let acked = self.acked_lsn.load(Ordering::Acquire);
        if lsn_raw <= acked && acked != 0 {
            return None;
        }
        if let Ok(mut ring) = self.dedup.lock() {
            if ring.record(lsn_raw) {
                return None;
            }
        }
        Some(lsn_raw)
    }

    /// Backpressure-aware delivery for the snapshot burst: AWAITS when the
    /// buffer is full instead of dropping (ADR-0025 residual fix — a snapshot
    /// truncated by sink backpressure corrupts the reconcile: `end` would reap
    /// the dropped rows' pks even though the server still has them). Gate logic
    /// identical to `deliver` via [`Self::admit`]; live fan-out keeps `deliver`
    /// (a dropped live event is acceptable; a dropped snapshot row is not).
    pub async fn deliver_awaiting(&self, event: ReplicationEvent) -> DeliveryDecision {
        let Some(lsn_raw) = self.admit(&event) else {
            return DeliveryDecision::Dropped;
        };
        if let Ok(()) = self.tx.send(SinkMsg::Event(Arc::new(event))).await {
            self.delivered_lsn.fetch_max(lsn_raw, Ordering::Release);
            DeliveryDecision::Delivered
        } else {
            // `send().await` only errors on a closed channel (writer exited /
            // client gone) — never on a full buffer (it awaits).
            self.open.store(false, Ordering::Release);
            DeliveryDecision::Dropped
        }
    }
}

#[async_trait]
impl EventSink for TokioEventSink {
    async fn deliver(&self, event: Arc<ReplicationEvent>) -> DeliveryDecision {
        let Some(lsn_raw) = self.admit(&event) else {
            return DeliveryDecision::Dropped;
        };
        // `try_send` is non-blocking — the whole point for live fan-out. A full
        // buffer → drop. (The snapshot uses `deliver_awaiting` so it is never
        // truncated by backpressure; a dropped live event is acceptable, a
        // dropped snapshot row is not.)
        match self.tx.try_send(SinkMsg::Event(event)) {
            Ok(()) => {
                self.delivered_lsn.fetch_max(lsn_raw, Ordering::Release);
                DeliveryDecision::Delivered
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.capacity_sheds.fetch_add(1, Ordering::Relaxed);
                DeliveryDecision::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.open.store(false, Ordering::Release);
                DeliveryDecision::Dropped
            }
        }
    }

    fn close(&self) {
        // Set the open flag to false so admit() drops all frames.
        self.open.store(false, Ordering::Release);
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
    use std::sync::Arc;

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
        assert_eq!(
            sink.deliver(Arc::new(ev(1))).await,
            DeliveryDecision::Delivered
        );
        assert_eq!(
            sink.deliver(Arc::new(ev(2))).await,
            DeliveryDecision::Delivered
        );
        // Buffer full now.
        assert_eq!(
            sink.deliver(Arc::new(ev(3))).await,
            DeliveryDecision::Dropped
        );

        // Drain one → next send succeeds again.
        rx.recv().await.unwrap();
        assert_eq!(
            sink.deliver(Arc::new(ev(4))).await,
            DeliveryDecision::Delivered
        );
    }

    #[tokio::test]
    async fn closed_sink_drops_everything() {
        let (sink, _rx) = TokioEventSink::channel(8);
        sink.close();
        assert_eq!(
            sink.deliver(Arc::new(ev(1))).await,
            DeliveryDecision::Dropped
        );
    }

    #[tokio::test]
    async fn control_frames_share_one_fifo_channel_with_events() {
        // ADR-0025 hole #2: snapshot boundaries MUST share the sink's FIFO
        // channel with the snapshot rows (not a separate channel the writer
        // `select!`s against) so the writer can't land `begin` after early rows.
        // This asserts the channel-level invariant — begin, rows, end come out
        // in delivery order from the ONE receiver — which the writer's
        // stop-at-Control batching (transport.rs) then preserves on the wire.
        // If a future change reroutes boundaries to a second channel, this fails.
        let (sink, mut rx) = TokioEventSink::channel(16);
        assert_eq!(
            sink.deliver_control(b"begin".to_vec()),
            DeliveryDecision::Delivered
        );
        assert_eq!(
            sink.deliver(Arc::new(ev(1))).await,
            DeliveryDecision::Delivered
        );
        assert_eq!(
            sink.deliver(Arc::new(ev(2))).await,
            DeliveryDecision::Delivered
        );
        assert_eq!(
            sink.deliver_control(b"end".to_vec()),
            DeliveryDecision::Delivered
        );

        // Drain in FIFO order: begin, e1, e2, end.
        let mut order = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                SinkMsg::Control(b) => {
                    order.push(format!("control({})", String::from_utf8_lossy(&b)));
                }
                SinkMsg::Event(e) => order.push(format!("event({})", e.lsn.raw())),
            }
        }
        assert_eq!(
            order,
            vec![
                "control(begin)".to_string(),
                "event(1)".to_string(),
                "event(2)".to_string(),
                "control(end)".to_string(),
            ],
            "begin/rows/end must share one FIFO channel (ADR-0025 hole #2)"
        );
    }

    #[tokio::test]
    async fn deliver_awaiting_blocks_on_full_buffer_then_delivers() {
        // ADR-0025 residual: snapshot rows use `deliver_awaiting` (backpressure-
        // aware) so a snapshot is never truncated by sink backpressure. A full
        // buffer must BLOCK until drained, not drop — the snapshot's
        // completeness (and thus the reconcile's correctness) depends on it.
        // (`deliver` drops on full; `deliver_awaiting` awaits — the difference
        // this test pins.)
        let (sink, mut rx) = TokioEventSink::channel(1);
        let sink = Arc::new(sink);
        // Fill the 1-deep buffer.
        assert_eq!(
            sink.deliver_awaiting(ev(1)).await,
            DeliveryDecision::Delivered
        );
        // The next deliver_awaiting must block (buffer full), not drop.
        let sink2 = Arc::clone(&sink);
        let handle = tokio::spawn(async move { sink2.deliver_awaiting(ev(2)).await });
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "deliver_awaiting must block on a full buffer (not drop) — else the snapshot truncates"
        );
        // Drain one → the blocked deliver_awaiting completes + delivers.
        rx.recv().await.unwrap();
        assert_eq!(handle.await.unwrap(), DeliveryDecision::Delivered);
    }

    #[tokio::test]
    async fn receiver_dropped_marks_closed() {
        let (sink, rx) = TokioEventSink::channel(8);
        drop(rx);
        // After receiver is gone, try_send reports Closed → drop.
        assert_eq!(
            sink.deliver(Arc::new(ev(1))).await,
            DeliveryDecision::Dropped
        );
    }

    #[tokio::test]
    async fn duplicate_lsn_is_dropped_by_dedup_ring() {
        // Same LSN delivered twice → second is a dedup hit (Dropped), even
        // though the buffer has room. The primary exactly-once guard is
        // LSN-resume; this ring is defense-in-depth (ADR-0009).
        let (sink, _rx) = TokioEventSink::channel(8);
        assert_eq!(
            sink.deliver(Arc::new(ev(5))).await,
            DeliveryDecision::Delivered
        );
        assert_eq!(
            sink.deliver(Arc::new(ev(5))).await,
            DeliveryDecision::Dropped
        );
    }

    #[tokio::test]
    async fn ack_advances_acked_lsn_monotonically() {
        let (sink, _rx) = TokioEventSink::channel(8);
        // No ack yet → None.
        assert_eq!(EventSink::last_acked_lsn(&sink), None);
        // Acks are clamped to what was delivered, so deliver first — this
        // test is about MONOTONICITY, and the ceiling is covered below.
        sink.deliver(Arc::new(ev(200))).await;
        sink.record_ack(Lsn::new(100));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(100)));
        // Lower ack ignored (monotonic).
        sink.record_ack(Lsn::new(50));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(100)));
        // Higher ack advances.
        sink.record_ack(Lsn::new(200));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(200)));
    }

    /// A client cannot acknowledge data it was never sent.
    ///
    /// Defense-in-depth, NOT a cross-tenant fix: slot advance folds the
    /// minimum acked LSN across sessions, so an inflated ack can't flush the
    /// slot past another session's data. What it can do is jam this session's
    /// own `admit` acked-range guard shut forever — silent, and undiagnosable
    /// from the client side.
    #[tokio::test]
    async fn ack_is_clamped_to_what_was_actually_delivered() {
        let (sink, _rx) = TokioEventSink::channel(8);

        // Nothing delivered → nothing is ackable, however large the claim.
        sink.record_ack(Lsn::new(u64::MAX));
        assert_eq!(
            EventSink::last_acked_lsn(&sink),
            None,
            "an ack before any delivery must not register"
        );

        // Deliver 10; an ack of u64::MAX may only count as far as 10.
        sink.deliver(Arc::new(ev(10))).await;
        sink.record_ack(Lsn::new(u64::MAX));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(10)));

        // And the session is NOT wedged: event 11 still gets through. Before
        // the clamp, acked=u64::MAX made `admit` drop everything forever.
        assert_eq!(
            sink.deliver(Arc::new(ev(11))).await,
            DeliveryDecision::Delivered
        );
    }

    /// `delivered_lsn` is a high-water mark, not "most recent". A snapshot row
    /// carries a LOWER base LSN than live traffic already delivered (live
    /// fan-out starts before the snapshot query), so a plain store would let
    /// the ceiling regress and clamp a legitimate ack back down.
    #[tokio::test]
    async fn delivered_lsn_does_not_regress_when_a_lower_lsn_arrives_later() {
        let (sink, _rx) = TokioEventSink::channel(8);
        sink.deliver(Arc::new(ev(100))).await;
        sink.deliver(Arc::new(ev(7))).await; // late snapshot row at a lower base LSN
        assert_eq!(
            EventSink::last_delivered_lsn(&sink),
            Some(Lsn::new(100)),
            "high-water mark must survive an out-of-order lower delivery"
        );
        sink.record_ack(Lsn::new(100));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(100)));
    }

    #[tokio::test]
    async fn seed_acked_lsn_sets_both_cursors() {
        let (sink, _rx) = TokioEventSink::channel(8);
        sink.seed_acked_lsn(Lsn::new(42));
        assert_eq!(EventSink::last_acked_lsn(&sink), Some(Lsn::new(42)));
        assert_eq!(EventSink::last_delivered_lsn(&sink), Some(Lsn::new(42)));
        // A resume-seeded sink won't re-receive already-applied LSNs.
        assert_eq!(
            sink.deliver(Arc::new(ev(42))).await,
            DeliveryDecision::Dropped
        );
    }

    #[tokio::test]
    async fn deliver_records_delivered_lsn() {
        let (sink, _rx) = TokioEventSink::channel(8);
        assert_eq!(EventSink::last_delivered_lsn(&sink), None);
        sink.deliver(Arc::new(ev(7))).await;
        assert_eq!(EventSink::last_delivered_lsn(&sink), Some(Lsn::new(7)));
    }
}
