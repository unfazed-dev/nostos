//! `/sync` authentication adapters — implement the [`SyncAuth`] port.
//!
//! The transport resolves a connection's bearer token to a [`Principal`] before
//! upgrading the WebSocket. Two implementations:
//!
//! - [`AllowAnonymous`] — OSS self-host dev default (`NOSTOS_SYNC_AUTH=none`).
//!   Every connection becomes [`Principal::anonymous`]; no tenant filter is
//!   injected. Never use in a multi-tenant managed deploy.
//! - [`SupabaseJwtAuth`] — HS256-verifies a Supabase JWT and lifts `sub` as the
//!   account id and tenant id. Mirrors `nostos_cloud::auth::Hs256Verifier`'s
//!   algorithm exactly (HMAC-SHA256 over `header.payload`), kept here rather
//!   than shared so `nostos-server` doesn't depend on the control-plane crate
//!   (ADR-0010).
//!
//! The tenant id in Phase 0 defaults to the `sub` itself (one tenant per
//! account) — sufficient to prove the anti-self-attestation path. Phase 2
//! resolves a real tenant claim / RLS join (ADR-0011).

use async_trait::async_trait;
use nostos_application::ports::SyncAuth;
use nostos_domain::Principal;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

/// Authenticate nobody — every token becomes the anonymous principal.
///
/// This is the `NOSTOS_SYNC_AUTH=none` default for OSS self-host development.
/// The transport still requires it be set explicitly so an unauthenticated
/// deploy is a deliberate choice, not an oversight.
#[derive(Debug, Clone, Default)]
pub struct AllowAnonymous;

impl AllowAnonymous {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SyncAuth for AllowAnonymous {
    async fn authenticate(&self, _token: &str) -> Option<Principal> {
        Some(Principal::anonymous())
    }
}

/// Authenticate a Supabase-issued HS256 JWT by verifying its signature and
/// lifting the `sub` claim. No `exp`/`aud`/`iss` check in Phase 0 — Supabase's
/// GoTrue mints short-lived tokens and the gateway handles expiry; we verify
/// the signature so a forged token can't read another tenant's rows.
pub struct SupabaseJwtAuth {
    secret: Vec<u8>,
}

impl SupabaseJwtAuth {
    /// Construct from the raw JWT secret (`SUPABASE_JWT_SECRET`). Stored as
    /// bytes; never logged.
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self { secret }
    }
}

#[async_trait]
impl SyncAuth for SupabaseJwtAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        verify_supabase_hs256(token, &self.secret)
    }
}

/// Verify an HS256 JWT and return a principal whose account/tenant id is the
/// `sub` claim. Returns `None` on any malformation or signature mismatch.
///
/// Kept as a free function so a test can exercise the crypto without
/// constructing the async adapter.
fn verify_supabase_hs256(token: &str, secret: &[u8]) -> Option<Principal> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let sig = parts.next()?;
    if parts.next().is_some() {
        return None; // a JWT has exactly three segments
    }
    let signing_input = format!("{header}.{payload}");
    let sig_bytes = decode_base64url_to_bytes(sig)?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(signing_input.as_bytes());
    if mac.verify_slice(&sig_bytes).is_err() {
        warn!("jwt rejected: bad signature");
        return None;
    }
    let payload_bytes = decode_base64url_to_bytes(payload)?;
    let claims: SupabaseClaims = serde_json::from_slice(&payload_bytes).ok()?;
    let sub = claims.sub;
    if sub.is_empty() {
        return None;
    }
    // Phase 0: tenant = account (one tenant per Supabase user). ADR-0011 defers
    // the real tenant-claim/RLS resolution.
    Some(Principal::new(sub.clone(), sub))
}

#[derive(serde::Deserialize)]
struct SupabaseClaims {
    sub: String,
}

/// Minimal base64url decode (no padding). Avoids a base64 dependency for the
/// handful of JWT segments we decode per connection.
fn decode_base64url_to_bytes(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v: u32 = match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a' + 26),
            b'0'..=b'9' => u32::from(c - b'0' + 52),
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from(buf >> bits).expect("byte masked to 8 bits"));
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_anonymous_mints_anonymous_principal() {
        let auth = AllowAnonymous::new();
        let p = auth.authenticate("anything").await.expect("always Some");
        assert!(p.is_anonymous());
    }

    #[tokio::test]
    async fn malformed_token_rejected() {
        let auth = SupabaseJwtAuth::new(b"secret".to_vec());
        assert!(auth.authenticate("not-a-jwt").await.is_none());
        assert!(auth.authenticate("a.b").await.is_none());
        assert!(auth.authenticate("").await.is_none());
    }

    #[tokio::test]
    async fn forged_signature_rejected() {
        // A well-formed three-segment token with a wrong signature.
        let auth = SupabaseJwtAuth::new(b"correct-secret".to_vec());
        // header.payload.sig — sig won't verify against "correct-secret".
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1MSJ9.ZmFrZXNpZw";
        assert!(auth.authenticate(token).await.is_none());
    }

    #[test]
    fn base64url_decodes_roundtrip() {
        // "hi" in base64url = "aGk" (no padding).
        assert_eq!(decode_base64url_to_bytes("aGk"), Some(b"hi".to_vec()));
        // Empty.
        assert_eq!(decode_base64url_to_bytes(""), Some(Vec::new()));
        // Invalid char.
        assert!(decode_base64url_to_bytes("!!!!").is_none());
    }
}
