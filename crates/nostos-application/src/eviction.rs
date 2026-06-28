//! WAL-bloat protection — the cost of ack-driven slot advance, made explicit.
//!
//! ADR-0009 made resume *correct*: the slot's `confirmed_flush_lsn` advances
//! only as far as the slowest live client has acked. The cost: a permanently
//! silent (or pathologically slow) client keeps the slot pinned → unbounded WAL
//! retention on the customer's primary Postgres → disk full → outage.
//!
//! The fix is a **deliberate, documented tradeoff**: if a client lags further
//! than a configurable threshold behind the head of the WAL, the server
//! disconnects it. The client then reconnects and re-syncs from a fresh
//! checkpoint — trading a controlled replay window for source-DB safety.
//!
//! **OFF by default** (per ADR-0016): an unconfigured deploy never evicts, so
//! existing behavior is unchanged and no threshold-guess surprises a benchmark
//! or early adopter. A production deploy MUST set `max_lag` (the threshold is
//! workload-dependent; the default is a placeholder, not a recommendation).

use nostos_domain::Lsn;

/// The WAL-bloat protection policy. Pure logic: decides whether the slowest
/// session should be evicted given the head-of-WAL and the slowest acked LSN.
///
/// Construct with [`Self::disabled`] (the default — no eviction) or
/// [`Self::new`] with an explicit `max_lag`.
#[derive(Debug, Clone, Copy)]
pub struct EvictionPolicy {
    /// The maximum tolerable gap (in WAL bytes / LSN units) between the
    /// head-of-stream and the slowest client's acked LSN. `None` = never evict.
    pub max_lag: Option<u64>,
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

impl EvictionPolicy {
    /// Eviction disabled — the safe default. No client is ever disconnected for
    /// lag. Use this for benchmarks, dev, and any deploy that has NOT set
    /// `max_slot_wal_keep_size` on its replication slot.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { max_lag: None }
    }

    /// Eviction enabled with a lag threshold of `max_lag` LSN-units. When the
    /// gap between `head` and the slowest client's acked LSN exceeds this, the
    /// slowest session is evicted (disconnected; it reconnects + re-syncs).
    #[must_use]
    pub const fn new(max_lag: u64) -> Self {
        Self {
            max_lag: Some(max_lag),
        }
    }

    /// Should the slowest session be evicted?
    ///
    /// - `head` — the highest LSN the fan-out has emitted (head of the stream).
    /// - `slowest_acked` — the minimum acked LSN across live sessions.
    ///
    /// Returns `true` only if eviction is enabled AND the gap exceeds the
    /// threshold. A `None` slowest_acked (no sessions have acked yet) does NOT
    /// evict — the very first events of a fresh connection always lag.
    #[must_use]
    pub fn should_evict(&self, head: Lsn, slowest_acked: Option<Lsn>) -> bool {
        let Some(threshold) = self.max_lag else {
            return false; // disabled
        };
        let Some(slowest) = slowest_acked else {
            return false; // nobody's acked — give them a moment
        };
        // head >= slowest by construction (you can't ack past the head); guard
        // the subtraction against a pathological inversion anyway.
        head.raw().saturating_sub(slowest.raw()) > threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_evicts() {
        let p = EvictionPolicy::disabled();
        assert!(!p.should_evict(Lsn::new(1_000_000), Some(Lsn::new(0))));
    }

    #[test]
    fn default_is_disabled() {
        // The whole point: a fresh deploy with no config never surprises anyone.
        assert_eq!(EvictionPolicy::default().max_lag, None);
    }

    #[test]
    fn enabled_evicts_when_gap_exceeds_threshold() {
        let p = EvictionPolicy::new(1_000);
        // 10_000 head, 5_000 acked → 5_000 gap > 1_000 → evict.
        assert!(p.should_evict(Lsn::new(10_000), Some(Lsn::new(5_000))));
    }

    #[test]
    fn enabled_does_not_evict_within_threshold() {
        let p = EvictionPolicy::new(1_000);
        // 1_500 gap ≤ 1_000 threshold boundary → keep (strictly-greater).
        assert!(!p.should_evict(Lsn::new(10_000), Some(Lsn::new(9_000))));
    }

    #[test]
    fn enabled_does_not_evict_when_nobody_has_acked() {
        // The first events of a fresh connection always lag until the first ack.
        // Evicting here would churn connections forever.
        let p = EvictionPolicy::new(1);
        assert!(!p.should_evict(Lsn::new(1_000_000), None));
    }

    #[test]
    fn threshold_is_strictly_greater() {
        // Exactly at the threshold → NOT evicted (gap > threshold, not >=).
        let p = EvictionPolicy::new(1_000);
        assert!(!p.should_evict(Lsn::new(2_000), Some(Lsn::new(1_000))));
        // One past → evicted.
        assert!(p.should_evict(Lsn::new(2_001), Some(Lsn::new(1_000))));
    }
}
