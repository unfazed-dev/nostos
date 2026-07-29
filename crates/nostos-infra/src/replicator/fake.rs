//! `FakeReplicator` — a synthetic WAL-event generator that implements
//! [`ReplicatorStream`].
//!
//! This is the engine that drives the Week-1 benchmark. It generates
//! `ReplicationEvent`s as fast as the consumer will take them (or at a
//! configured rate), modeling a realistic Postgres logical-replication stream:
//!
//! - 80% Insert / 15% Update / 5% Delete (typical append-heavy app).
//! - Monotonically increasing LSNs.
//! - Configurable payload size (`small` ≈ 100 B, `large` ≈ 4 KB) to expose any
//!   per-byte copy cliffs.
//! - A configurable table name (default `tasks`).
//!
//! **Why not a real Postgres for Week 1?** A real PG at ~60 txn/sec would
//! *itself* be the bottleneck — we'd be benchmarking PG, not Nostos. The fake
//! generates faster than the router can push, so the measured ceiling is the
//! router's. See `WEEK-01-PLAN.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use nostos_application::ports::ReplicatorStream;
use nostos_domain::{Lsn, Operation, ReplicationEvent, RowOp};

/// Default deterministic seed for the synthetic generator. Keeps benchmark
/// runs reproducible. (0xCA110_5eed — "call seed", mnemonic, all valid hex.)
const DEFAULT_SEED: u64 = 0xCA11_05EED;

/// Configuration for the synthetic generator.
#[derive(Debug, Clone)]
pub struct FakeReplicatorConfig {
    /// How many events to emit before the stream returns `None` (clean end).
    /// `usize::MAX` = effectively unbounded.
    pub total_events: u64,
    /// Payload byte length for Insert/Update events.
    pub payload_size: usize,
    /// Table name for generated events.
    pub table: String,
    /// Seed for deterministic generation (so runs are reproducible).
    pub seed: u64,
    /// Emit at most this many events per second. `0` = unbounded (the
    /// benchmark default — pacing would cap the very ceiling we measure).
    /// Set it for interactive/dev servers, where an unbounded generator is
    /// pure load with no observer (ADR-0027 finding, A10).
    pub events_per_sec: u64,
    /// Recycle primary keys over this many distinct values. `0` = monotonic
    /// (`pk = emitted + 1`, so the table grows forever).
    ///
    /// Client apply is an upsert (`ON CONFLICT(table_name, pk) DO UPDATE`,
    /// `sqlite.rs`), so a bounded key space means a bounded *table*. That is
    /// what keeps a full-table watch snapshot O(1) in session length instead
    /// of O(events) — pacing alone only slows the growth.
    pub distinct_keys: u64,
}

impl FakeReplicatorConfig {
    /// Small-row workload (~100 B) — the PowerSync "small row" regime.
    #[must_use]
    pub fn small(total: u64) -> Self {
        Self {
            total_events: total,
            payload_size: 100,
            table: "tasks".into(),
            seed: DEFAULT_SEED,
            events_per_sec: 0,
            distinct_keys: 0,
        }
    }

    /// Large-row workload (~4 KB) — exposes per-byte copy cliffs.
    #[must_use]
    pub fn large(total: u64) -> Self {
        Self {
            total_events: total,
            payload_size: 4096,
            table: "tasks".into(),
            seed: DEFAULT_SEED,
            events_per_sec: 0,
            distinct_keys: 0,
        }
    }

    /// Cap the emission rate (events/second). `0` restores unbounded.
    #[must_use]
    pub fn paced(mut self, events_per_sec: u64) -> Self {
        self.events_per_sec = events_per_sec;
        self
    }

    /// Recycle primary keys over `n` distinct values, bounding the table the
    /// stream produces. `0` restores the monotonic (ever-growing) key space.
    #[must_use]
    pub fn recycling_keys(mut self, n: u64) -> Self {
        self.distinct_keys = n;
        self
    }
}

impl Default for FakeReplicatorConfig {
    fn default() -> Self {
        Self::small(100_000)
    }
}

/// A synthetic replication stream. Cheap to clone (shares an atomic counter) so
/// the benchmark can drive it from one task while reading state from another.
pub struct FakeReplicator {
    cfg: FakeReplicatorConfig,
    /// How many events we've emitted so far.
    emitted: Arc<AtomicU64>,
    /// The next LSN to stamp on an event. LSNs advance by ~10 per op (rough
    /// model of WAL growth; exact value doesn't affect throughput).
    next_lsn: Arc<AtomicU64>,
    /// PRNG state (xorshift64) — deterministic from `cfg.seed`.
    rng_state: Arc<AtomicU64>,
}

impl FakeReplicator {
    #[must_use]
    pub fn new(cfg: FakeReplicatorConfig) -> Self {
        let seed = cfg.seed | 1; // must be nonzero for xorshift
        Self {
            cfg,
            emitted: Arc::new(AtomicU64::new(0)),
            next_lsn: Arc::new(AtomicU64::new(1)),
            rng_state: Arc::new(AtomicU64::new(seed)),
        }
    }

    /// How many events have been emitted so far.
    #[inline]
    #[must_use]
    pub fn emitted_count(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }

    /// Deterministic xorshift64 — reproducible across runs.
    fn next_rand(&self) -> u64 {
        loop {
            let current = self.rng_state.load(Ordering::Relaxed);
            let mut x = current;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if self
                .rng_state
                .compare_exchange(current, x, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return x;
            }
        }
    }

    /// Decide the operation kind from a random u64: 80% insert / 15% update / 5% delete.
    fn pick_op(r: u64) -> Operation {
        let bucket = r % 100;
        if bucket < 80 {
            Operation::Insert
        } else if bucket < 95 {
            Operation::Update
        } else {
            Operation::Delete
        }
    }

    fn make_payload(&self) -> Bytes {
        // Deterministic filler — content doesn't matter for throughput, only size.
        let mut v = vec![0u8; self.cfg.payload_size];
        let mut fill = self.next_rand();
        for b in &mut v {
            *b = (fill & 0xFF) as u8;
            fill = fill
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
        }
        Bytes::from(v)
    }

    fn next_event_inner(&self) -> Option<ReplicationEvent> {
        let emitted = self.emitted.fetch_add(1, Ordering::Relaxed);
        if emitted >= self.cfg.total_events {
            // Undo the increment so `emitted_count` stays honest at the cap.
            self.emitted.fetch_sub(1, Ordering::Relaxed);
            return None;
        }

        let lsn = Lsn::new(self.next_lsn.fetch_add(10, Ordering::Relaxed));
        let r = self.next_rand();
        let pk = if self.cfg.distinct_keys == 0 {
            emitted + 1
        } else {
            emitted % self.cfg.distinct_keys + 1
        }
        .to_string();
        let op = match Self::pick_op(r) {
            Operation::Insert => RowOp::Insert {
                table: self.cfg.table.clone(),
                pk,
                payload: self.make_payload(),
            },
            Operation::Update => RowOp::Update {
                table: self.cfg.table.clone(),
                pk,
                payload: self.make_payload(),
            },
            Operation::Delete => RowOp::Delete {
                table: self.cfg.table.clone(),
                pk,
                old_payload: None,
            },
        };
        // Group events into transactions of 8 — mirrors what PgReplicator stamps
        // from real Begin/Commit boundaries, so dedup/resume tests exercise the
        // txn_id path against the fake (ADR-0009). txn id = floor(emitted / 8).
        let txn_id = emitted / 8;
        Some(ReplicationEvent::new(lsn, op).with_txn(txn_id))
    }
}

#[async_trait]
impl ReplicatorStream for FakeReplicator {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        // Unpaced: no I/O — yield immediately. The router's backpressure
        // (bounded sinks) is what naturally rate-limits us to the sustainable
        // throughput. This is the benchmark path.
        //
        // ponytail: pacing is a per-event sleep, so the real rate is
        // `min(events_per_sec, 1s / timer_granularity)` (~1 kHz on tokio).
        // Good enough for dev/demo; batch-and-sleep if a paced load test ever
        // needs a precise high rate.
        // `checked_div` is the `events_per_sec == 0` (unbounded) branch.
        if let Some(nanos) = 1_000_000_000_u64.checked_div(self.cfg.events_per_sec) {
            tokio::time::sleep(std::time::Duration::from_nanos(nanos)).await;
        }
        self.next_event_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_exactly_total_then_ends() {
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(10));
        let mut count = 0;
        while r.next_event().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 10);
        assert_eq!(r.emitted_count(), 10);
    }

    #[tokio::test]
    async fn lsns_are_monotonic() {
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(5));
        let mut prev = Lsn::ZERO;
        for _ in 0..5 {
            let e = r.next_event().await.unwrap();
            assert!(e.lsn > prev, "lsn must increase: {} > {}", e.lsn, prev);
            prev = e.lsn;
        }
    }

    #[tokio::test]
    async fn operation_distribution_is_roughly_correct() {
        // With 10k events, 80/15/5 should hold within a few percent.
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(10_000));
        let (mut ins, mut upd, mut del) = (0u64, 0u64, 0u64);
        while let Some(e) = r.next_event().await {
            match e.op.operation() {
                Operation::Insert => ins += 1,
                Operation::Update => upd += 1,
                Operation::Delete => del += 1,
            }
        }
        assert!((7800..=8200).contains(&ins), "inserts: {ins}");
        assert!((1300..=1700).contains(&upd), "updates: {upd}");
        assert!((300..=700).contains(&del), "deletes: {del}");
    }

    #[tokio::test]
    async fn payload_size_matches_config() {
        let mut r = FakeReplicator::new(FakeReplicatorConfig::large(3));
        while let Some(e) = r.next_event().await {
            if e.op.has_payload() {
                assert_eq!(e.payload_len(), 4096);
            }
        }
    }

    #[tokio::test]
    async fn recycling_keys_bounds_the_key_space() {
        // A10: client apply is an upsert on (table, pk), so a bounded key space
        // bounds the *table* — that is what keeps a full-table watch snapshot
        // O(1) in session length instead of O(events).
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(500).recycling_keys(10));
        let mut keys = std::collections::HashSet::new();
        while let Some(e) = r.next_event().await {
            keys.insert(e.op.pk().to_string());
        }
        assert_eq!(keys.len(), 10, "keys: {keys:?}");
    }

    #[tokio::test]
    async fn pacing_throttles_emission() {
        // Real time, but only ~50 ms of it: sleeps overshoot, never undershoot,
        // so a floor assert can't flake. (`start_paused` would need tokio's
        // `test-util` feature — not worth a dep for a 50 ms test.)
        let start = std::time::Instant::now();
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(5).paced(100));
        while r.next_event().await.is_some() {}
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(30),
            "elapsed: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn deterministic_across_runs() {
        // Same seed → same first 100 events.
        let cfg = FakeReplicatorConfig::small(100);
        let mut a = FakeReplicator::new(cfg.clone());
        let mut b = FakeReplicator::new(cfg);
        for _ in 0..100 {
            let ea = a.next_event().await.unwrap();
            let eb = b.next_event().await.unwrap();
            assert_eq!(ea.lsn, eb.lsn);
            assert_eq!(ea.op.pk(), eb.op.pk());
        }
    }
}
