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

#[cfg(test)]
mod tests {
    use super::*;

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
