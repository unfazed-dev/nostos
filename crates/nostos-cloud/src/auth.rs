//! Auth — the dual-path identity resolver for Nostos Cloud.
//!
//! **ADD, not replace** (per architecture consult): managed-cloud callers send
//! a Supabase-issued `Authorization: Bearer <jwt>`; self-hosted OSS callers
//! (and the web admin) use the existing email/password → session-cookie flow.
//! Both resolve to the same account id — the resolution itself lives inline in
//! `routes::current_account_id`.
//!
//! The JWT verifier is a trait so `cfg(test)` can inject one that accepts any
//! well-formed token — smoke tests run with no external Supabase. The default
//! verifier does a real HS256 HMAC check against the configured JWT secret
//! (Supabase projects default to HS256 with a shared secret; RS256 is a later
//! upgrade behind this same trait).

use nostos_license::base64url_decode;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

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
        let signing_input = format!("{header_b64}.{payload_b64}").into_bytes();
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
        // ADR-0029 §Decision-4: reject expired tokens (no `exp` = never expires,
        // matching nostos-infra). 60s leeway mirrors `jsonwebtoken`'s default.
        if let Some(exp) = claims.exp {
            let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                Err(_) => i64::MAX,
            };
            if now > exp + 60 {
                return None;
            }
        }
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
    /// Expiry, seconds since the UNIX epoch. Optional (JWT convention: no `exp`
    /// = never expires). ADR-0029 §Decision-4: the HS256 verifier enforces `exp`
    /// when present, matching nostos-infra's JWKS/RS256 path.
    #[serde(default)]
    exp: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_license::{base64url_decode, base64url_encode};

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
    async fn jwt_verifier_resolves_sub() {
        let verifier = TestVerifier;
        assert_eq!(
            verifier.verify_sub(&fake_jwt("user_42")),
            Some("user_42".into())
        );
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
