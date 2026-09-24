use super::DEFAULT_SESSION_BUFFER;
#[cfg(doc)]
use super::{handshake::sync_handler, session::run_session};
use nostos_application::ports::{
    Metrics, OpLogSource, SchemaSource, SnapshotSource, SyncAuth, WriteBack,
};
use nostos_application::{ActiveRuleset, SessionManager};
use std::collections::HashSet;
use std::sync::Arc;

/// Shared state injected into the axum router.
#[derive(Clone)]
pub struct SyncRouterState {
    pub manager: Arc<SessionManager>,
    pub session_buffer: usize,
    /// The server-wide metrics handle (ADR-0025 slice 4b). Read for
    /// `slot_epoch` — the reconnect-resume gate compares the client's epoch to
    /// it. Defaults to a throwaway `Metrics::new()` in [`Self::new`]; the
    /// composition root injects the real shared handle via [`Self::with_metrics`].
    pub metrics: Arc<Metrics>,
    pub auth: Arc<dyn SyncAuth>,
    /// When set, every predicate is AND-constrained to `tenant_column =
    /// principal.tenant_id` (server-enforced, never client-attested). `None`
    /// means no tenant enforcement (single-tenant / anonymous deploys).
    pub tenant_column: Option<String>,
    /// The write-back port (ADR-0013). Defaults to [`NoWriteBack`] (the
    /// fake-mode stub that refuses every call); the composition root injects
    /// `PgWriteBack` under `NOSTOS_REPLICATOR=pg` with feature `pg`.
    pub write_back: Arc<dyn WriteBack>,
    /// The set of tables clients may write to (ADR-0013). Enforced by the
    /// transport FIRST — before the adapter is called — so the allowlist is a
    /// single trust-boundary check that holds regardless of which adapter is
    /// injected (the `PgWriteBack` adapter re-validates it as
    /// defense-in-depth). Empty = no tables writable. Defaults empty.
    pub write_tables: Arc<HashSet<String>>,
    /// ADR-0040: when true, a session that sheds events (buffer full) sends
    /// one `resync_required` control frame so the client clears + reconciles.
    /// Opt-in via `NOSTOS_RESYNC_SIGNAL=1`; older clients never receive the
    /// frame, so the default stays off.
    pub resync_signal: bool,
    /// The snapshot-on-subscribe port (ADR-0014). When set, a freshly-
    /// subscribing session receives the table's pre-existing rows as `Insert`
    /// events BEFORE live fan-out. `None` means snapshot-on-subscribe is off
    /// (the `FakeReplicator` path, or a binary built without feature `pg`).
    /// The composition root injects `PgSnapshotter` under
    /// `NOSTOS_REPLICATOR=pg`.
    pub snapshotter: Option<Arc<dyn SnapshotSource>>,
    /// The typed-schema port (WS1). When set, `GET /schema` serves the
    /// publication's tables/columns/affinities for client auto-schema. `None`
    /// means the endpoint returns 404 (the `FakeReplicator` path, or a binary
    /// built without feature `pg`). Injected under `NOSTOS_REPLICATOR=pg`.
    pub schema_source: Option<Arc<dyn SchemaSource>>,
    /// The op-log replay port (ADR-0025 slice 4b). When set + the client's
    /// epoch matches + its `resume_lsn` is in-window, `register_subscribe`
    /// replays the offline gap from `cairn_oplog` instead of full-snapshotting.
    /// `None` (fake mode, or a binary built without feature `pg`) → always
    /// snapshot. Injected under `NOSTOS_REPLICATOR=pg`.
    pub oplog_reader: Option<Arc<dyn OpLogSource>>,
    /// The active ruleset (ADR-0031). Swapped at runtime by the reload watcher
    /// (Task 14) and by `PUT /rules` (Task 20); read at SUBSCRIBE time only —
    /// never per delivered event. Defaults to `ActiveRuleset::all_mode()`.
    pub rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
    /// Broadcasts the active ruleset's checksum. A per-connection
    /// `Receiver::changed()` arm is free while unchanged, so live-session
    /// invalidation (Task 14) costs nothing on the delivery path.
    /// Defaults to a channel seeded with `ActiveRuleset::all_mode().checksum()`.
    pub rules_changed: tokio::sync::watch::Receiver<u64>,
    /// The send half of the same channel `rules_changed` receives from
    /// (Task 20). `PUT /rules` swaps `rules` then sends on this — the exact
    /// swap-then-notify order `watch_rules` uses — so every live session's
    /// `rules_changed` clone wakes immediately instead of waiting for the
    /// next poll tick. Must always be the paired sender for `rules_changed`;
    /// [`Self::with_rules`] sets both together so they can't drift apart.
    pub rules_tx: tokio::sync::watch::Sender<u64>,
    /// Path `PUT /rules` atomically writes `nostos_rules.toml` to (Task 20).
    /// Defaults to `nostos_rules.toml` (relative to cwd); the composition root
    /// overrides via [`Self::with_rules_file_path`] to match
    /// `--rules-file`/`NOSTOS_RULES_FILE`.
    pub rules_file_path: std::path::PathBuf,
    /// Replicator→fan-out driver liveness (audit 2026-08-17 M6). The
    /// composition root flips this when the driver task EXITS (stream end);
    /// `/healthz` folds it into a 503 `"degraded"` so a load balancer
    /// stops routing to a zombie server that accepts `/sync` but delivers
    /// no live events. `None` = not wired (tests) = treated live.
    pub driver_dead: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Browser origins permitted to open `/sync`. **Empty = no check**, which
    /// is the default and preserves every existing deployment.
    ///
    /// Opt-in for a concrete reason: a non-empty default would reject the one
    /// browser client in this repo (`nostos-ffi-wasm`, driven by the Svelte
    /// demo) on upgrade, and every native client — `nostos-client`,
    /// `nostos-bench`, the Flutter SDK via FFI, and all 30-odd integration
    /// tests — reaches this on `tokio-tungstenite`, which sends **no** `Origin`
    /// header at all.
    ///
    /// That asymmetry is also why an absent `Origin` passes rather than fails:
    /// only browsers set it, and only browsers can be *forced* to set it
    /// truthfully. A native attacker is not constrained by this header in
    /// either direction, so rejecting on absence would break every legitimate
    /// native client while stopping nobody. What it does buy — and the audit's
    /// actual concern — is that on an `AllowAnonymous` deployment a random page
    /// on the internet can no longer open a socket.
    ///
    /// **Scope: the axum `/sync` route only.** The iroh transport
    /// (`crate::iroh_sync`) calls [`run_session`] directly and never passes
    /// through [`sync_handler`], so this list does not apply there. That is
    /// correct rather than a hole — iroh is P2P QUIC, there is no browser and
    /// no `Origin` header to check — but do not read this field as "every
    /// transport is restricted".
    pub allowed_origins: Arc<Vec<String>>,
}

impl SyncRouterState {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, auth: Arc<dyn SyncAuth>) -> Self {
        let rules = ActiveRuleset::all_mode();
        // No reload watcher wired by default — the sender has no consumer
        // until the composition root creates one (main.rs) and passes the
        // matching `Receiver` via `with_rules`.
        let (rules_tx, rules_changed) = tokio::sync::watch::channel(rules.checksum());
        Self {
            manager,
            session_buffer: DEFAULT_SESSION_BUFFER,
            metrics: Arc::new(Metrics::new()),
            auth,
            tenant_column: None,
            write_back: Arc::new(crate::write_back::NoWriteBack::new()),
            write_tables: Arc::new(HashSet::new()),
            resync_signal: false,
            snapshotter: None,
            schema_source: None,
            oplog_reader: None,
            rules: Arc::new(tokio::sync::RwLock::new(rules)),
            rules_changed,
            rules_tx,
            rules_file_path: std::path::PathBuf::from("nostos_rules.toml"),
            driver_dead: None,
            allowed_origins: Arc::new(Vec::new()),
        }
    }

    /// ADR-0040: enable the `resync_required` continuity signal.
    #[must_use]
    pub fn with_resync_signal(mut self, enabled: bool) -> Self {
        self.resync_signal = enabled;
        self
    }

    /// Wire the driver-liveness flag (M6) — see [`Self::driver_dead`].
    #[must_use]
    pub fn with_driver_dead(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.driver_dead = Some(flag);
        self
    }

    /// Restrict `/sync` to a set of browser origins. Empty (the default) keeps
    /// the check off entirely — see [`SyncRouterState::allowed_origins`].
    #[must_use]
    pub fn with_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = Arc::new(origins);
        self
    }

    /// Set the per-session bounded buffer depth.
    #[must_use]
    pub fn with_buffer(mut self, buffer: usize) -> Self {
        self.session_buffer = buffer.max(1);
        self
    }

    /// Set the tenant column used to inject server-enforced predicates.
    #[must_use]
    pub fn with_tenant_column(mut self, column: impl Into<String>) -> Self {
        self.tenant_column = Some(column.into());
        self
    }

    /// Inject the write-back adapter (ADR-0013). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgWriteBack`; otherwise the default
    /// `NoWriteBack` stub surfaces a clear "write-back requires pg replicator"
    /// error to any client attempting a write.
    #[must_use]
    pub fn with_write_back(mut self, wb: Arc<dyn WriteBack>) -> Self {
        self.write_back = wb;
        self
    }

    /// Set the writable-table allowlist (ADR-0013). Enforced by the transport
    /// before the adapter is called. Build from `NOSTOS_WRITE_TABLES` via
    /// [`crate::parse_allowlist`].
    #[must_use]
    pub fn with_write_tables(mut self, tables: HashSet<String>) -> Self {
        self.write_tables = Arc::new(tables);
        self
    }

    /// Inject the snapshot-on-subscribe adapter (ADR-0014). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgSnapshotter`; otherwise leave it `None`
    /// (the default) so subscribe-time snapshots are skipped and clients rely
    /// on live fan-out alone.
    #[must_use]
    pub fn with_snapshotter(mut self, snap: Arc<dyn SnapshotSource>) -> Self {
        self.snapshotter = Some(snap);
        self
    }

    /// Inject the typed-schema adapter (WS1). Call under `NOSTOS_REPLICATOR=pg`
    /// with a `PgSchemaSource`; otherwise leave it `None` (the default) so
    /// `GET /schema` returns 404.
    #[must_use]
    pub fn with_schema_source(mut self, src: Arc<dyn SchemaSource>) -> Self {
        self.schema_source = Some(src);
        self
    }

    /// Inject the server-wide metrics handle (ADR-0025 slice 4b). The
    /// composition root passes the same `Arc<Metrics>` the replicator bumps
    /// `slot_epoch` into, so `register_subscribe` reads the live epoch. The
    /// default in [`Self::new`] is a throwaway (slot_epoch stays 0 → the gate
    /// forces snapshot, which is correct for tests / fake mode).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Inject the op-log replay adapter (ADR-0025 slice 4b). Call under
    /// `NOSTOS_REPLICATOR=pg` with a `PgOpLogReader`; otherwise leave it `None`
    /// (the default) so reconnecting clients always take the snapshot path.
    #[must_use]
    pub fn with_oplog_reader(mut self, reader: Arc<dyn OpLogSource>) -> Self {
        self.oplog_reader = Some(reader);
        self
    }

    /// Inject the active ruleset (ADR-0031), its checksum-change receiver, and
    /// the paired sender (Task 20's `PUT /rules` sends on it after a swap).
    /// The composition root loads `nostos_rules.toml` (or falls back to
    /// [`ActiveRuleset::all_mode`]), creates one
    /// `tokio::sync::watch::channel`, and passes both halves here — `rules_tx`
    /// must be the sender that channel's `rules_changed` was subscribed from,
    /// or `PUT /rules` will notify a channel no live session is listening on.
    #[must_use]
    pub fn with_rules(
        mut self,
        rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
        rules_changed: tokio::sync::watch::Receiver<u64>,
        rules_tx: tokio::sync::watch::Sender<u64>,
    ) -> Self {
        self.rules = rules;
        self.rules_changed = rules_changed;
        self.rules_tx = rules_tx;
        self
    }

    /// Set the path `PUT /rules` atomically writes `nostos_rules.toml` to
    /// (Task 20). The composition root calls this with `--rules-file` /
    /// `NOSTOS_RULES_FILE` so the HTTP write target matches what `main()`
    /// loaded at boot and what `watch_rules` polls.
    #[must_use]
    pub fn with_rules_file_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.rules_file_path = path.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_application::ports::SessionStore;

    // (ADR-0031) SyncRouterState::new defaults to the permissive zero-config
    // ruleset so no existing construction site (none of which pass rules)
    // breaks.
    #[tokio::test]
    async fn router_state_defaults_to_all_mode() {
        let store: Arc<dyn SessionStore> = Arc::new(crate::store::InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn SyncAuth> = Arc::new(crate::auth::AllowAnonymous::new());
        let state = SyncRouterState::new(manager, auth);
        assert_eq!(
            state.rules.read().await.mode(),
            nostos_domain::SyncMode::All
        );
    }
}
