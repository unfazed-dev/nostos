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
    std::env::var("NOSTOS_ADMIN_TOKEN")
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

/// Bearer-token gate. `check` is the only entry point: it never logs, never
/// echoes `expected` or the candidate, and returns a plain bool so the
/// caller decides the HTTP status (401) and the audit trail (Task 21 §3).
pub struct AdminAuth;

impl AdminAuth {
    pub fn check(headers: &axum::http::HeaderMap, expected: &SecretString) -> bool {
        match bearer_token(headers) {
            Some(candidate) => tokens_match(candidate, expected),
            None => false,
        }
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

    #[test]
    fn check_rejects_missing_and_malformed_headers() {
        let expected = SecretString("a".repeat(32));
        assert!(!AdminAuth::check(&axum::http::HeaderMap::new(), &expected));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic not-a-bearer-token".parse().unwrap(),
        );
        assert!(!AdminAuth::check(&headers, &expected));
    }

    #[test]
    fn check_accepts_correct_bearer_token() {
        let token = "a".repeat(32);
        let expected = SecretString(token.clone());
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(AdminAuth::check(&headers, &expected));
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
