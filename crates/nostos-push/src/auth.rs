//! API-key auth (ADR-0038 §2, plan task 1.3; key model pinned 0.2).
//!
//! Bearer auth against the env-seeded key list (NOSTOS_PUSHD_API_KEYS).
//! The presented secret is compared in constant time via subtle over
//! SHA-256 digests of both sides — the nostos-server admin_auth.rs idiom:
//! digests are always 32 bytes, so there is no unequal-length branch to
//! reason about before the constant-time compare even starts. On a match
//! the TENANT is stamped into the request extensions (ADR-0018 discipline
//! — server-derived, never client-attested); everything else is a 401 with
//! the contract error shape. /v1/healthz lives on a separate,
//! unauthenticated router.
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

/// One env-seeded key: the tenant it authenticates plus the SHA-256 digest
/// of its secret (raw secrets are never retained after boot).
#[derive(Debug, Clone)]
struct ApiKeyEntry {
    tenant: String,
    secret_digest: [u8; 32],
}

/// The parsed, boot-validated key list (plan pin 0.2). Construction goes
/// through [ApiKeys::parse], which fails fast on anything malformed so a
/// half-configured daemon never starts.
#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    entries: Vec<ApiKeyEntry>,
}

impl ApiKeys {
    /// Parse the NOSTOS_PUSHD_API_KEYS value: one or more comma-separated
    /// tenant:secret pairs. Duplicated tenants are rejected (ambiguous
    /// stamping); a secret may itself contain colons but never commas.
    ///
    /// # Errors
    /// anyhow describing the first malformed entry (the env var name is
    /// included — this runs exactly once at boot).
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "NOSTOS_PUSHD_API_KEYS is required: set it to one or more                  comma-separated tenant:secret pairs, e.g. acme:s3cr3t"
            );
        }
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty entry (stray comma?)");
            }
            let Some((tenant, secret)) = part.split_once(':') else {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: entry is not tenant:secret");
            };
            let tenant = tenant.trim();
            if tenant.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty tenant");
            }
            if secret.is_empty() {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: empty secret for tenant");
            }
            if !seen.insert(tenant.to_string()) {
                anyhow::bail!("NOSTOS_PUSHD_API_KEYS: duplicate tenant");
            }
            entries.push(ApiKeyEntry {
                tenant: tenant.to_string(),
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

    /// Resolve a presented bearer secret to its tenant. Iterates the key
    /// list comparing digests in constant time; the first match wins.
    #[must_use]
    pub fn identify(&self, presented: &str) -> Option<&str> {
        let candidate = Sha256::digest(presented.as_bytes());
        self.entries
            .iter()
            .find(|e| {
                e.secret_digest
                    .as_slice()
                    .ct_eq(candidate.as_slice())
                    .into()
            })
            .map(|e| e.tenant.as_str())
    }
}

/// Axum middleware (plan task 1.3): bearer gate + tenant stamping. Runs on
/// the authed sub-router only; /v1/healthz is merged in unauthenticated.
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
    let Some(tenant) = state.api_keys.identify(presented) else {
        return unauthorized("unknown API key");
    };
    req.extensions_mut().insert(TenantId(tenant.to_string()));
    next.run(req).await
}

/// The contract 401 shape — error string, never echoing the key.
fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::ApiKeys;

    #[test]
    fn parses_pairs_and_identifies() {
        let keys = ApiKeys::parse("acme:s3cr3t, globex:other").expect("valid list");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys.identify("s3cr3t"), Some("acme"));
        assert_eq!(keys.identify("other"), Some("globex"));
        assert_eq!(keys.identify("wrong"), None);
        assert_eq!(keys.identify(""), None);
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
    }

    #[test]
    fn rejects_duplicate_tenant() {
        assert!(ApiKeys::parse("acme:s1,acme:s2").is_err());
    }

    #[test]
    fn secret_may_contain_colons() {
        let keys = ApiKeys::parse("acme:aa:bb:cc").expect("colon-bearing secret");
        assert_eq!(keys.identify("aa:bb:cc"), Some("acme"));
    }
}
