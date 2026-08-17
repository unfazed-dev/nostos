//! API-key auth (ADR-0038 §2, plan task 1.3; key model pinned 0.2, role
//! suffix added by the 2026-08-17 security-audit closeout, plan task 4.1).
//!
//! Bearer auth against the env-seeded key list (NOSTOS_PUSHD_API_KEYS).
//! The presented secret is compared in constant time via subtle over
//! SHA-256 digests of both sides — the nostos-server admin_auth.rs idiom:
//! digests are always 32 bytes, so there is no unequal-length branch to
//! reason about before the constant-time compare even starts. On a match
//! the TENANT and the key's ROLE are stamped into the request extensions
//! (ADR-0018 discipline — server-derived, never client-attested);
//! everything else is a 401 with the contract error shape. /v1/healthz
//! lives on a separate, unauthenticated router.
//!
//! Roles (audit finding 1): an entry is `tenant:secret[:rail]`. The
//! optional literal `:rail` suffix grants the Rail role — the only role
//! permitted to use rail-mode dispatch (POST /v1/send with an unregistered
//! token + `platform` field, the mode nostos-server's RemoteNotifier
//! delegates with). Standard keys keep registry-path sends only, so a
//! leaked tenant key cannot push to arbitrary tokens it happens to know.
//! A secret that itself ends in `:rail` is rejected at boot: the suffix
//! is reserved, so such a secret is unexpressible without ambiguity.
//!
//! ponytail: CLI key CRUD + hashed-at-rest storage deferred to v1.1 (pin
//! 0.2) — keys live in .env under the same threat model as the rail
//! secrets; upgrade path is a keyed table in the registry store plus this
//! same identify seam returning digests from disk instead of env.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::api::AppState;

/// The authenticated tenant, stamped into request extensions by
/// [auth_middleware] and extracted by every authed handler. HTTP land can
/// never construct one — the tenant is always the one the matched key
/// belongs to.
#[derive(Debug, Clone)]
pub struct TenantId(pub String);

/// What the matched key may do — stamped into request extensions next to
/// [TenantId] (audit finding 1, plan task 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRole {
    /// Registry-path sends only: /v1/tokens + /v1/send for tokens this
    /// tenant registered.
    Standard,
    /// Standard rights PLUS rail-mode dispatch (unregistered token +
    /// `platform` field) — the trusted-delegator role nostos-server's
    /// RemoteNotifier authenticates with. Opt-in per key via the
    /// `:rail` suffix in NOSTOS_PUSHD_API_KEYS.
    Rail,
}

/// One env-seeded key: the tenant it authenticates, its role, plus the
/// SHA-256 digest of its secret (raw secrets are never retained after
/// boot).
#[derive(Debug, Clone)]
struct ApiKeyEntry {
    tenant: String,
    role: KeyRole,
    secret_digest: [u8; 32],
}

/// The parsed, boot-validated key list (plan pin 0.2; role suffix per plan
/// task 4.1). Construction goes through [ApiKeys::parse], which fails fast
/// on anything malformed so a half-configured daemon never starts.
#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    entries: Vec<ApiKeyEntry>,
}

/// The literal suffix that marks a Rail key — reserved, so no Standard
/// secret may end with it.
const RAIL_SUFFIX: &str = ":rail";

impl ApiKeys {
    /// Parse the NOSTOS_PUSHD_API_KEYS value: one or more comma-separated
    /// `tenant:secret[:rail]` entries. The optional trailing `:rail`
    /// marks the key Rail-role; without it the key is Standard. Duplicated
    /// tenants are rejected (ambiguous stamping); a secret may itself
    /// contain colons but never commas, and may never end with the reserved
    /// `:rail` suffix (boot error — pick another secret).
    ///
    /// # Errors
    /// anyhow describing the first malformed entry (the env var name is
    /// included — this runs exactly once at boot).
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "NOSTOS_PUSHD_API_KEYS is required: set it to one or more comma-separated \\
                 tenant:secret[:rail] pairs, e.g. acme:s3cr3t,hq:delegator-key:rail"
            );
        }
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty entry (stray comma?)");
            }
            let Some((tenant, rest)) = part.split_once(':') else {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: entry is not tenant:secret");
            };
            let tenant = tenant.trim();
            if tenant.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty tenant");
            }
            // Role suffix (audit finding 1): one trailing ":rail" marks the
            // Rail role; a secret that STILL ends with it afterwards is the
            // reserved-suffix violation — fail fast, it is unexpressible.
            let (secret, role) = match rest.strip_suffix(RAIL_SUFFIX) {
                Some(s) => (s, KeyRole::Rail),
                None => (rest, KeyRole::Standard),
            };
            if secret.ends_with(RAIL_SUFFIX) {
                anyhow::bail!(
                    "NOSTOS_PUSHD_API_KEYS: secret for tenant '{tenant}' ends with the reserved \\
                     suffix ':rail' — append ':rail' once to grant the rail role, or pick a secret \\
                     that does not end with it"
                );
            }
            if secret.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty secret for tenant");
            }
            if !seen.insert(tenant.to_string()) {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: duplicate tenant");
            }
            entries.push(ApiKeyEntry {
                tenant: tenant.to_string(),
                role,
                secret_digest: Sha256::digest(secret.as_bytes()).into(),
            });
        }
        Ok(Self { entries })
    }

    /// Number of configured tenants (boot logging).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a presented bearer secret to its (tenant, role). Iterates
    /// the key list comparing digests in constant time; the first match
    /// wins. The presented string is matched against the SECRET only — the
    /// `:rail` suffix is configuration, never part of what a client sends.
    #[must_use]
    pub fn identify(&self, presented: &str) -> Option<(&str, KeyRole)> {
        let candidate = Sha256::digest(presented.as_bytes());
        self.entries
            .iter()
            .find(|e| {
                e.secret_digest
                    .as_slice()
                    .ct_eq(candidate.as_slice())
                    .into()
            })
            .map(|e| (e.tenant.as_str(), e.role))
    }
}

/// Axum middleware (plan task 1.3): bearer gate + tenant/role stamping.
/// Runs on the authed sub-router only; /v1/healthz is merged in
/// unauthenticated.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(presented) = presented else {
        return unauthorized("missing bearer API key");
    };
    let Some((tenant, role)) = state.api_keys.identify(presented) else {
        return unauthorized("unknown API key");
    };
    req.extensions_mut().insert(TenantId(tenant.to_string()));
    req.extensions_mut().insert(role);
    next.run(req).await
}

/// The contract 401 shape — error string, never echoing the key.
fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::{ApiKeys, KeyRole};

    #[test]
    fn parses_pairs_and_identifies() {
        let keys = ApiKeys::parse("acme:s3cr3t, globex:other").expect("valid list");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys.identify("s3cr3t"), Some(("acme", KeyRole::Standard)));
        assert_eq!(keys.identify("other"), Some(("globex", KeyRole::Standard)));
        assert_eq!(keys.identify("wrong"), None);
        assert_eq!(keys.identify(""), None);
    }

    #[test]
    fn rail_suffix_grants_rail_role() {
        let keys = ApiKeys::parse("acme:s3cr3t, hq:delegator-key:rail").expect("valid list");
        assert_eq!(keys.identify("s3cr3t"), Some(("acme", KeyRole::Standard)));
        // The client presents the SECRET only — no suffix on the wire.
        assert_eq!(keys.identify("delegator-key"), Some(("hq", KeyRole::Rail)));
        assert_eq!(keys.identify("delegator-key:rail"), None);
    }

    #[test]
    fn rejects_empty() {
        assert!(ApiKeys::parse("").is_err());
        assert!(ApiKeys::parse("   ").is_err());
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(ApiKeys::parse("no-colon-here").is_err());
        assert!(ApiKeys::parse("acme:").is_err());
        assert!(ApiKeys::parse(":secret").is_err());
        assert!(ApiKeys::parse("acme:s1,,globex:s2").is_err());
        assert!(
            ApiKeys::parse("acme::rail").is_err(),
            "empty secret after strip"
        );
    }

    #[test]
    fn rejects_duplicate_tenant() {
        assert!(ApiKeys::parse("acme:s1,acme:s2").is_err());
        assert!(ApiKeys::parse("acme:s1,acme:s2:rail").is_err());
    }

    #[test]
    fn secret_may_contain_colons() {
        let keys = ApiKeys::parse("acme:aa:bb:cc").expect("colon-bearing secret");
        assert_eq!(keys.identify("aa:bb:cc"), Some(("acme", KeyRole::Standard)));
    }

    #[test]
    fn secret_ending_in_rail_suffix_is_rejected_at_boot() {
        // "aa:rail:rail" strips one suffix, leaves a secret that STILL ends
        // with the reserved suffix — the fail-fast case (documented).
        let err = ApiKeys::parse("acme:aa:rail:rail").expect_err("reserved suffix");
        assert!(
            err.to_string().contains("reserved"),
            "names the rule: {err}"
        );
    }
}
