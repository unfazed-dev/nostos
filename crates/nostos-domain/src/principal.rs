//! Authenticated identity for a sync session — the anti-self-attestation type.
//!
//! A [`Principal`] is what the `SyncAuth` port (application layer) resolves a
//! client's bearer token into. It rides into the [`SyncSession`] so the server
//! can enforce that the client's predicate never escapes its authorized scope
//! (ADR-0011: server-enforced predicates).
//!
//! Before this type existed, `/sync` had no identity at all — the client's
//! `Subscribe` predicate was trusted verbatim, so any connected client could
//! read any tenant's rows. The `Principal` is the value an authenticated
//! session is scoped to; the transport injects the tenant filter from it rather
//! than trusting whatever the client sent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The authenticated identity of one sync connection.
///
/// - `account_id` — the user/account the token was minted for (Supabase `sub`).
/// - `tenant_id` — the row-level scope this account belongs to (the column the
///   server ANDs into every predicate, e.g. `org_id`). When the deployment does
///   not configure a tenant column, this is unused but still carried so the
///   type is uniform.
/// - `extra` — flat, string-valued JWT claims beyond `sub`/`tenant_id`, keyed
///   by claim name (ADR-0031, D1). Consulted by the rules grammar's
///   `claims.<field>` references via [`Self::claim`]. Populated by the auth
///   adapter (`nostos_infra::auth`), which is also where the size caps and
///   reserved-name filtering live — this type only stores whatever it is
///   handed.
///
/// `Debug` is hand-implemented (not derived) to print `extra`'s claim *names*
/// only, never its values — a claim can carry user-controlled secrets and
/// must never round-trip into a log line via `{:?}`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal {
    pub account_id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl std::fmt::Debug for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names only, never values — `extra` is attacker/user-controlled JWT
        // claim data and must never leak into a log line via `{:?}`. Already
        // sorted: `extra` is a `BTreeMap`.
        let claim_names: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        f.debug_struct("Principal")
            .field("account_id", &self.account_id)
            .field("tenant_id", &self.tenant_id)
            .field("extra_claim_names", &claim_names)
            .finish()
    }
}

impl Principal {
    /// Construct a principal from explicit id parts, with no extra claims.
    #[inline]
    #[must_use]
    pub fn new(account_id: impl Into<String>, tenant_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            tenant_id: tenant_id.into(),
            extra: BTreeMap::new(),
        }
    }

    /// Construct a principal carrying additional flat JWT claims (ADR-0031,
    /// D1). `extra` must already be filtered/validated by the caller — this
    /// constructor does not re-check reserved names or size caps.
    #[inline]
    #[must_use]
    pub fn with_claims(
        account_id: impl Into<String>,
        tenant_id: impl Into<String>,
        extra: BTreeMap<String, String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            tenant_id: tenant_id.into(),
            extra,
        }
    }

    /// The anonymous principal used when `NOSTOS_SYNC_AUTH=none` (OSS self-host
    /// dev default). Every connection is the same undifferentiated identity;
    /// no tenant filter is injected. A managed deploy never mints this.
    #[inline]
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            account_id: String::new(),
            tenant_id: String::new(),
            extra: BTreeMap::new(),
        }
    }

    /// True for the [`Self::anonymous`] principal — no real identity was
    /// authenticated, so the server must NOT inject a tenant filter (there is
    /// no tenant to scope to).
    #[inline]
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.account_id.is_empty()
    }

    /// Resolve a `claims.<field>` reference (ADR-0031's rules grammar).
    /// `"sub"` resolves from `account_id`, `"tenant_id"` from `tenant_id`,
    /// anything else from `extra`. `None` means "this principal has no such
    /// claim" — the caller (rules evaluation) must deny, never fall back to
    /// an empty-string match. In particular an empty `account_id`/`tenant_id`
    /// (the anonymous principal) does not resolve `"sub"`/`"tenant_id"` to
    /// `Some("")`.
    #[inline]
    #[must_use]
    pub fn claim(&self, field: &str) -> Option<&str> {
        match field {
            "sub" => (!self.account_id.is_empty()).then_some(self.account_id.as_str()),
            "tenant_id" => (!self.tenant_id.is_empty()).then_some(self.tenant_id.as_str()),
            other => self.extra.get(other).map(String::as_str),
        }
    }
}

/// A server-computed tenant scope for one write (ADR-0018, extending
/// ADR-0011's read-side injection to the write path). Carries the operator-
/// configured tenant column name and the authenticated principal's tenant
/// value — never the client's own claim.
///
/// Construction is gated by the SAME condition as the read-side predicate
/// injection ([`Principal::is_anonymous`] + a configured tenant column) — see
/// [`Principal::tenant_scope`], the single seam both paths call so the two
/// enforcement points can't drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantScope<'a> {
    /// The tenant column name (operator config, e.g. `org_id`).
    pub column: &'a str,
    /// The authenticated principal's tenant value — server-derived, never
    /// client-attested.
    pub value: &'a str,
}

impl<'a> TenantScope<'a> {
    #[inline]
    #[must_use]
    pub fn new(column: &'a str, value: &'a str) -> Self {
        Self { column, value }
    }
}

impl Principal {
    /// The single seam that decides IF tenant scoping applies, for both the
    /// read path (`build_predicate`) and the write path (`dispatch_write`):
    /// a tenant column must be configured AND the principal must be a real,
    /// authenticated identity (not [`Principal::anonymous`] — there is no
    /// tenant to scope an anonymous connection to). Returns `None` when
    /// either condition fails, `Some(scope)` otherwise.
    #[inline]
    #[must_use]
    pub fn tenant_scope<'a>(&'a self, tenant_column: Option<&'a str>) -> Option<TenantScope<'a>> {
        let column = tenant_column?;
        if self.is_anonymous() {
            return None;
        }
        Some(TenantScope::new(column, &self.tenant_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_scope_none_when_anonymous() {
        let p = Principal::anonymous();
        assert!(p.tenant_scope(Some("org_id")).is_none());
    }

    #[test]
    fn tenant_scope_none_when_no_column_configured() {
        let p = Principal::new("u1", "acme");
        assert!(p.tenant_scope(None).is_none());
    }

    #[test]
    fn tenant_scope_some_for_authenticated_principal_with_column() {
        let p = Principal::new("u1", "acme");
        let scope = p.tenant_scope(Some("org_id")).expect("scoped");
        assert_eq!(scope.column, "org_id");
        assert_eq!(scope.value, "acme");
    }

    #[test]
    fn anonymous_is_flagged() {
        let p = Principal::anonymous();
        assert!(p.is_anonymous());
        assert_eq!(p.account_id, "");
        assert_eq!(p.tenant_id, "");
    }

    #[test]
    fn real_principal_is_not_anonymous() {
        let p = Principal::new("user-123", "org-acme");
        assert!(!p.is_anonymous());
        assert_eq!(p.account_id, "user-123");
        assert_eq!(p.tenant_id, "org-acme");
    }

    #[test]
    fn claim_resolves_sub_tenant_and_extra() {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("org_id".to_string(), "acme".to_string());
        let p = Principal::with_claims("u1", "t1", extra);
        assert_eq!(p.claim("sub"), Some("u1"));
        assert_eq!(p.claim("tenant_id"), Some("t1"));
        assert_eq!(p.claim("org_id"), Some("acme"));
        assert_eq!(p.claim("role"), None);
    }

    #[test]
    fn anonymous_has_no_claims() {
        let p = Principal::anonymous();
        assert_eq!(p.claim("sub"), None);
    }

    #[test]
    fn claims_do_not_leak_into_logs() {
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("secret_token".to_string(), "hunter2".to_string());
        let p = Principal::with_claims("u1", "t1", extra);
        let rendered = format!("{p:?}");
        assert!(
            !rendered.contains("hunter2"),
            "Debug rendering must not leak claim values: {rendered}"
        );
    }
}
