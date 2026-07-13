//! License tokens — the entitlement credential a Nostos Cloud subscription mints.
//!
//! **Scope (per STRATEGY.md §7.2):** licenses gate **Nostos Cloud** (managed
//! hosting) and **Enterprise** features, *never* the self-hosted Apache-2.0
//! binary. The OSS engine is free and unlimited forever; a license only ever
//! applies to managed instances and Enterprise services.
//!
//! ## Format
//!
//! A license is a compact, URL-safe string: `<payload-base64url>.<sig-hex>`,
//! where `payload` is a tiny JSON object (`tier`, `plan`, `device_cap`,
//! `expires_at`, `project_id`) and `sig` is `HMAC-SHA256(secret, payload)`.
//! This mirrors the Keygen offline-license model (signed dataset + signature,
//! verifiable without phoning home) — but trimmed to exactly what a launch needs.
//!
//! The managed `nostos-server` verifies a presented license by recomputing the
//! HMAC; if valid and unexpired, the instance runs at the licensed tier. No
//! network call, no telemetry — respect for the open-core trust model.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;

// Tier is defined in nostos-domain (the pure ring) so both the control plane
// (here) and the sync engine (nostos-server) can share one taxonomy + device-cap
// without either sibling depending on the other. Re-exported here so callers
// importing `Tier` from the license module keep resolving.
pub use nostos_domain::Tier;

/// The decoded payload of a license token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseClaims {
    pub project_id: String,
    pub tier: Tier,
    /// Unix timestamp (seconds) when the license expires.
    pub expires_at: i64,
    /// Optional device cap override (else tier default applies).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub device_cap: Option<u64>,
}

impl LicenseClaims {
    /// Mint a signed license string for these claims.
    ///
    /// # Errors
    /// Only fails if HMAC init fails (effectively never with a valid key).
    pub fn sign(&self, secret: &[u8]) -> Result<String, LicenseError> {
        let payload_json = serde_json::to_vec(self)?;
        let payload_b64 = base64url_encode(&payload_json);
        let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| LicenseError::BadSecret)?;
        mac.update(payload_b64.as_bytes());
        let sig = mac.finalize().into_bytes();
        Ok(format!("{payload_b64}.{}", hex::encode(sig)))
    }

    /// Verify + decode a license string against the secret. Returns the claims
    /// only if the signature is valid AND the license has not expired.
    ///
    /// # Errors
    /// [`LicenseError::BadSignature`] on tampering, [`LicenseError::Expired`]
    /// past `expires_at`.
    pub fn verify(token: &str, secret: &[u8]) -> Result<Self, LicenseError> {
        let (payload_b64, sig_hex) = token.split_once('.').ok_or(LicenseError::Malformed)?;
        let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| LicenseError::BadSecret)?;
        mac.update(payload_b64.as_bytes());
        mac.verify_slice(hex::decode(sig_hex)?.as_slice())
            .map_err(|_| LicenseError::BadSignature)?;
        let payload_json = base64url_decode(payload_b64)?;
        let claims: Self = serde_json::from_slice(&payload_json)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if claims.expires_at <= now {
            return Err(LicenseError::Expired);
        }
        Ok(claims)
    }
}

/// URL-safe base64 encode (no padding) — keeps license tokens short & URL-safe.
#[allow(clippy::cast_possible_truncation)]
pub fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHA[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = u32::from(bytes[i]) << 16;
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
    } else if rem == 2 {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
    }
    out
}

/// URL-safe base64 decode (no padding). `pub` so nostos-cloud's auth module can
/// reuse it for JWT decoding — no base64 dependency needed.
#[allow(clippy::cast_possible_truncation)] // ponytail: masked value is always 0..=255
pub fn base64url_decode(s: &str) -> Result<Vec<u8>, LicenseError> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in bytes {
        let v = u32::from(val(c).ok_or(LicenseError::Malformed)?);
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    #[error("malformed license token")]
    Malformed,
    #[error("invalid signing secret")]
    BadSecret,
    #[error("license signature mismatch (tampered or wrong secret)")]
    BadSignature,
    #[error("license has expired")]
    Expired,
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
}

/// The startup entitlement a managed `nostos-server` reaches from a license
/// token (or the OSS fallback). Produced by [`resolve_entitlement`]; the server
/// feeds `device_cap` into `SessionManager::with_device_cap` and uses `tier`
/// for tier-gated paths.
#[derive(Debug, Clone)]
pub struct ResolvedEntitlement {
    /// The authoritative tier (from the token when presented, else the fallback).
    pub tier: Tier,
    /// Effective concurrent-device cap: the token's explicit `device_cap` if
    /// set, else the tier default (`Tier::device_cap()`).
    pub device_cap: u64,
    /// The project the license was minted for (empty string when no token was
    /// presented — the OSS self-host path).
    pub project_id: String,
}

/// Resolve the startup entitlement from a signed license token.
///
/// - `license_token` — the `<payload>.<sig>` string from `NOSTOS_LICENSE`
///   (empty means no license presented).
/// - `secret` — the HMAC secret from `NOSTOS_LICENSE_SECRET`.
/// - `fallback_tier` — the tier `NOSTOS_TIER` resolves to (OSS self-host keeps
///   its free, unlimited `enterprise` default).
///
/// **Trust rule — the managed-server boundary:** an *absent* license falls back
/// to `fallback_tier`; a *presented-but-invalid* license (bad signature,
/// malformed, or expired) is a **hard error**. A managed deploy that presents a
/// license is asserting "enforce my entitlement," so an invalid token must
/// never silently downgrade to the unlimited OSS default. The caller treats the
/// `Err` as fatal — `nostos-server` refuses to start. This makes the open-core
/// trust model (ADR-0006) auditable: OSS stays free + unlimited, while a
/// managed instance cannot fake or ignore its licensed cap.
///
/// # Errors
/// [`LicenseError`] from [`LicenseClaims::verify`], but only when a token is
/// actually *presented*. An empty token never errors — it returns the fallback.
pub fn resolve_entitlement(
    license_token: &str,
    secret: &[u8],
    fallback_tier: Tier,
) -> Result<ResolvedEntitlement, LicenseError> {
    if license_token.is_empty() {
        return Ok(ResolvedEntitlement {
            tier: fallback_tier,
            device_cap: fallback_tier.device_cap(),
            project_id: String::new(),
        });
    }
    let claims = LicenseClaims::verify(license_token, secret)?;
    let device_cap = claims
        .device_cap
        .unwrap_or_else(|| claims.tier.device_cap());
    Ok(ResolvedEntitlement {
        tier: claims.tier,
        device_cap,
        project_id: claims.project_id,
    })
}

#[cfg(test)]
mod resolve_tests {
    //! Trust-boundary tests for the entitlement resolution the managed
    //! `nostos-server` performs at startup. These cover the NEW wiring (token
    //! → tier + effective cap) without spinning the binary —
    //! [`resolve_entitlement`] is the exact function nostos-server calls, so
    //! this is the server's consumption path under test.
    use super::*;

    const SECRET: &[u8] = b"test-license-secret";

    fn signed(tier: Tier, device_cap: Option<u64>, expires_in_secs: i64) -> String {
        LicenseClaims {
            project_id: "proj_abc".into(),
            tier,
            expires_at: OffsetDateTime::now_utc().unix_timestamp() + expires_in_secs,
            device_cap,
        }
        .sign(SECRET)
        .unwrap()
    }

    #[test]
    fn absent_license_falls_back_to_env_tier() {
        // OSS self-host: no token → fallback tier + its default cap.
        let ent = resolve_entitlement("", SECRET, Tier::Enterprise).unwrap();
        assert_eq!(ent.tier, Tier::Enterprise);
        assert_eq!(ent.device_cap, Tier::Enterprise.device_cap());
        assert!(ent.project_id.is_empty());
    }

    #[test]
    fn valid_license_yields_claimed_tier_and_default_cap() {
        let token = signed(Tier::Pro, None, 3_600);
        let ent = resolve_entitlement(&token, SECRET, Tier::Enterprise).unwrap();
        assert_eq!(ent.tier, Tier::Pro);
        assert_eq!(ent.device_cap, Tier::Pro.device_cap()); // 1_000
        assert_eq!(ent.project_id, "proj_abc");
    }

    #[test]
    fn license_device_cap_overrides_tier_default() {
        // A negotiated cap wins over the tier's built-in cap. This is the knob
        // that lets managed-server enforcement tests reject the Nth device
        // cheaply (cap=2 → 3rd connect rejected) without spinning 100+.
        let token = signed(Tier::Pro, Some(2), 3_600);
        let ent = resolve_entitlement(&token, SECRET, Tier::Enterprise).unwrap();
        assert_eq!(ent.tier, Tier::Pro);
        assert_eq!(ent.device_cap, 2);
    }

    #[test]
    fn tampered_signature_is_fatal() {
        let mut token = signed(Tier::Pro, None, 3_600);
        let last = token.pop().unwrap();
        token.push(if last == 'a' { 'b' } else { 'a' });
        assert!(resolve_entitlement(&token, SECRET, Tier::Enterprise).is_err());
    }

    #[test]
    fn wrong_secret_is_fatal() {
        let token = signed(Tier::Pro, None, 3_600);
        assert!(resolve_entitlement(&token, b"wrong-secret", Tier::Enterprise).is_err());
    }

    #[test]
    fn expired_license_is_fatal() {
        let token = signed(Tier::Pro, None, -1);
        let err = resolve_entitlement(&token, SECRET, Tier::Enterprise).unwrap_err();
        assert!(matches!(err, LicenseError::Expired));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(project: &str, tier: Tier, valid_secs: i64) -> LicenseClaims {
        LicenseClaims {
            project_id: project.into(),
            tier,
            expires_at: OffsetDateTime::now_utc().unix_timestamp() + valid_secs,
            device_cap: None,
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let secret = b"nostos-cloud-secret";
        let c = claims("proj_123", Tier::Pro, 3600);
        let token = c.sign(secret).unwrap();
        let back = LicenseClaims::verify(&token, secret).unwrap();
        assert_eq!(back.project_id, "proj_123");
        assert_eq!(back.tier, Tier::Pro);
    }

    #[test]
    fn rejects_tampered_token() {
        let secret = b"secret";
        let c = claims("p", Tier::Pro, 3600);
        let mut token = c.sign(secret).unwrap();
        // Flip the last sig char.
        let last = token.pop().unwrap();
        token.push(if last == 'a' { 'b' } else { 'a' });
        assert!(matches!(
            LicenseClaims::verify(&token, secret),
            Err(LicenseError::BadSignature)
        ));
    }

    #[test]
    fn rejects_expired() {
        let secret = b"secret";
        let mut c = claims("p", Tier::Hobby, 3600);
        c.expires_at = OffsetDateTime::now_utc().unix_timestamp() - 1;
        let token = c.sign(secret).unwrap();
        assert!(matches!(
            LicenseClaims::verify(&token, secret),
            Err(LicenseError::Expired)
        ));
    }

    #[test]
    fn rejects_wrong_secret() {
        let c = claims("p", Tier::Scale, 3600);
        let token = c.sign(b"secret-a").unwrap();
        assert!(LicenseClaims::verify(&token, b"secret-b").is_err());
    }

    #[test]
    fn tier_device_caps_match_strategy() {
        // Concurrent-device caps (not registered). See Tier::device_cap doc.
        assert_eq!(Tier::Hobby.device_cap(), 100);
        assert_eq!(Tier::Pro.device_cap(), 1_000);
        assert_eq!(Tier::Scale.device_cap(), 10_000);
        assert_eq!(Tier::Enterprise.device_cap(), u64::MAX);
    }

    #[test]
    fn base64url_roundtrips_arbitrary_bytes() {
        for payload in [
            vec![0u8],
            vec![1, 2, 3],
            vec![0xff; 32],
            b"{\"tier\":\"pro\"}".to_vec(),
        ] {
            let enc = base64url_encode(&payload);
            let dec = base64url_decode(&enc).unwrap();
            assert_eq!(dec, payload, "roundtrip failed for {payload:?}");
        }
    }
}
