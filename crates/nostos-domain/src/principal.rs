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

/// The authenticated identity of one sync connection.
///
/// - `account_id` — the user/account the token was minted for (Supabase `sub`).
/// - `tenant_id` — the row-level scope this account belongs to (the column the
///   server ANDs into every predicate, e.g. `org_id`). When the deployment does
///   not configure a tenant column, this is unused but still carried so the
///   type is uniform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal {
    pub account_id: String,
    pub tenant_id: String,
}

impl Principal {
    /// Construct a principal from explicit id parts.
    #[inline]
    #[must_use]
    pub fn new(account_id: impl Into<String>, tenant_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            tenant_id: tenant_id.into(),
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
}
