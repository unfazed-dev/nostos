//! `SessionManager` — the use-case that adds/removes sessions as clients
//! connect and disconnect.
//!
//! This is the driving-side entry point the WebSocket transport adapter calls
//! on connection open/close. It owns no state itself — it delegates to the
//! [`SessionStore`] port, keeping the connection lifecycle logic in the
//! application layer and the storage mechanism in infra.
//!
//! **Concurrent-device cap (reactive-default strategy):** `connect` enforces a
//! peak concurrent-session limit derived from the instance's licensed tier
//! (`Tier::device_cap`). This is the single chokepoint — the cap lives in the
//! application layer (where the tier is known) rather than cloud middleware,
//! so a self-hosted OSS instance with no Cloud license still gets its cap.

use std::sync::Arc;

use nostos_domain::{SessionId, SyncSession};

use crate::ports::{EventSink, SessionStore};

/// Why a `connect` was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The instance is at its concurrent-device cap for the licensed tier.
    #[error("concurrent device cap reached ({cap} for tier {tier:?})")]
    DeviceCapReached { tier: nostos_domain::Tier, cap: u64 },
}

/// Manages the lifecycle of sync sessions: register on connect, unregister on
/// disconnect.
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    /// The tier this instance is licensed for; gates the concurrent-device cap.
    tier: nostos_domain::Tier,
}

impl SessionManager {
    /// Construct at a fixed tier. The tier's `device_cap()` becomes the
    /// concurrent-session ceiling enforced on every `connect`.
    #[inline]
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>, tier: nostos_domain::Tier) -> Self {
        Self { store, tier }
    }

    /// Register a new session. Called by the transport when a client subscribes.
    ///
    /// Returns `Err(DeviceCapReached)` when accepting the session would exceed
    /// the licensed tier's concurrent-device ceiling. The transport should close
    /// the connection with a 429-style signal in that case.
    pub async fn connect(
        &self,
        session: SyncSession,
        sink: Arc<dyn EventSink>,
    ) -> Result<SessionId, ConnectError> {
        let cap = self.tier.device_cap();
        let live = self.store.len().await;
        // Compare in u64 (widening usize is always safe). Enterprise's cap is
        // u64::MAX, so the check is never true for the unlimited tier.
        if live as u64 >= cap {
            return Err(ConnectError::DeviceCapReached {
                tier: self.tier,
                cap,
            });
        }
        let id = session.id;
        self.store.add(session, sink).await;
        Ok(id)
    }

    /// Unregister a session. Called by the transport when the connection closes.
    pub async fn disconnect(&self, id: SessionId) {
        self.store.remove(id).await;
    }

    /// Current live session count (for health/metrics endpoints).
    pub async fn session_count(&self) -> usize {
        self.store.len().await
    }

    /// The licensed tier this manager enforces.
    #[must_use]
    pub fn tier(&self) -> nostos_domain::Tier {
        self.tier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{DeliveryDecision, SessionCandidate};
    use async_trait::async_trait;
    use nostos_domain::{Predicate, ReplicationEvent};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct NoopSink;
    #[async_trait]
    impl EventSink for NoopSink {
        async fn deliver(&self, _e: ReplicationEvent) -> DeliveryDecision {
            DeliveryDecision::Delivered
        }
        fn is_open(&self) -> bool {
            true
        }
    }

    struct MapStore(Mutex<HashMap<SessionId, SessionCandidate>>);
    #[async_trait]
    impl SessionStore for MapStore {
        async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>) {
            self.0.lock().unwrap().insert(
                session.id,
                SessionCandidate {
                    id: session.id,
                    predicate: session.predicate,
                    sink,
                },
            );
        }
        async fn remove(&self, id: SessionId) {
            self.0.lock().unwrap().remove(&id);
        }
        async fn candidates_for(&self, _e: &ReplicationEvent) -> Vec<SessionCandidate> {
            self.0.lock().unwrap().values().cloned().collect()
        }
        async fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    #[tokio::test]
    async fn connect_and_disconnect_track_count() {
        let store = Arc::new(MapStore(Mutex::new(HashMap::new())));
        let mgr = SessionManager::new(store, nostos_domain::Tier::Enterprise);
        assert_eq!(mgr.session_count().await, 0);

        let s = SyncSession::new(Predicate::all("tasks"));
        let id = mgr.connect(s, Arc::new(NoopSink)).await.unwrap();
        assert_eq!(mgr.session_count().await, 1);

        mgr.disconnect(id).await;
        assert_eq!(mgr.session_count().await, 0);
    }

    #[tokio::test]
    async fn device_cap_rejects_over_limit() {
        // Ponytail: build a session store, fill it to the Hobby cap (100), then
        // assert the 101st connect is rejected with DeviceCapReached.
        let store = Arc::new(MapStore(Mutex::new(HashMap::new())));
        let mgr = SessionManager::new(store, nostos_domain::Tier::Hobby);
        for _ in 0..nostos_domain::Tier::Hobby.device_cap() {
            let s = SyncSession::new(Predicate::all("tasks"));
            mgr.connect(s, Arc::new(NoopSink)).await.unwrap();
        }
        let over = SyncSession::new(Predicate::all("tasks"));
        let err = mgr.connect(over, Arc::new(NoopSink)).await.unwrap_err();
        assert!(matches!(
            err,
            ConnectError::DeviceCapReached {
                tier: nostos_domain::Tier::Hobby,
                cap: 100
            }
        ));
    }
}
