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
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::time::Instant;

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
    /// Emit at this many events per second, on an OPEN-LOOP schedule: event
    /// `i` is due at `first_call + i/R` regardless of how long the consumer
    /// took for event `i-1`. `0` = unbounded (the ceiling-measuring default —
    /// pacing would cap the very thing we measure).
    ///
    /// Open-loop is the load-testing contract: a generator that waits for the
    /// consumer measures the consumer's pace and calls it the arrival rate.
    /// See [`FakeReplicator::max_lateness`] for how a run that failed to hold
    /// the rate announces itself.
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
    /// Origin of the open-loop emission schedule, set on the first paced call.
    /// A `tokio::time::Instant` so `start_paused` tests drive it deterministically.
    schedule_origin: Option<Instant>,
    /// Largest observed slip behind that schedule, nanoseconds. Shared so the
    /// benchmark can read it after moving the replicator into its own task.
    max_lateness_nanos: Arc<AtomicU64>,
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
            schedule_origin: None,
            max_lateness_nanos: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many events have been emitted so far.
    #[inline]
    #[must_use]
    pub fn emitted_count(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }

    /// Largest gap between an event's scheduled emit time and its actual one.
    ///
    /// Zero for an unpaced run, and zero for a paced run the generator kept up
    /// with. Non-zero means the offered load fell BELOW the requested rate, so
    /// any drop rate measured in that run describes the generator rather than
    /// the system under test — the one failure mode an open-loop harness must
    /// never report as a result.
    #[must_use]
    pub fn max_lateness(&self) -> Duration {
        Duration::from_nanos(self.max_lateness_nanos.load(Ordering::Relaxed))
    }

    /// Shared handle to the lateness counter, readable after the replicator has
    /// been moved into a fan-out task (which is where a benchmark drives it).
    #[must_use]
    pub fn lateness_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.max_lateness_nanos)
    }

    /// Hold the open-loop schedule: event `i` is due at `origin + i/R`, the
    /// origin being the first paced call. Early → sleep to the due time.
    /// Late → emit immediately and record the slip.
    ///
    /// That distinction is the whole point. Pacing used to be a `sleep(1/R)`
    /// *after* each event, so a consumer taking `d` per event dropped the real
    /// rate to `1/(1/R + d)`: the generator quietly slowed to whatever the
    /// system could absorb, and then reported no problem. That is coordinated
    /// omission (Gil Tene, "How Not to Measure Latency") — the measurement
    /// skips exactly the intervals where the system was struggling. A fixed
    /// schedule cannot slow down; it accumulates a backlog, which is what a
    /// real arrival process does.
    ///
    /// It also retires the old ~1 kHz ceiling: behind schedule we never sleep,
    /// so catching up is a burst instead of a chain of per-event timers.
    async fn await_schedule(&mut self) {
        let rate = u128::from(self.cfg.events_per_sec);
        let Some(origin) = self.schedule_origin else {
            // Event 0 defines the origin, so it cannot be late against it.
            // Recording its sub-microsecond offset would put a floor under
            // `max_lateness` and make "the schedule was kept" unexpressible.
            self.schedule_origin = Some(Instant::now());
            return;
        };
        let offset = u128::from(self.emitted.load(Ordering::Relaxed)) * 1_000_000_000 / rate;
        let due = origin + Duration::from_nanos(u64::try_from(offset).unwrap_or(u64::MAX));
        let now = Instant::now();
        if now < due {
            tokio::time::sleep_until(due).await;
        } else {
            let late = u64::try_from((now - due).as_nanos()).unwrap_or(u64::MAX);
            self.max_lateness_nanos.fetch_max(late, Ordering::Relaxed);
        }
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
        // Unpaced (`events_per_sec == 0`): no I/O at all. The router's
        // backpressure (bounded sinks) is what rate-limits us to the
        // sustainable throughput — that is the ceiling-measuring path, and
        // any pacing here would cap the ceiling being measured.
        if self.cfg.events_per_sec > 0 {
            self.await_schedule().await;
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

    /// Coordinated-omission guard: a consumer stall must not slow the
    /// generator's schedule, only build a backlog it then bursts through.
    ///
    /// 100 events/sec is one every 10 ms. The consumer stalls 200 ms after the
    /// first event, so events 1..=20 are all overdue when it returns and must
    /// go out at once. Closed-loop pacing — `sleep(1/R)` *after* each event —
    /// would take 200 ms + 20x10 ms and report a rate of ~52/sec while
    /// claiming 100/sec was offered. Open-loop finishes in ~200 ms and records
    /// the slip instead of absorbing it.
    #[tokio::test]
    async fn a_stalled_consumer_does_not_slow_the_schedule() {
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(21).paced(100));
        let start = std::time::Instant::now();
        assert!(r.next_event().await.is_some());
        tokio::time::sleep(Duration::from_millis(200)).await;
        while r.next_event().await.is_some() {}

        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(300),
            "the schedule slipped with the consumer (closed-loop pacing): {elapsed:?}"
        );
        assert!(
            r.max_lateness() >= Duration::from_millis(100),
            "a 200 ms stall must be recorded as slip, not absorbed: {:?}",
            r.max_lateness()
        );
    }

    /// The other half: a generator that keeps up reports exactly zero slip, so
    /// `max_lateness > 0` is usable as "this run did not offer the rate it
    /// claimed" without a fudge threshold.
    #[tokio::test]
    async fn a_kept_schedule_records_no_lateness() {
        let mut r = FakeReplicator::new(FakeReplicatorConfig::small(4).paced(50));
        while r.next_event().await.is_some() {}
        assert_eq!(r.max_lateness(), Duration::ZERO);
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
