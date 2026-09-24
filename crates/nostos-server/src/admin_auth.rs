//! Bearer-token gate for config-mutating routes (ADR-0031, D5 / Task 21).
//!
//! DELIBERATELY NOT the sync auth path: `NOSTOS_SYNC_AUTH=supabase-jwt`
//! authenticates *application users*, and no application user may ever
//! rewrite the server's rules. A Supabase JWT — however valid, whatever
//! claims it carries — is rejected here; this module knows nothing about
//! JWTs, JWKS, or `Principal` at all, which is the point.
//!
//! Fail-closed by construction: [`admin_token_from_env`] returns `None`
//! when `NOSTOS_ADMIN_TOKEN` is unset, and the caller (`main.rs`) treats
//! `None` as "the route is not mounted" — a 404, not a 401. A default
//! deployment that never opts in has no mutable surface to attack.

/// A `NOSTOS_ADMIN_TOKEN` value. Never `Display`, never plain `Debug` — the
/// only way to read the bytes is [`SecretString::expose`], so an accidental
/// `{token:?}` in a log line or error message prints `SecretString(REDACTED)`
/// instead of the token.
pub struct SecretString(String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(REDACTED)")
    }
}

/// Startup floor: a token shorter than this is guessable via the very route
/// it's meant to protect, so the server refuses to boot rather than serve
/// it (Task 21 step 4).
pub const MIN_ADMIN_TOKEN_LEN: usize = 32;

/// `None` when `NOSTOS_ADMIN_TOKEN` is unset (or empty) → the route is **not
/// mounted**. Read fresh from the environment on every call rather than
/// threaded through `SyncRouterState`: there is exactly one call site per
/// request (the handler) plus one at startup (the length check), so state
/// plumbing would be an abstraction with no second caller to justify it.
pub fn admin_token_from_env() -> Option<SecretString> {
    nostos_infra::env::var("NOSTOS_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .map(SecretString)
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Constant-time comparison. Compares SHA-256 digests of both sides rather
/// than the raw bytes: digests are always 32 bytes, so there is no
/// unequal-length branch to reason about (or panic on) before the
/// constant-time compare even starts — `sha2` is already a dependency of
/// this crate, so this doesn't reach for anything new.
fn tokens_match(candidate: &str, expected: &SecretString) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    let a = Sha256::digest(candidate.as_bytes());
    let b = Sha256::digest(expected.expose().as_bytes());
    a.ct_eq(&b).into()
}

/// Consecutive failed admin-auth attempts, process-wide. Reset by any success.
static ADMIN_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Each consecutive failure adds this much delay to the next one.
const FAILURE_DELAY_STEP: std::time::Duration = std::time::Duration::from_millis(100);

/// Ceiling on that delay. Bounded so a failing health probe or a
/// misconfigured operator script cannot pin a request task indefinitely.
const FAILURE_DELAY_CAP: std::time::Duration = std::time::Duration::from_secs(2);

/// Log loudly once the failures stop looking like a typo.
const FAILURE_WARN_AT: u64 = 5;

/// Throttle for the *n*-th consecutive failure. Pure so it can be tested
/// without sleeping.
fn failure_delay(consecutive_failures: u64) -> std::time::Duration {
    FAILURE_DELAY_STEP
        .saturating_mul(u32::try_from(consecutive_failures).unwrap_or(u32::MAX))
        .min(FAILURE_DELAY_CAP)
}

/// Bearer-token gate. `check` is the only entry point: it never logs the
/// candidate, never echoes `expected`, and returns a plain bool so the caller
/// decides the HTTP status (401) and the audit trail (Task 21 §3).
///
/// # Why the throttle attaches to the failure, not to the route
///
/// The obvious shape — "N bad attempts and the admin path is locked for M
/// minutes" — hands any unauthenticated caller a DoS on the operator: spam
/// wrong tokens and the real admin can no longer reach `PUT /rules`. That
/// trades a low-severity hardening gap for an availability bug.
///
/// So the token is compared *first* and a correct one returns immediately with
/// the counter reset — a legitimate operator is never delayed, even while an
/// attack is in progress. Only the failing branch pays, and the cost is a
/// bounded delay rather than a lockout.
///
/// Global rather than per-IP on purpose: `ConnectInfo` is not plumbed, and
/// behind a proxy every request shares one address anyway, which would make
/// per-IP keying more code and less truth. Honest scope: brute-forcing a
/// [`MIN_ADMIN_TOKEN_LEN`]-char token is infeasible with or without this. The
/// value is defence-in-depth against a *weak* long token, plus a log signal
/// that someone is trying.
pub struct AdminAuth;

impl AdminAuth {
    pub async fn check(headers: &axum::http::HeaderMap, expected: &SecretString) -> bool {
        let ok = match bearer_token(headers) {
            Some(candidate) => tokens_match(candidate, expected),
            None => false,
        };

        if ok {
            ADMIN_FAILURES.store(0, std::sync::atomic::Ordering::Relaxed);
            return true;
        }

        let failures = ADMIN_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if failures >= FAILURE_WARN_AT {
            tracing::warn!(
                consecutive_failures = failures,
                "admin auth: repeated failed PUT /rules attempts"
            );
        }
        tokio::time::sleep(failure_delay(failures)).await;
        false
    }
}

/// Audit `actor` id (Task 21 §3): first 8 hex chars of SHA-256(token) — a
/// non-secret fingerprint. Enough to tell two operators apart in the audit
/// log, useless to an attacker who only has the log.
pub fn actor_id(token: &SecretString) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(token.expose().as_bytes())
        .iter()
        .take(4)
        .fold(String::with_capacity(8), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_tokens_pass() {
        let expected = SecretString("a".repeat(32));
        assert!(tokens_match(&"a".repeat(32), &expected));
    }

    #[test]
    fn mismatched_tokens_fail() {
        let expected = SecretString("a".repeat(32));
        assert!(!tokens_match(&"b".repeat(32), &expected));
    }

    #[test]
    fn different_length_tokens_fail_without_panicking() {
        let expected = SecretString("a".repeat(32));
        assert!(!tokens_match("short", &expected));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let secret = SecretString("super-secret-token-value".to_string());
        let printed = format!("{secret:?}");
        assert_eq!(printed, "SecretString(REDACTED)");
        assert!(!printed.contains("super-secret-token-value"));
    }

    /// `ADMIN_FAILURES` is process-global, so a test that exercises the failing
    /// branch inherits whatever count its neighbours left behind. Reset first,
    /// or the throttle grows to its 2s cap and the suite crawls.
    fn reset_failures() {
        ADMIN_FAILURES.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[tokio::test]
    async fn check_rejects_missing_and_malformed_headers() {
        reset_failures();
        let expected = SecretString("a".repeat(32));
        assert!(!AdminAuth::check(&axum::http::HeaderMap::new(), &expected).await);

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic not-a-bearer-token".parse().unwrap(),
        );
        assert!(!AdminAuth::check(&headers, &expected).await);
    }

    #[tokio::test]
    async fn check_accepts_correct_bearer_token() {
        reset_failures();
        let token = "a".repeat(32);
        let expected = SecretString(token.clone());
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(AdminAuth::check(&headers, &expected).await);
    }

    #[test]
    fn failure_delay_escalates_then_stops_at_the_cap() {
        assert_eq!(failure_delay(0), std::time::Duration::ZERO);
        assert_eq!(failure_delay(1), FAILURE_DELAY_STEP);
        assert_eq!(failure_delay(3), FAILURE_DELAY_STEP * 3);

        // The cap is the point: without it a long-running attack would grow the
        // delay without bound and pin request tasks — turning the throttle into
        // the very DoS it exists to avoid.
        assert_eq!(failure_delay(1_000_000), FAILURE_DELAY_CAP);
        assert_eq!(failure_delay(u64::MAX), FAILURE_DELAY_CAP);
    }

    #[tokio::test]
    async fn a_correct_token_is_never_throttled_and_clears_the_counter() {
        // The property that makes this safe to ship: an attacker hammering the
        // route cannot lock the real operator out of it. Drive the failure
        // count up, then assert the correct token still passes.
        reset_failures();
        let token = "c".repeat(32);
        let expected = SecretString(token.clone());

        let mut wrong = axum::http::HeaderMap::new();
        wrong.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", "d".repeat(32)).parse().unwrap(),
        );
        for _ in 0..3 {
            assert!(!AdminAuth::check(&wrong, &expected).await);
        }
        assert!(ADMIN_FAILURES.load(std::sync::atomic::Ordering::Relaxed) >= 3);

        let mut right = axum::http::HeaderMap::new();
        right.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let started = std::time::Instant::now();
        assert!(AdminAuth::check(&right, &expected).await);
        assert!(
            started.elapsed() < FAILURE_DELAY_STEP,
            "a valid admin token must not pay the failure throttle"
        );
        assert_eq!(ADMIN_FAILURES.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn actor_id_is_stable_and_short() {
        let a = actor_id(&SecretString("a".repeat(32)));
        let b = actor_id(&SecretString("a".repeat(32)));
        let c = actor_id(&SecretString("b".repeat(32)));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }
}
