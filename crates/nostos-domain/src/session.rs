//! Sync sessions — one per connected client.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::predicate::Predicate;
use crate::principal::Principal;

/// Stable identifier for a sync session. A new one is minted per WebSocket
/// connection; the client references it when checkpointing its resume LSN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a fresh random session id.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One connected client's subscription: an id, the predicate it asked for, and
/// the authenticated identity the server enforces it against.
///
/// The `SessionStore` (application port) indexes these by `predicate.table`
/// so the router can find candidate sessions in O(1) per incoming event.
///
/// `principal` is `None` only for the legacy/unauthenticated path; once
/// `SyncAuth` is wired every session carries the resolved identity so the
/// transport can inject the tenant filter (ADR-0011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSession {
    pub id: SessionId,
    pub predicate: Predicate,
    /// The authenticated identity (anti-self-attestation). `None` marks a
    /// session created before auth wiring or under `NOSTOS_SYNC_AUTH=none`.
    pub principal: Option<Principal>,
}

impl SyncSession {
    /// Construct an unauthenticated session (predicates self-attested) — the
    /// pre-auth shape, retained for the benchmark and tests that don't model
    /// identity. Production paths use [`Self::new_authenticated`].
    #[inline]
    #[must_use]
    pub fn new(predicate: Predicate) -> Self {
        Self {
            id: SessionId::new(),
            predicate,
            principal: None,
        }
    }

    /// Construct a session scoped to an authenticated principal. The transport
    /// builds the predicate by injecting the principal's tenant filter before
    /// calling this, so the predicate here is already server-enforced.
    #[inline]
    #[must_use]
    pub fn new_authenticated(predicate: Predicate, principal: Principal) -> Self {
        Self {
            id: SessionId::new(),
            predicate,
            principal: Some(principal),
        }
    }

    /// The table this session is subscribed to (for indexing).
    #[inline]
    #[must_use]
    pub fn table(&self) -> &str {
        &self.predicate.table
    }

    /// The authenticated principal, if any.
    #[inline]
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::ColumnValue;

    #[test]
    fn session_ids_are_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn session_exposes_table_for_indexing() {
        let s = SyncSession::new(Predicate::eq("tasks", "org_id", ColumnValue::text("acme")));
        assert_eq!(s.table(), "tasks");
    }

    #[test]
    fn authenticated_session_carries_principal() {
        let s = SyncSession::new_authenticated(
            Predicate::all("tasks"),
            Principal::new("u1", "org-acme"),
        );
        let p = s
            .principal()
            .expect("authenticated session has a principal");
        assert_eq!(p.account_id, "u1");
        assert_eq!(p.tenant_id, "org-acme");
    }

    #[test]
    fn unauthenticated_session_has_no_principal() {
        let s = SyncSession::new(Predicate::all("tasks"));
        assert!(s.principal().is_none());
    }
}
