//! A minimal latency histogram for benchmark reporting.
//!
//! Records raw sample values (microseconds) and computes percentiles by sorting
//! on demand. Adequate for Week-1 reporting (up to ~10^7 samples per run); a
//! streaming HDR-histogram can replace it later if memory or sort cost bites.

use std::collections::BinaryHeap;

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

    /// Number of samples recorded.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether zero samples have been recorded.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Percentile in microseconds. `p` is in `[0.0, 1.0]`. Returns 0.0 if empty.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        // Use a min-heap-based selection to avoid a full sort on every call.
        // For Week-1 sample sizes a sort would also be fine; the heap keeps
        // the door open to larger runs.
        let p = p.clamp(0.0, 1.0);
        let k = ((self.samples.len() as f64) * p).ceil() as usize;
        let k = k.clamp(1, self.samples.len());
        let mut heap: BinaryHeap<std::cmp::Reverse<u64>> =
            BinaryHeap::with_capacity(self.samples.len());
        for &s in &self.samples {
            heap.push(std::cmp::Reverse(s));
        }
        let mut val = 0u64;
        for _ in 0..k {
            if let Some(std::cmp::Reverse(v)) = heap.pop() {
                val = v;
            }
        }
        val as f64
    }
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
    fn merge_combines() {
        let mut a = Histogram::new();
        a.record(1);
        let mut b = Histogram::new();
        b.record(2);
        a.merge(&b);
        assert_eq!(a.len(), 2);
    }
}
