//! `SessionManager` — the use-case that adds/removes sessions as clients
//! connect and disconnect.
//!
//! This is the driving-side entry point the WebSocket transport adapter calls
//! on connection open/close. It owns no state itself — it delegates to the
//! [`SessionStore`] port, keeping the connection lifecycle logic in the
//! application layer and the storage mechanism in infra.

use std::sync::Arc;

use nostos_domain::{SessionId, SyncSession};

use crate::ports::{EventSink, SessionStore};

/// Manages the lifecycle of sync sessions: register on connect, unregister on
/// disconnect.
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
}

impl SessionManager {
    #[inline]
    #[must_use]
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    /// Register a new session. Called by the transport when a client subscribes.
    pub async fn connect(&self, session: SyncSession, sink: Arc<dyn EventSink>) -> SessionId {
        let id = session.id;
        self.store.add(session, sink).await;
        id
    }

    /// Unregister a session. Called by the transport when the connection closes.
    pub async fn disconnect(&self, id: SessionId) {
        self.store.remove(id).await;
    }

    /// Current live session count (for health/metrics endpoints).
    pub async fn session_count(&self) -> usize {
        self.store.len().await
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
        let mgr = SessionManager::new(store);
        assert_eq!(mgr.session_count().await, 0);

        let s = SyncSession::new(Predicate::all("tasks"));
        let id = mgr.connect(s, Arc::new(NoopSink)).await;
        assert_eq!(mgr.session_count().await, 1);

        mgr.disconnect(id).await;
        assert_eq!(mgr.session_count().await, 0);
    }
}
