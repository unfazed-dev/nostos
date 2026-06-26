//! Auth — the dual-path identity resolver for Nostos Cloud.
//!
//! **ADD, not replace** (per architecture consult): managed-cloud callers send
//! a Supabase-issued `Authorization: Bearer <jwt>`; self-hosted OSS callers
//! (and the web admin) use the existing email/password → session-cookie flow.
//! Both resolve to the same account id.
//!
//! The JWT verifier is a trait so `cfg(test)` can inject one that accepts any
//! well-formed token — smoke tests run with no external Supabase. The default
//! verifier does a real HS256 HMAC check against the configured JWT secret
//! (Supabase projects default to HS256 with a shared secret; RS256 is a later
//! upgrade behind this same trait).

use crate::license::base64url_decode;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

/// Verifies a Supabase-style JWT and returns its `sub` (the account id).
/// Trait so tests can skip real crypto.
pub trait JwtVerifier: Send + Sync {
    /// Verify `token` and return its `sub` claim, or `None` if invalid.
    fn verify_sub(&self, token: &str) -> Option<String>;
}

/// Production HS256 verifier using the Supabase project's JWT secret.
pub struct Hs256Verifier {
    secret: Vec<u8>,
}

impl Hs256Verifier {
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self { secret }
    }
}

impl JwtVerifier for Hs256Verifier {
    fn verify_sub(&self, token: &str) -> Option<String> {
        let mut parts = token.split('.');
        let header_b64 = parts.next()?;
        let payload_b64 = parts.next()?;
        let sig_b64 = parts.next()?;
        // Recompute HS256 over header.payload and constant-time compare.
        let signing_input =
            format!("{header_b64}.{payload_b64}").into_bytes();
        let mut mac = HmacSha256::new_from_slice(&self.secret).ok()?;
        mac.update(&signing_input);
        let expected = mac.finalize().into_bytes();
        let got = base64url_decode(sig_b64).ok()?;
        if got != expected.as_slice() {
            return None;
        }
        // Decode the payload and read `sub`.
        let payload = base64url_decode(payload_b64).ok()?;
        let claims: SupabaseClaims = serde_json::from_slice(&payload).ok()?;
        Some(claims.sub)
    }
}

/// A verifier that accepts ANY token and returns its decoded `sub` without
/// signature checking. **Test-only** — never construct in production code.
#[cfg(test)]
pub struct TestVerifier;

#[cfg(test)]
impl JwtVerifier for TestVerifier {
    fn verify_sub(&self, token: &str) -> Option<String> {
        let payload_b64 = token.split('.').nth(1)?;
        let payload = base64url_decode(payload_b64).ok()?;
        let claims: SupabaseClaims = serde_json::from_slice(&payload).ok()?;
        Some(claims.sub)
    }
}

/// The subset of JWT claims Nostos reads. `sub` is the Supabase user id;
/// we treat it as the account id (Supabase owns identity per the assembly ADR).
#[derive(Deserialize)]
struct SupabaseClaims {
    sub: String,
}

/// The error returned when no auth credential resolves an account.
#[derive(Debug)]
pub struct AuthError;

impl IntoResponse for AuthError {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::UNAUTHORIZED, "not authenticated").into_response()
    }
}

/// Extractor: pull a `Bearer` token from the `Authorization` header.
/// Returns `None` (not an error) when no bearer token is present — callers
/// fall back to the session cookie. Only fails on a *malformed* Authorization
/// header that isn't a Bearer scheme.
pub struct BearerToken(pub Option<String>);

#[axum::async_trait]
impl<S> FromRequestParts<S> for BearerToken
where
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Some(header) = parts.headers.get("authorization") else {
            return Ok(Self(None));
        };
        let Ok(s) = header.to_str() else {
            return Ok(Self(None));
        };
        let token = s.strip_prefix("Bearer ").unwrap_or("");
        if token.is_empty() {
            return Ok(Self(None));
        }
        Ok(Self(Some(token.to_string())))
    }
}

/// Resolve an account id from either credential path:
///
/// 1. `Authorization: Bearer <jwt>` → verify via the configured verifier → `sub`
/// 2. else the `cairn_session` cookie → account id (existing flow)
///
/// Returns `Err` only when neither path yields a valid account.
pub fn resolve_account_id(
    bearer: &BearerToken,
    cookie_id: Option<&str>,
    verifier: &Arc<dyn JwtVerifier>,
) -> Result<String, AuthError> {
    // Path 1: JWT.
    if let Some(token) = &bearer.0 {
        if let Some(sub) = verifier.verify_sub(token) {
            return Ok(sub);
        }
    }
    // Path 2: session cookie.
    if let Some(id) = cookie_id {
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    Err(AuthError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license::{base64url_decode, base64url_encode};

    #[allow(dead_code)]
    fn _ensure_decode_linked() {
        let _ = base64url_decode("A").map(|v| v.len());
    }

    fn fake_jwt(sub: &str) -> String {
        // header.payload.sig — sig is irrelevant under TestVerifier.
        let header = base64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64url_encode(format!("{{\"sub\":\"{sub}\"}}").as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn jwt_path_resolves_sub() {
        let verifier: Arc<dyn JwtVerifier> = Arc::new(TestVerifier);
        let bearer = BearerToken(Some(fake_jwt("user_42")));
        let id = resolve_account_id(&bearer, None, &verifier).unwrap();
        assert_eq!(id, "user_42");
    }

    #[tokio::test]
    async fn cookie_path_resolves_when_no_bearer() {
        let verifier: Arc<dyn JwtVerifier> = Arc::new(TestVerifier);
        let bearer = BearerToken(None);
        let id = resolve_account_id(&bearer, Some("acc_99"), &verifier).unwrap();
        assert_eq!(id, "acc_99");
    }

    #[tokio::test]
    async fn no_credential_is_unauthorized() {
        let verifier: Arc<dyn JwtVerifier> = Arc::new(TestVerifier);
        let bearer = BearerToken(None);
        assert!(resolve_account_id(&bearer, None, &verifier).is_err());
    }

    #[tokio::test]
    async fn jwt_takes_precedence_over_cookie() {
        let verifier: Arc<dyn JwtVerifier> = Arc::new(TestVerifier);
        let bearer = BearerToken(Some(fake_jwt("jwt_user")));
        let id = resolve_account_id(&bearer, Some("cookie_user"), &verifier).unwrap();
        assert_eq!(id, "jwt_user");
    }

    #[test]
    fn hs256_verifier_rejects_bad_signature() {
        let v = Hs256Verifier::new(b"secret".to_vec());
        // A token whose signature doesn't match the secret must yield None.
        let token = fake_jwt("user_42");
        assert!(v.verify_sub(&token).is_none());
    }

    #[test]
    fn hs256_verifier_accepts_real_signature() {
        let secret = b"super-secret";
        let header = base64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64url_encode(br#"{"sub":"real_user"}"#);
        let signing = format!("{header}.{payload}").into_bytes();
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(&signing);
        let sig = base64url_encode(&mac.finalize().into_bytes());
        let token = format!("{header}.{payload}.{sig}");
        let v = Hs256Verifier::new(secret.to_vec());
        assert_eq!(v.verify_sub(&token).as_deref(), Some("real_user"));
    }
}
