//! ADR-0031 D2 client half: a real `SyncClient` persists the rules checksum
//! from `resume_info` and resends it on the next `Subscribe`, so a
//! server-side rules change is detected by checksum mismatch (not just
//! epoch) and forces a snapshot instead of a stale replay.
//!
//! Companion to `epoch_persistence.rs` (which proves the epoch half and
//! documents that a fresh client stays on the *composed* fallback because it
//! has never sent a checksum). These tests exercise the *raw* advertisement
//! path (`rules_checksum_roundtrip`) and confirm the composed fallback is
//! stable and non-spammy when the client never adopts an explicit checksum
//! (`absent_checksum_is_accepted`).
//!
//! PG-free, same harness pattern as `epoch_persistence.rs`: a `CountingOpLog`
//! stands in for the real op-log so "did the reconnect take the replay path"
//! is observable as a call count, without needing a `SnapshotSource` (the
//! snapshot branch is a no-op in this harness regardless of gate outcome —
//! see `nostos-infra/src/transport.rs::register_subscribe`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nostos_application::ports::{Metrics, OpLogError, OpLogSource, SyncAuth};
use nostos_application::{ActiveRuleset, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_core::Storage;
use nostos_domain::{
    compose_sync_epoch, Lsn, ReplicationEvent, RowOp, SyncMode, SyncRules, TableRule, RULES_VERSION,
};
use nostos_infra::auth::AllowAnonymous;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};

const ADVERTISED_EPOCH: u64 = 7;

/// `absent_checksum_is_accepted` installs a *global* tracing dispatcher (the
/// persist it observes runs on a `spawn_blocking` thread, unreachable by a
/// thread-local one) — process-wide, so it would also capture
/// `rules_checksum_roundtrip`'s resume_info events if `cargo test` ran both
/// concurrently (its default). Serialize the two tests so the capture window
/// never overlaps with unrelated sessions. Async-aware `Mutex`: the guard is
/// held across `.await` points.
static TEST_SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn ev(lsn: u64) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: lsn.to_string(),
            payload: Bytes::from_static(b"x"),
        },
    )
}

/// Test double mirroring `transport.rs`'s own `MockOpLog` — the replay-call
/// counter is the observable proxy for "reconnect took the replay path"
/// (the snapshot branch is unobservable in this PG-free harness).
struct CountingOpLog {
    events: Vec<ReplicationEvent>,
    calls: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl OpLogSource for CountingOpLog {
    async fn replay_after(
        &self,
        _tenant_id: &str,
        _after_lsn: u64,
    ) -> Result<Vec<ReplicationEvent>, OpLogError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.events.clone())
    }

    async fn window_tail(&self) -> Result<u64, OpLogError> {
        Ok(0)
    }
}

async fn spawn_server(
    metrics: Arc<Metrics>,
    reader: Arc<dyn OpLogSource>,
    rules: Arc<tokio::sync::RwLock<ActiveRuleset>>,
) -> std::net::SocketAddr {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let auth: Arc<dyn SyncAuth> = Arc::new(AllowAnonymous::new());
    let checksum_now = rules.read().await.checksum();
    let (_tx, rules_changed) = tokio::sync::watch::channel(checksum_now);
    let state = SyncRouterState::new(manager, auth)
        .with_buffer(64)
        .with_metrics(metrics)
        .with_oplog_reader(reader)
        .with_rules(rules, rules_changed);
    let app = axum::Router::new()
        .route("/sync", axum::routing::get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);
    addr
}

fn short_idle_config() -> SyncClientConfig {
    SyncClientConfig {
        idle_timeout: Some(Duration::from_millis(500)),
        ..SyncClientConfig::default()
    }
}

/// End-to-end roundtrip across three reconnects on the SAME client/storage:
///
/// 1. A deliberately-*wrong* seed checksum (proves the persisted value after
///    this connect changed *because* resume_info was handled, not because
///    the seed happened to already be right) — mismatches the live ruleset,
///    so replay is declined, but the server still advertises its real
///    checksum and the client persists it.
/// 2. Reconnect with the now-correct persisted checksum against an unchanged
///    ruleset — gate matches, replay is attempted.
/// 3. The server's ruleset changes; the client's (still-current) checksum no
///    longer matches — gate mismatches, replay is declined again (snapshot
///    path), and the newly-advertised checksum is re-persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_checksum_roundtrip() {
    const WRONG_SEED: u64 = 0xDEAD_BEEF;
    let _serial = TEST_SERIAL.lock().await;

    let replay_calls = Arc::new(AtomicU64::new(0));
    let reader: Arc<dyn OpLogSource> = Arc::new(CountingOpLog {
        events: Vec::new(),
        calls: Arc::clone(&replay_calls),
    });
    let ruleset_v1 = ActiveRuleset::all_mode();
    let rules = Arc::new(tokio::sync::RwLock::new(ruleset_v1.clone()));
    let metrics = Arc::new(Metrics::new());
    metrics
        .slot_epoch
        .store(ADVERTISED_EPOCH, Ordering::Relaxed);

    let addr = spawn_server(Arc::clone(&metrics), reader, Arc::clone(&rules)).await;
    let url = format!("ws://{addr}/sync");

    let mut storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    // A resume_lsn > 0 is required for the replay branch to ever trigger —
    // apply an empty batch purely to advance the checkpoint.
    storage
        .apply_batch(&[], Lsn::new(5), &std::collections::HashSet::new())
        .expect("seed checkpoint");
    storage.save_epoch(ADVERTISED_EPOCH).expect("seed epoch");
    storage
        .save_rules_checksum(WRONG_SEED)
        .expect("seed wrong checksum");

    let client = SyncClient::new(url, storage, short_idle_config());

    // Connect 1: wrong seed checksum -> gate mismatch -> replay declined,
    // but the server's real checksum is still advertised and persisted.
    client.run_once().await.expect("run_once #1 completes");
    assert_eq!(
        replay_calls.load(Ordering::Relaxed),
        0,
        "wrong seed checksum must not match the live ruleset -> no replay"
    );
    let persisted = client
        .rules_checksum()
        .await
        .expect("checksum read after run 1");
    assert_eq!(
        persisted,
        ruleset_v1.checksum(),
        "resume_info's real checksum (not the wrong seed) must be persisted"
    );

    // Connect 2: resend the just-persisted, now-correct checksum against an
    // unchanged ruleset -> gate matches -> replay attempted.
    client.run_once().await.expect("run_once #2 completes");
    assert_eq!(
        replay_calls.load(Ordering::Relaxed),
        1,
        "correct persisted checksum against an unchanged ruleset must replay"
    );

    // Server-side ruleset changes.
    let rules_v2 = SyncRules {
        version: RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }],
        hand: vec![],
    };
    let ruleset_v2 = ActiveRuleset::compile(&rules_v2).expect("compile v2 rules");
    assert_ne!(
        ruleset_v1.checksum(),
        ruleset_v2.checksum(),
        "test needs two distinguishable rulesets"
    );
    *rules.write().await = ruleset_v2.clone();

    // Connect 3: same persisted (now-stale) checksum against the NEW ruleset
    // -> mismatch -> replay declined again (snapshot path).
    client.run_once().await.expect("run_once #3 completes");
    assert_eq!(
        replay_calls.load(Ordering::Relaxed),
        1,
        "rules change -> checksum mismatch -> replay declined (snapshot path)"
    );
    let final_checksum = client
        .rules_checksum()
        .await
        .expect("checksum read after run 3");
    assert_eq!(
        final_checksum,
        ruleset_v2.checksum(),
        "resume_info always re-syncs the checksum, even after a mismatch"
    );
}

/// A `Subscriber` that counts DEBUG-level events carrying both the
/// `server_epoch` and `rules_checksum` fields — the exact shape of
/// `client.rs`'s "resume_info received" log line, and no other. Avoids a new
/// `tracing-subscriber` dev-dependency: `tracing` (already a direct
/// dependency) exposes the `Subscriber` trait directly.
#[derive(Clone)]
struct ResumeInfoLogCounter {
    hits: Arc<AtomicU64>,
}

impl tracing::Subscriber for ResumeInfoLogCounter {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let fields = event.metadata().fields();
        if *event.metadata().level() == tracing::Level::DEBUG
            && fields.field("server_epoch").is_some()
            && fields.field("rules_checksum").is_some()
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// A `resume_info` with no `rules_checksum` key (the composed-fallback path,
/// e.g. an old server or a client that has never adopted an explicit
/// checksum) must:
/// - leave the stored checksum untouched at its default (`0`), never write
///   `None` over a value;
/// - be handled by exactly one log line per session, not once per frame —
///   verified here across a session carrying resume_info *and* several
///   subsequent replayed event frames.
///
/// The persist happens inside `tokio::task::spawn_blocking` (a dedicated
/// blocking-pool thread, not the test's own thread), so a thread-local
/// dispatcher (`tracing::subscriber::set_default`) would never see it. This
/// test is the only caller of `set_global_default` in this binary, so
/// installing the global dispatcher is safe (no other test's counts are
/// asserted on).
#[tokio::test]
async fn absent_checksum_is_accepted() {
    let _serial = TEST_SERIAL.lock().await;
    let replay_calls = Arc::new(AtomicU64::new(0));
    let reader: Arc<dyn OpLogSource> = Arc::new(CountingOpLog {
        events: vec![ev(10), ev(11), ev(12)],
        calls: Arc::clone(&replay_calls),
    });
    let ruleset_v1 = ActiveRuleset::all_mode();
    let rules = Arc::new(tokio::sync::RwLock::new(ruleset_v1.clone()));
    let metrics = Arc::new(Metrics::new());
    metrics
        .slot_epoch
        .store(ADVERTISED_EPOCH, Ordering::Relaxed);

    let addr = spawn_server(Arc::clone(&metrics), reader, Arc::clone(&rules)).await;
    let url = format!("ws://{addr}/sync");

    let mut storage = SqliteStorage::open_in_memory().expect("open in-memory sqlite");
    storage
        .apply_batch(&[], Lsn::new(5), &std::collections::HashSet::new())
        .expect("seed checkpoint");
    // Composed-mode epoch: the client has NEVER persisted a checksum (stays
    // at the trait default 0), so the Subscribe omits rules_checksum and the
    // server falls back to the composed (epoch, checksum) value — seed the
    // exact value the gate expects so the replay branch is reachable.
    let composed_epoch = compose_sync_epoch(ADVERTISED_EPOCH, ruleset_v1.checksum());
    storage
        .save_epoch(composed_epoch)
        .expect("seed composed epoch");

    let hits = Arc::new(AtomicU64::new(0));
    let subscriber = ResumeInfoLogCounter {
        hits: Arc::clone(&hits),
    };
    // Global, not thread-local: the persist runs on a spawn_blocking thread.
    tracing::dispatcher::set_global_default(tracing::Dispatch::new(subscriber))
        .expect("this is the only test in this binary that installs a dispatcher");

    let client = SyncClient::new(url, storage, short_idle_config());
    client.run_once().await.expect("run_once completes");

    assert_eq!(
        replay_calls.load(Ordering::Relaxed),
        1,
        "unchanged composed epoch must still gate-match and attempt replay"
    );
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "resume_info log line must fire exactly once per session, not per frame \
         (this session also carried 3 replayed event frames)"
    );
    assert_eq!(
        client
            .rules_checksum()
            .await
            .expect("checksum read after run"),
        0,
        "absent rules_checksum in resume_info must leave the stored value untouched"
    );
    assert_eq!(
        client.epoch().await.expect("epoch read after run"),
        composed_epoch,
        "composed epoch is re-advertised unchanged when the ruleset is unchanged"
    );
}
