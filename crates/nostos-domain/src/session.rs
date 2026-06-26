//! Sync sessions — one per connected client.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::predicate::Predicate;

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

/// One connected client's subscription: an id + the predicate it asked for.
///
/// The `SessionStore` (application port) indexes these by `predicate.table`
/// so the router can find candidate sessions in O(1) per incoming event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSession {
    pub id: SessionId,
    pub predicate: Predicate,
}

impl SyncSession {
    #[inline]
    #[must_use]
    pub fn new(predicate: Predicate) -> Self {
        Self {
            id: SessionId::new(),
            predicate,
        }
    }

    /// Construct with a fixed id (useful in tests / deterministic benchmarks).
    #[inline]
    #[must_use]
    pub fn with_id(id: SessionId, predicate: Predicate) -> Self {
        Self { id, predicate }
    }

    /// The table this session is subscribed to (for indexing).
    #[inline]
    #[must_use]
    pub fn table(&self) -> &str {
        &self.predicate.table
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
}
