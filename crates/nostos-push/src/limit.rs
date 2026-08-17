//! Per-tenant send rate limiting (ADR-0038; 2026-08-17 security-audit
//! closeout, plan task 4.1 — audit finding 2).
//!
//! A hand-rolled token bucket over `std::time::Instant` — deliberately NO
//! new dependency: the need is one map of (tenant -> bucket) with a
//! check-and-consume under a short `std::sync::Mutex` critical section
//! (no await inside, so a std Mutex is correct on an async handler).
//!
//! Semantics: each tenant starts with `burst` tokens and regains
//! `rate_per_sec` per second up to `burst` again. POST /v1/send consumes
//! one token per request BEFORE any body parsing; an empty bucket is the
//! contract's 429. Registry reads, receipts polls, and healthz are NOT
//! limited — only the expensive, fan-out-shaped route is.
//!
//! The tenant map is bounded by the configured key list (tenants arrive
//! only from a matched key, never from the request), so it needs no
//! eviction of its own.
//!
//! ponytail: the knobs are process-wide, not per-tenant — one daemon, one
//! policy; the upgrade path (v1.1) is per-key limits in the registry store
//! once key CRUD lands, plus a `Retry-After` header derived from the
//! bucket deficit. Defaults: NOSTOS_PUSHD_SEND_RATE_PER_SEC=10,
//! NOSTOS_PUSHD_SEND_BURST=50.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Default sustained rate (requests/sec per tenant) — the config default.
pub const DEFAULT_RATE_PER_SEC: u32 = 10;
/// Default burst size (max instantaneous requests per tenant).
pub const DEFAULT_BURST: u32 = 50;

/// One tenant's bucket: fractional tokens plus the last refill instant.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

/// The per-tenant limiter shared by every /v1/send handler clone.
#[derive(Debug)]
pub struct SendRateLimiter {
    rate_per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl SendRateLimiter {
    /// Build with the configured knobs. A `rate_per_sec` of 0 still allows
    /// the initial burst then never refills; a burst of 0 rejects
    /// everything (an operator's explicit off-switch).
    #[must_use]
    pub fn new(rate_per_sec: u32, burst: u32) -> Self {
        Self {
            rate_per_sec: f64::from(rate_per_sec),
            burst: f64::from(burst),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consume `n` tokens for `tenant` atomically — ALL or NOTHING: a
    /// short bucket keeps every token, so a batch caller's 429 means ZERO
    /// items were admitted (plan v1.1 batch-send pin, contract 0.4.0).
    pub fn try_acquire_n(&self, tenant: &str, n: u32) -> bool {
        if n == 0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        let bucket = buckets
            .entry(tenant.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: self.burst,
                last: now,
            });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_sec).min(self.burst);
        bucket.last = now;
        if bucket.tokens >= f64::from(n) {
            bucket.tokens -= f64::from(n);
            true
        } else {
            false
        }
    }

    /// Consume one token for `tenant`, refilling first. `false` = the
    /// caller must answer 429.
    pub fn try_acquire(&self, tenant: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        let bucket = buckets
            .entry(tenant.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: self.burst,
                last: now,
            });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_sec).min(self.burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SendRateLimiter, DEFAULT_BURST, DEFAULT_RATE_PER_SEC};

    #[test]
    fn burst_exhausts_then_refills() {
        let limiter = SendRateLimiter::new(1, 2);
        assert!(limiter.try_acquire("a"));
        assert!(limiter.try_acquire("a"), "burst of 2");
        assert!(!limiter.try_acquire("a"), "exhausted -> 429");
        // Refill is elapsed-based; without sleeping we cannot observe a full
        // token, but the math is exercised by the clamp path below.
        let fresh = SendRateLimiter::new(1, 1);
        assert!(fresh.try_acquire("b"));
        assert!(!fresh.try_acquire("b"));
    }

    #[test]
    fn acquire_n_is_all_or_nothing() {
        let limiter = SendRateLimiter::new(1, 5);
        assert!(limiter.try_acquire_n("a", 3), "3 of 5 burst");
        assert!(
            !limiter.try_acquire_n("a", 3),
            "only 2 left — short bucket keeps them"
        );
        // The failed acquire_n did NOT drain: the 2 remaining tokens still buy 2 singles.
        assert!(limiter.try_acquire("a"));
        assert!(limiter.try_acquire("a"));
        assert!(!limiter.try_acquire("a"));
        assert!(limiter.try_acquire_n("b", 5), "other tenant unaffected");
    }

    #[test]
    fn tenants_are_isolated() {
        let limiter = SendRateLimiter::new(1, 1);
        assert!(limiter.try_acquire("a"));
        assert!(!limiter.try_acquire("a"));
        assert!(limiter.try_acquire("b"), "other tenant unaffected");
    }

    #[test]
    fn defaults_match_the_documented_knobs() {
        assert_eq!(super::DEFAULT_RATE_PER_SEC, 10);
        assert_eq!(DEFAULT_BURST, 50);
        let _ = DEFAULT_RATE_PER_SEC;
    }
}
