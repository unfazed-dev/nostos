//! `/sync` authentication adapters — implement the [`SyncAuth`] port.
//!
//! The transport resolves a connection's bearer token to a [`Principal`] before
//! upgrading the WebSocket. Implementations:
//!
//! - [`AllowAnonymous`] — OSS self-host dev default (`NOSTOS_SYNC_AUTH=none`).
//!   Every connection becomes [`Principal::anonymous`]; no tenant filter is
//!   injected. Never use in a multi-tenant managed deploy.
//! - [`StaticBearerAuth`] — one shared bearer secret resolves to one fixed
//!   principal (`NOSTOS_SYNC_AUTH=bearer`). The single-tenant self-host middle
//!   rung: a real (non-anonymous) identity — so push-token registration
//!   works — without standing up an IdP (ADR-0010 addendum).
//! - [`SupabaseJwtAuth`] — verifies a Supabase JWT and lifts `sub` as the
//!   account id and tenant id. Routes on the token's header `alg`:
//!   - `HS256` verifies against a configured shared secret. Mirrors
//!     `nostos_cloud::auth::Hs256Verifier`'s algorithm exactly (HMAC-SHA256
//!     over `header.payload`), kept here rather than shared so `nostos-server`
//!     doesn't depend on the control-plane crate (ADR-0010).
//!   - `RS256`/`ES256`/`EdDSA` verify against a fetched-and-cached JWKS (see
//!     [`crate::jwks`]) — Supabase's default for projects created since
//!     2025-10-01 (ADR-0010 addendum). Never confused with the HS256 path: an
//!     HS256 token is never checked against JWKS key material, and vice
//!     versa — the header `alg` selects the verifier deterministically and
//!     `alg: none` is rejected outright (`jsonwebtoken`'s `Algorithm` has no
//!     "none" variant, so header parsing itself fails).
//!
//! The tenant id in Phase 0 defaults to the `sub` itself (one tenant per
//! account) — sufficient to prove the anti-self-attestation path. Phase 2
//! resolves a real tenant claim / RLS join (ADR-0011). This is identical
//! across the HS256 and JWKS paths.

use async_trait::async_trait;
use nostos_application::ports::SyncAuth;
use nostos_domain::Principal;
use hmac::{Hmac, Mac};
use jsonwebtoken::Algorithm;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::warn;

use crate::jwks::JwksVerifier;

type HmacSha256 = Hmac<Sha256>;

/// Default JWKS cache TTL — matches Supabase's ~10-minute edge cache on
/// `/.well-known/jwks.json` (ADR-0010 addendum). A deploy that wants a
/// shorter window can be given a config knob later; no one has asked yet.
pub const DEFAULT_JWKS_TTL: Duration = Duration::from_mins(10);

/// D1 security review (ADR-0031): max number of flat claims lifted from a
/// verified JWT into `Principal::extra`. Beyond this the token is REJECTED,
/// not truncated — a silently truncated claim set could drop the one claim a
/// rules scope depends on and change the meaning of a rule.
const MAX_EXTRA_CLAIMS: usize = 64;
/// D1: max byte length of any single lifted claim name or value. Same
/// rejection rule and same reason as `MAX_EXTRA_CLAIMS`.
const MAX_CLAIM_LEN: usize = 1024;
/// D1: claim names that may never enter `Principal::extra`, because
/// `Principal::claim` resolves them from the typed `account_id`/`tenant_id`
/// fields and a duplicate in `extra` would be ambiguous — worse, a
/// legitimately-issued token could smuggle a `tenant_id` claim that never
/// reaches `extra` (it's filtered here) but must also never overwrite the
/// server-derived `Principal::tenant_id`.
const RESERVED_CLAIMS: [&str; 4] = ["sub", "tenant_id", "exp", "iat"];

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

/// Authenticate one shared bearer secret into one fixed, non-anonymous
/// principal — `NOSTOS_SYNC_AUTH=bearer` (ADR-0010 addendum).
///
/// The middle rung between [`AllowAnonymous`] and [`SupabaseJwtAuth`]: a
/// single-tenant self-host deploy (a personal device pair, a lab rig) that
/// needs an AUTHENTICATED principal — the push-token registry refuses the
/// anonymous one (ADR-0037 §3), so `none` can never deliver a push receipt —
/// without operating an identity provider. Every connection presenting
/// exactly the configured secret becomes the same `Principal` (account
/// `local`, tenant `local` unless overridden).
///
/// Honest limits: this is ONE shared credential for the whole deployment —
/// no per-device identity, no per-device revocation, and rotating it means
/// re-provisioning every client. A managed multi-tenant deploy belongs to
/// `supabase-jwt`.
///
/// Timing: the presented token and the configured secret are both run
/// through SHA-256 and only the fixed-length 32-byte digests are compared,
/// so comparison timing carries no information about either secret's bytes
/// (the digest-vs-digest compare is the same shape for every input).
pub struct StaticBearerAuth {
    digest: [u8; 32],
    account_id: String,
    tenant_id: String,
}

impl StaticBearerAuth {
    /// The default identity: account `local`, tenant `local`.
    #[must_use]
    pub fn new(token: impl AsRef<[u8]>) -> Self {
        Self::with_identity(token, "local", "local")
    }

    /// An explicit fixed identity (account/tenant the registry rows and
    /// tenant-scoped predicates will carry).
    #[must_use]
    pub fn with_identity(token: impl AsRef<[u8]>, account_id: &str, tenant_id: &str) -> Self {
        Self {
            digest: Sha256::digest(token.as_ref()).into(),
            account_id: account_id.to_string(),
            tenant_id: tenant_id.to_string(),
        }
    }
}

#[async_trait]
impl SyncAuth for StaticBearerAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        if Sha256::digest(token.as_bytes()).as_slice() != self.digest.as_slice() {
            return None;
        }
        Some(Principal::new(
            self.account_id.clone(),
            self.tenant_id.clone(),
        ))
    }
}

/// Authenticate a Supabase-issued JWT, either HS256 (legacy shared secret) or
/// RS256/ES256/EdDSA (JWKS — Supabase's default since 2025-10-01). At least
/// one of `hs256_secret` / `jwks` must be configured (main.rs fails fast
/// otherwise); both may be set at once, in which case each token's header
/// `alg` picks the verifier.
///
/// HS256 verification: signature check + non-empty `sub` only. No
/// `exp`/`aud`/`iss` check in Phase 0 — Supabase's GoTrue mints short-lived
/// tokens and the gateway handles expiry; we verify the signature so a forged
/// token can't read another tenant's rows. **Unchanged from before this JWKS
/// addition** — existing HS256 behavior and tests are untouched.
///
/// JWKS verification additionally validates `exp` (via `jsonwebtoken`'s
/// default `Validation`, which requires and checks it) — new relative to the
/// HS256 path; `aud`/`role` are not checked either way, matching HS256's
/// laxity, since `Principal` only ever lifts `sub`.
pub struct SupabaseJwtAuth {
    hs256_secret: Option<Vec<u8>>,
    jwks: Option<JwksVerifier>,
}

impl SupabaseJwtAuth {
    /// Legacy path: HS256 shared-secret verification only. Kept as the
    /// original constructor so existing callers/tests are untouched.
    #[must_use]
    pub fn new(secret: Vec<u8>) -> Self {
        Self {
            hs256_secret: Some(secret),
            jwks: None,
        }
    }

    /// Construct from the resolved config surface: an optional legacy HS256
    /// secret and/or an optional JWKS URL. At least one must be `Some` — the
    /// caller (`nostos-server`'s config wiring) enforces that and fails fast
    /// otherwise, matching the existing missing-secret fail-fast style.
    #[must_use]
    pub fn from_config(hs256_secret: Option<Vec<u8>>, jwks_url: Option<String>) -> Self {
        Self {
            hs256_secret,
            jwks: jwks_url.map(|url| JwksVerifier::new(url, DEFAULT_JWKS_TTL)),
        }
    }
}

#[async_trait]
impl SyncAuth for SupabaseJwtAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        // Peek the header only (alg + kid) — no signature check yet. An
        // unparseable header (including `alg: "none"`, which has no
        // `Algorithm` variant) is rejected here.
        let header = jsonwebtoken::decode_header(token).ok()?;
        match header.alg {
            Algorithm::HS256 => {
                let secret = self.hs256_secret.as_ref()?;
                verify_supabase_hs256(token, secret)
            }
            Algorithm::RS256 | Algorithm::ES256 | Algorithm::EdDSA => {
                let jwks = self.jwks.as_ref()?;
                jwks.verify(token, &header).await
            }
            other => {
                warn!(?other, "jwt rejected: unsupported/disallowed algorithm");
                None
            }
        }
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
    // ADR-0029 §Decision-4: enforce `exp` when present (the JWKS/RS256 path
    // already does via jsonwebtoken's `Validation`). A token with no `exp`
    // never expires (JWT convention) — this preserves the Phase-0 behavior the
    // existing tests rely on (their tokens carry no `exp`).
    if let Some(exp) = claims.exp {
        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            Err(_) => i64::MAX,
        };
        if now > exp + JWT_LEEWAY_SECS {
            warn!("jwt rejected: expired");
            return None;
        }
    }
    let sub = claims.sub;
    if sub.is_empty() {
        return None;
    }
    // ADR-0031, D1: filter+cap the remaining claims before they ever reach a
    // Principal. `?` here rejects the whole token (never a downgrade to
    // anonymous) if the claim set or a claim exceeds its size cap.
    let extra = lift_extra_claims(&claims.rest)?;
    // Phase 0: tenant = account (one tenant per Supabase user). ADR-0011 defers
    // the real tenant-claim/RLS resolution. `extra` never contains
    // `tenant_id` (RESERVED_CLAIMS), so a payload-carried tenant_id claim
    // cannot shadow this derived value.
    Some(Principal::with_claims(sub.clone(), sub, extra))
}

/// Clock-skew leeway for JWT `exp` enforcement (ADR-0029 §Decision-4). Mirrors
/// `jsonwebtoken`'s default 60s leeway so a token at the boundary isn't rejected
/// for trivial clock skew. Reused by the transport's live-socket close-on-exp
/// deadline so a connection is alive for exactly `[handshake, exp + leeway]`.
pub const JWT_LEEWAY_SECS: i64 = 60;

/// Best-effort decode of a JWT's `exp` claim (seconds since the UNIX epoch), or
/// `None` if the token is malformed or carries no `exp`. Used by the transport
/// to arm a live-socket close-on-expiry deadline (ADR-0029 §Decision-4) WITHOUT
/// threading `exp` through the domain `Principal` — auth lifecycle stays out of
/// the domain layer. This never accepts or rejects a token; signature
/// verification remains the [`SyncAuth`] adapter's job.
#[must_use]
pub fn token_exp(token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let payload_bytes = decode_base64url_to_bytes(payload)?;
    let claims: SupabaseClaims = serde_json::from_slice(&payload_bytes).ok()?;
    claims.exp
}

#[derive(serde::Deserialize)]
struct SupabaseClaims {
    sub: String,
    /// Expiry, seconds since the UNIX epoch. Optional: a token with no `exp`
    /// never expires (JWT convention). ADR-0029 §Decision-4.
    #[serde(default)]
    exp: Option<i64>,
    /// Every other claim in the payload (ADR-0031, D1) — `sub`/`exp` are
    /// matched by the named fields above and never appear here. Filtered to
    /// flat scalars and size-capped by [`lift_extra_claims`] before it ever
    /// reaches a [`Principal`].
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Filter a JWT payload's non-`sub`/`exp` claims down to the flat,
/// string-valued map `Principal::extra` carries (ADR-0031, D1 + security
/// review). Returns `None` — reject the whole token — when the claim set or
/// any single claim exceeds the configured caps; the caller propagates that
/// as an authentication failure via `?`, never a downgrade to anonymous.
///
/// `pub(crate)`: reused as-is by `crate::jwks::JwksVerifier::verify` (the
/// RS256/ES256/EdDSA path) so the security review lives in exactly one place
/// rather than being duplicated per verifier.
///
/// - Reserved names (`RESERVED_CLAIMS`) are dropped silently: they are
///   resolved from `Principal`'s typed fields, not `extra`, so a payload
///   value under one of these names must never survive into the map (that is
///   exactly the tenant-escape shape the review calls out).
/// - Objects and arrays are dropped, never stringified — a stringified
///   object would let a rules scope like `col = claims.org` compare against
///   `{"id":"acme"}` and silently never match.
/// - `null` is dropped, not turned into `""` — an empty-string claim would
///   make `col = claims.x` match rows with an empty column, a real widening.
/// - Numbers and booleans stringify via `serde_json::Value`'s own rendering.
pub(crate) fn lift_extra_claims(
    rest: &serde_json::Map<String, serde_json::Value>,
) -> Option<BTreeMap<String, String>> {
    let mut extra = BTreeMap::new();
    for (name, value) in rest {
        if RESERVED_CLAIMS.contains(&name.as_str()) {
            continue;
        }
        let value = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null
            | serde_json::Value::Object(_)
            | serde_json::Value::Array(_) => {
                continue;
            }
        };
        if name.len() > MAX_CLAIM_LEN || value.len() > MAX_CLAIM_LEN {
            warn!(claim = %name, "jwt rejected: claim name or value exceeds size cap");
            return None;
        }
        extra.insert(name.clone(), value);
    }
    if extra.len() > MAX_EXTRA_CLAIMS {
        warn!(count = extra.len(), "jwt rejected: too many claims");
        return None;
    }
    Some(extra)
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
    async fn static_bearer_matches_configured_secret() {
        let auth = StaticBearerAuth::new("s3cret");
        let p = auth.authenticate("s3cret").await.expect("exact match");
        assert!(!p.is_anonymous());
        assert_eq!(p.account_id, "local");
        assert_eq!(p.tenant_id, "local");
    }

    #[tokio::test]
    async fn static_bearer_rejects_wrong_partial_and_empty() {
        let auth = StaticBearerAuth::new("s3cret");
        assert!(auth.authenticate("wrong").await.is_none());
        // Prefixes/suffixes must not match either — the digest compare is
        // over the whole presented token, not a substring search.
        assert!(auth.authenticate("s3cre").await.is_none());
        assert!(auth.authenticate("s3cret2").await.is_none());
        assert!(auth.authenticate("").await.is_none());
    }

    #[tokio::test]
    async fn static_bearer_identity_is_configurable() {
        let auth = StaticBearerAuth::with_identity("s3cret", "owner", "arxa");
        let p = auth.authenticate("s3cret").await.expect("match");
        assert_eq!(p.account_id, "owner");
        assert_eq!(p.tenant_id, "arxa");
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

    // ---- combined HS256+JWKS routing / algorithm-confusion tests ----
    //
    // These exercise `SupabaseJwtAuth::authenticate` end-to-end (not
    // `JwksVerifier` directly, which `jwks::tests` already covers) to prove
    // the header-`alg`-based routing between the two verifiers is correct and
    // that neither verifier ever touches the other's key material.

    use crate::jwks::test_support::{b64url, mint_token, rsa_key_and_jwk, FixtureJwks};
    use jsonwebtoken::jwk::JwkSet;

    fn valid_hs256_token(secret: &[u8], sub: &str) -> String {
        let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64url(format!(r#"{{"sub":"{sub}"}}"#).as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
        mac.update(signing_input.as_bytes());
        let sig = b64url(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    /// Like [`valid_hs256_token`] but carries an `exp` claim (ADR-0029 §Decision-4).
    fn hs256_token_with_exp(secret: &[u8], sub: &str, exp: i64) -> String {
        let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64url(format!(r#"{{"sub":"{sub}","exp":{exp}}}"#).as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
        mac.update(signing_input.as_bytes());
        let sig = b64url(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    /// Sign an arbitrary JSON payload as an HS256 token (used by the D1
    /// extra-claims tests below, which need payload shapes
    /// `valid_hs256_token`/`hs256_token_with_exp` can't express).
    fn hs256_token_from_payload(secret: &[u8], payload_json: &str) -> String {
        let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64url(payload_json.as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
        mac.update(signing_input.as_bytes());
        let sig = b64url(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    // ---- D1: extra-claims lifting + security review (ADR-0031) ----

    #[tokio::test]
    async fn jwt_lifts_flat_string_claims() {
        let secret = b"secret";
        let payload =
            serde_json::json!({"sub": "u1", "org_id": "acme", "role": "admin", "n": 7}).to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        let p = auth.authenticate(&token).await.expect("verifies");
        assert_eq!(p.extra.len(), 3);
        assert_eq!(p.extra.get("org_id").map(String::as_str), Some("acme"));
        assert_eq!(p.extra.get("role").map(String::as_str), Some("admin"));
        assert_eq!(p.extra.get("n").map(String::as_str), Some("7"));
    }

    #[tokio::test]
    async fn nested_and_array_claims_are_dropped() {
        let secret = b"secret";
        let payload = serde_json::json!({
            "sub": "u1",
            "org": {"id": "acme"},
            "roles": ["a", "b"],
            "ok": "yes",
        })
        .to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        let p = auth.authenticate(&token).await.expect("verifies");
        assert_eq!(
            p.extra.len(),
            1,
            "objects and arrays must be dropped, not stringified: {:?}",
            p.extra
        );
        assert_eq!(p.extra.get("ok").map(String::as_str), Some("yes"));
    }

    #[tokio::test]
    async fn null_claims_are_dropped() {
        let secret = b"secret";
        let payload = serde_json::json!({"sub": "u1", "x": null}).to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        let p = auth.authenticate(&token).await.expect("verifies");
        assert!(
            !p.extra.contains_key("x"),
            "a null claim must be dropped, not stringified to an empty string"
        );
    }

    #[tokio::test]
    async fn oversized_claim_set_is_rejected() {
        let secret = b"secret";
        let mut map = serde_json::Map::new();
        map.insert("sub".to_string(), serde_json::json!("u1"));
        for i in 0..65 {
            map.insert(format!("c{i}"), serde_json::json!("v"));
        }
        let payload = serde_json::Value::Object(map).to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        assert!(
            auth.authenticate(&token).await.is_none(),
            "a 65-claim token must be rejected outright, not truncated or downgraded"
        );
    }

    #[tokio::test]
    async fn oversized_claim_value_is_rejected() {
        let secret = b"secret";
        let big_value = "x".repeat(MAX_CLAIM_LEN + 1);
        let payload = serde_json::json!({"sub": "u1", "big": big_value}).to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        assert!(
            auth.authenticate(&token).await.is_none(),
            "a claim value over {MAX_CLAIM_LEN} bytes must be rejected outright"
        );
    }

    #[tokio::test]
    async fn reserved_claim_names_cannot_shadow() {
        let secret = b"secret";
        // Phase 0 derives tenant_id from sub ("u1"); the payload tries to
        // smuggle a different tenant_id through the flattened claims map.
        let payload = serde_json::json!({"sub": "u1", "tenant_id": "evil"}).to_string();
        let token = hs256_token_from_payload(secret, &payload);
        let auth = SupabaseJwtAuth::new(secret.to_vec());
        let p = auth.authenticate(&token).await.expect("verifies");
        assert_eq!(
            p.tenant_id, "u1",
            "a payload-carried tenant_id claim must never overwrite the derived tenant_id"
        );
        assert_eq!(
            p.claim("tenant_id"),
            Some("u1"),
            "claim(\"tenant_id\") must resolve the derived value, not the payload's"
        );
    }

    #[tokio::test]
    async fn hs256_expired_token_rejected() {
        // ADR-0029 §Decision-4: the HS256 path now enforces `exp`, matching the
        // JWKS/RS256 path. A token whose `exp` is in the past is rejected.
        let auth = SupabaseJwtAuth::new(b"secret".to_vec());
        let expired = hs256_token_with_exp(b"secret", "user-1", 1); // 1970-01-01
        assert!(
            auth.authenticate(&expired).await.is_none(),
            "an expired HS256 token must be rejected at auth"
        );
    }

    #[tokio::test]
    async fn hs256_future_exp_token_accepted() {
        // A non-expired `exp` is accepted (and the 60s leeway does not reject a
        // comfortably-future token).
        let auth = SupabaseJwtAuth::new(b"secret".to_vec());
        let future = hs256_token_with_exp(b"secret", "user-1", 9_999_999_999); // year ~2286
        assert!(
            auth.authenticate(&future).await.is_some(),
            "a future-exp HS256 token must authenticate"
        );
    }

    #[tokio::test]
    async fn alg_none_rejected_outright() {
        // A token whose header claims `alg: none` — jsonwebtoken's `Algorithm`
        // has no "none" variant, so header parsing itself must fail before
        // either verifier is even selected.
        let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64url(br#"{"sub":"user-1"}"#);
        let token = format!("{header}.{payload}."); // empty signature, as alg=none tokens carry
        let auth = SupabaseJwtAuth::new(b"secret".to_vec());
        assert!(auth.authenticate(&token).await.is_none());
    }

    #[tokio::test]
    async fn hs256_token_rejected_when_only_jwks_configured() {
        // JWKS-only config (no legacy secret): an HS256 token must be
        // rejected outright, never checked against any key material.
        let (_enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let auth = SupabaseJwtAuth::from_config(None, Some(fixture.url()));
        let token = valid_hs256_token(b"whatever-secret", "user-1");
        assert!(auth.authenticate(&token).await.is_none());
        assert_eq!(fixture.hit_count(), 0, "HS256 token never touches JWKS");
    }

    #[tokio::test]
    async fn rs256_token_rejected_when_only_hs256_configured() {
        // HS256-only config (no JWKS): an RS256 token must be rejected
        // outright, even though the signature itself is perfectly valid.
        let (enc, jwk) = rsa_key_and_jwk("k1");
        // Constructed but never registered with a JWKS endpoint the server
        // knows about — proves the rejection is "no jwks configured", not
        // "kid not found".
        let _ = &jwk;
        let auth = SupabaseJwtAuth::new(b"some-secret".to_vec());
        let token = mint_token(&enc, jsonwebtoken::Algorithm::RS256, "k1", "user-1");
        assert!(auth.authenticate(&token).await.is_none());
    }

    #[tokio::test]
    async fn combined_mode_routes_hs256_and_jwks_to_correct_principals() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let secret = b"legacy-secret".to_vec();
        let auth = SupabaseJwtAuth::from_config(Some(secret.clone()), Some(fixture.url()));

        let hs_token = valid_hs256_token(&secret, "hs-user");
        let hs_principal = auth.authenticate(&hs_token).await.expect("hs256 verifies");
        assert_eq!(hs_principal.account_id, "hs-user");

        let rs_token = mint_token(&enc, jsonwebtoken::Algorithm::RS256, "k1", "rs-user");
        let rs_principal = auth.authenticate(&rs_token).await.expect("rs256 verifies");
        assert_eq!(rs_principal.account_id, "rs-user");
    }

    #[tokio::test]
    async fn alg_confusion_rsa_public_material_as_hmac_secret_rejected() {
        // Classic RS256->HS256 downgrade attack: an attacker who only knows
        // the RSA *public* key tries to forge an HS256 token using the
        // public modulus bytes as if they were the HMAC secret, hoping a
        // naive verifier derives its "secret" from the public key. Our
        // HS256 path only ever checks against the explicitly configured
        // `NOSTOS_SUPABASE_JWT_SECRET` — never anything derived from a JWKS —
        // so this must fail regardless of what key material is public.
        let (_enc, jwk) = rsa_key_and_jwk("k1");
        let AlgorithmParametersForTest::RSA(params) = &jwk.algorithm else {
            unreachable!("rsa_key_and_jwk always returns an RSA key")
        };
        let forged_secret = params.n.clone().into_bytes(); // attacker's guess: n as HMAC key
        let real_secret = b"the-actual-configured-secret".to_vec();
        let auth = SupabaseJwtAuth::new(real_secret);

        let header = b64url(br#"{"alg":"HS256","typ":"JWT","kid":"k1"}"#);
        let payload = b64url(br#"{"sub":"attacker"}"#);
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(&forged_secret).expect("hmac key");
        mac.update(signing_input.as_bytes());
        let sig = b64url(&mac.finalize().into_bytes());
        let token = format!("{signing_input}.{sig}");

        assert!(auth.authenticate(&token).await.is_none());
    }

    // Local alias so the alg-confusion test above can pattern-match the JWK's
    // algorithm variant without importing jsonwebtoken's type under a name
    // that collides with anything in this module.
    use jsonwebtoken::jwk::AlgorithmParameters as AlgorithmParametersForTest;
}
