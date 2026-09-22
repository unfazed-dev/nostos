//! A minimal latency histogram for benchmark reporting.
//!
//! Records raw sample values (microseconds) and computes percentiles by sorting
//! on demand. Adequate for Week-1 reporting (up to ~10^7 samples per run); a
//! streaming HDR-histogram can replace it later if memory or sort cost bites.

/// Collects latency samples and computes percentiles.
pub struct Histogram {
    samples: Vec<u64>,
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(1024),
        }
    }

    /// Record one sample (microseconds, must be > 0).
    pub fn record(&mut self, us: u64) {
        self.samples.push(us.max(1));
    }

    /// Merge another histogram into this one.
    pub fn merge(&mut self, other: &Self) {
        self.samples.extend_from_slice(&other.samples);
    }

    /// Percentile in microseconds. `p` is in `[0.0, 1.0]`. Returns 0.0 if empty.
    ///
    /// Nearest-rank: index = floor(p * (n-1)). Simple and correct for Week-1
    /// sample sizes (percentiles are queried a handful of times at end-of-run).
    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let p = p.clamp(0.0, 1.0);
        let mut sorted: Vec<u64> = self.samples.clone();
        sorted.sort_unstable();
        let k = ((sorted.len() - 1) as f64 * p).floor() as usize;
        sorted[k] as f64
    }
}

/// MLPerf-style trimmed mean: drop the fastest and the slowest sample, average
/// what is left. Fewer than three samples leaves nothing to trim, so it
/// degrades to the plain mean (and one sample to itself). Empty is 0.0.
///
/// The value is not robustness for its own sake — it is that the policy is
/// fixed BEFORE the run. An ad-hoc run count invites choosing N after seeing
/// the numbers, which is how a benchmark ends up reporting its luckiest
/// repetition. MLPerf fixes N per benchmark and states the tolerance N was
/// chosen to hold; `--reps` (default 5) is that N here.
pub fn trimmed_mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    if values.len() < 3 {
        return values.iter().sum::<f64>() / values.len() as f64;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let kept = &sorted[1..sorted.len() - 1];
    kept.iter().sum::<f64>() / kept.len() as f64
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_known_samples() {
        let mut h = Histogram::new();
        for &v in &[10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            h.record(v);
        }
        assert_eq!(h.percentile(0.5), 50.0); // median
        assert!(h.percentile(0.99) >= 90.0); // near max
        assert_eq!(h.percentile(0.0), 10.0); // min
    }

    #[test]
    fn empty_returns_zero() {
        let h = Histogram::new();
        assert_eq!(h.percentile(0.99), 0.0);
    }

    #[test]
    fn trimmed_mean_drops_the_extremes() {
        // The outlier is 20x the others: if it survived the trim it would move
        // the mean by more than an order of magnitude.
        assert_eq!(trimmed_mean(&[100.0, 500.0, 500.0, 500.0, 10_000.0]), 500.0);
    }

    #[test]
    fn trimmed_mean_degrades_gracefully_below_three_samples() {
        assert_eq!(trimmed_mean(&[]), 0.0);
        assert_eq!(trimmed_mean(&[42.0]), 42.0);
        assert_eq!(trimmed_mean(&[10.0, 20.0]), 15.0);
    }

    #[test]
    fn merge_combines() {
        let mut a = Histogram::new();
        a.record(1);
        let mut b = Histogram::new();
        b.record(2);
        a.merge(&b);
        assert_eq!(a.percentile(1.0), 2.0);
        assert_eq!(a.percentile(0.0), 1.0);
    }
}
