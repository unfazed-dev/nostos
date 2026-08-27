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
//! Per-tenant policy (B2 of the arxa integration plan — the former v1.1
//! ponytail, closed 2026-08-27): store-backed API keys carry optional
//! per-tenant (rate_per_sec, burst) overrides resolved at bucket creation;
//! tenants without an override (and env-only keys) use the daemon-wide
//! NOSTOS_PUSHD_SEND_RATE_PER_SEC / NOSTOS_PUSHD_SEND_BURST defaults. 429s
//! now carry `Retry-After`, derived from the bucket deficit — the ponytail
//! second half.

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

/// One tenant's resolved policy: defaults or the key's override.
#[derive(Debug, Clone, Copy)]
struct Policy {
    rate_per_sec: f64,
    burst: f64,
}

/// The per-tenant limiter shared by every /v1/send handler clone.
#[derive(Debug)]
pub struct SendRateLimiter {
    default_policy: Policy,
    overrides: HashMap<String, Policy>,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

/// What a failed acquire tells the caller (the 429's Retry-After).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejected {
    /// Whole seconds until `n` tokens are available (at least 1 — HTTP
    /// Retry-After is coarse; a sub-second deficit still asks the caller
    /// to wait one tick).
    pub retry_after_secs: u64,
}

impl SendRateLimiter {
    /// Build with the configured knobs. A `rate_per_sec` of 0 still allows
    /// the initial burst then never refills; a burst of 0 rejects
    /// everything (an operator's explicit off-switch).
    #[must_use]
    pub fn new(rate_per_sec: u32, burst: u32) -> Self {
        Self::with_overrides(rate_per_sec, burst, HashMap::new())
    }

    /// Build with per-tenant overrides (store-backed keys, B2). A tenant
    /// missing from the map uses the defaults.
    #[must_use]
    pub fn with_overrides(
        rate_per_sec: u32,
        burst: u32,
        overrides: HashMap<String, (u32, u32)>,
    ) -> Self {
        Self {
            default_policy: Policy {
                rate_per_sec: f64::from(rate_per_sec),
                burst: f64::from(burst),
            },
            overrides: overrides
                .into_iter()
                .map(|(t, (r, b))| {
                    (
                        t,
                        Policy {
                            rate_per_sec: f64::from(r),
                            burst: f64::from(b),
                        },
                    )
                })
                .collect(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The tenant's resolved policy (override or default).
    fn policy(&self, tenant: &str) -> Policy {
        self.overrides
            .get(tenant)
            .copied()
            .unwrap_or(self.default_policy)
    }

    /// The tenant's effective burst (the batch cap uses the CALLER's
    /// budget, not the daemon default).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn burst_for(&self, tenant: &str) -> u32 {
        self.policy(tenant).burst as u32
    }

    /// The daemon-default burst in whole tokens — retained for the boot
    /// log and tests; the batch cap uses `burst_for(tenant)` (the CALLER's
    /// budget, per-key overrides included).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn default_burst(&self) -> u32 {
        self.default_policy.burst as u32
    }

    /// Consume `n` tokens for `tenant` atomically — ALL or NOTHING: a
    /// short bucket keeps every token, so a batch caller's 429 means ZERO
    /// items were admitted (plan v1.1 batch-send pin, contract 0.4.0).
    /// `Err` carries the Retry-After (deficit / refill rate, >= 1s).
    pub fn try_acquire_n(&self, tenant: &str, n: u32) -> Result<(), Rejected> {
        if n == 0 {
            return Ok(());
        }
        let policy = self.policy(tenant);
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        let bucket = buckets
            .entry(tenant.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: policy.burst,
                last: now,
            });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * policy.rate_per_sec).min(policy.burst);
        bucket.last = now;
        if bucket.tokens >= f64::from(n) {
            bucket.tokens -= f64::from(n);
            Ok(())
        } else {
            Err(Self::retry_after(policy, bucket.tokens, n))
        }
    }

    /// Retry-After for a rejected acquire: whole seconds until `n` tokens
    /// exist at the refill rate, floored at 1 (HTTP Retry-After is
    /// coarse). A zero refill rate (the off-switch) never recovers —
    /// answer 1s so callers do not back off forever on a config the
    /// operator can flip back.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn retry_after(policy: Policy, tokens: f64, n: u32) -> Rejected {
        if policy.rate_per_sec <= 0.0 {
            return Rejected {
                retry_after_secs: 1,
            };
        }
        let deficit = f64::from(n) - tokens;
        // >= 1.0 by the max, finite by construction (rate > 0 checked).
        let secs = (deficit / policy.rate_per_sec).ceil().max(1.0);
        Rejected {
            retry_after_secs: secs as u64,
        }
    }

    /// Refund `n` tokens to `tenant`, capped at burst (a refund can never
    /// inflate the bucket past capacity). Batch send uses this when a
    /// phase-1 validation failure aborts the batch AFTER the all-or-nothing
    /// acquire: zero sends were attempted, so the reservation is returned
    /// (a single /v1/send charges 1 token for the same failure — a batch
    /// must not charge n). Phase-2 per-item admission failures are NOT
    /// refunded: an admission attempt costs its token, same as /v1/send.
    pub fn release_n(&self, tenant: &str, n: u32) {
        if n == 0 {
            return;
        }
        let policy = self.policy(tenant);
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        let bucket = buckets
            .entry(tenant.to_string())
            .or_insert_with(|| TokenBucket {
                tokens: policy.burst,
                last: now,
            });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * policy.rate_per_sec).min(policy.burst);
        bucket.last = now;
        bucket.tokens = (bucket.tokens + f64::from(n)).min(policy.burst);
    }

    /// Consume one token for `tenant`, refilling first. `Err` = the caller
    /// must answer 429 WITH the carried Retry-After.
    pub fn try_acquire(&self, tenant: &str) -> Result<(), Rejected> {
        self.try_acquire_n(tenant, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{SendRateLimiter, DEFAULT_BURST, DEFAULT_RATE_PER_SEC};
    use std::collections::HashMap;

    #[test]
    fn burst_exhausts_then_refills() {
        let limiter = SendRateLimiter::new(1, 2);
        assert!(limiter.try_acquire("a").is_ok());
        assert!(limiter.try_acquire("a").is_ok(), "burst of 2");
        assert!(limiter.try_acquire("a").is_err(), "exhausted -> 429");
        // Refill is elapsed-based; without sleeping we cannot observe a full
        // token, but the math is exercised by the clamp path below.
        let fresh = SendRateLimiter::new(1, 1);
        assert!(fresh.try_acquire("b").is_ok());
        assert!(fresh.try_acquire("b").is_err());
    }

    #[test]
    fn acquire_n_is_all_or_nothing() {
        let limiter = SendRateLimiter::new(1, 5);
        assert!(limiter.try_acquire_n("a", 3).is_ok(), "3 of 5 burst");
        assert!(
            limiter.try_acquire_n("a", 3).is_err(),
            "only 2 left — short bucket keeps them"
        );
        // The failed acquire_n did NOT drain: the 2 remaining tokens still buy 2 singles.
        assert!(limiter.try_acquire("a").is_ok());
        assert!(limiter.try_acquire("a").is_ok());
        assert!(limiter.try_acquire("a").is_err());
        assert!(
            limiter.try_acquire_n("b", 5).is_ok(),
            "other tenant unaffected"
        );
    }

    #[test]
    fn release_n_refunds_and_clamps_at_burst() {
        let limiter = SendRateLimiter::new(1, 5);
        assert!(limiter.try_acquire_n("a", 5).is_ok(), "drain the bucket");
        assert!(limiter.try_acquire("a").is_err(), "empty");
        limiter.release_n("a", 5);
        assert!(limiter.try_acquire_n("a", 5).is_ok(), "refunded in full");
        // Clamp: refunding more than capacity cannot inflate the bucket.
        limiter.release_n("a", 99);
        assert!(
            limiter.try_acquire_n("a", 5).is_ok(),
            "clamped at burst, not 104"
        );
        assert!(
            limiter.try_acquire("a").is_err(),
            "no inflation beyond burst"
        );
    }

    #[test]
    fn tenants_are_isolated() {
        let limiter = SendRateLimiter::new(1, 1);
        assert!(limiter.try_acquire("a").is_ok());
        assert!(limiter.try_acquire("a").is_err());
        assert!(limiter.try_acquire("b").is_ok(), "other tenant unaffected");
    }

    #[test]
    fn defaults_match_the_documented_knobs() {
        assert_eq!(super::DEFAULT_RATE_PER_SEC, 10);
        assert_eq!(DEFAULT_BURST, 50);
        let _ = DEFAULT_RATE_PER_SEC;
    }

    // ------------------------------------------------ per-tenant overrides (B2)

    #[test]
    fn per_tenant_override_isolates_budgets() {
        let mut overrides = HashMap::new();
        overrides.insert("vip".to_string(), (1, 10));
        let limiter = SendRateLimiter::with_overrides(1, 1, overrides);
        // Default tenant: burst 1.
        assert!(limiter.try_acquire("pleb").is_ok());
        assert!(limiter.try_acquire("pleb").is_err());
        // Overridden tenant: burst 10.
        for _ in 0..10 {
            assert!(limiter.try_acquire("vip").is_ok(), "override burst 10");
        }
        assert!(limiter.try_acquire("vip").is_err());
        assert_eq!(limiter.burst_for("vip"), 10);
        assert_eq!(limiter.burst_for("pleb"), 1);
        assert_eq!(limiter.default_burst(), 1);
    }

    #[test]
    fn retry_after_is_deficit_over_rate_floored_at_one() {
        let limiter = SendRateLimiter::new(2, 3);
        assert!(limiter.try_acquire_n("a", 3).is_ok(), "drain");
        let r = limiter.try_acquire_n("a", 5).expect_err("rejected");
        // Deficit 5 at rate 2/sec = 2.5s -> ceil 3s.
        assert_eq!(r.retry_after_secs, 3, "deficit/rate ceil");
        // A sub-second deficit still floors at 1s (HTTP coarseness).
        let tight = SendRateLimiter::new(100, 1);
        assert!(tight.try_acquire("a").is_ok());
        let r2 = tight.try_acquire("a").expect_err("rejected");
        assert_eq!(r2.retry_after_secs, 1, "floored at 1");
        // The off-switch (rate 0) answers 1s, not infinity.
        let off = SendRateLimiter::new(0, 1);
        assert!(off.try_acquire("a").is_ok());
        let r3 = off.try_acquire("a").expect_err("rejected");
        assert_eq!(r3.retry_after_secs, 1, "zero rate answers 1s");
    }
}
