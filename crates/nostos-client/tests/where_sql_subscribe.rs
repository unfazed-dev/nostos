//! `where_sql` subscribe — Tier-7 compiler end-to-end at the client boundary.
//!
//! C1 wired `where_sql` into the wire `ClientMessage::Subscribe` and the
//! transport's `build_predicate` (the server compiles + ANDs in the expression).
//! This test proves the *native client* threads a `where_sql` from
//! [`SyncClientConfig`] into the subscribe frame AND that the server-side
//! compiler + matcher actually filters rows against it.
//!
//! The shape: a `tasks` row stream where half the rows have `priority <= 5`
//! (filtered out) and half have `priority > 5` (delivered). The in-process
//! server's FanOutService uses a **real JSON-parsing extractor** (the production
//! `extract_json_column` shape) so the predicate engine can evaluate
//! `priority > 5` against each row's actual value — not the table-only
//! `ColumnValue::Any` wildcard the chaos e2e uses.
//!
//! The kill assertion: only the high-priority rows land in the client's SQLite.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};

use nostos_application::ports::{ReplicatorStream, SessionStore};
use nostos_application::FanOutService;
use nostos_domain::{ColumnValue, Lsn, ReplicationEvent, RowOp};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::AllowAnonymous;

use common::spawn_server_with_existing_store;

/// Build a `tasks` insert whose payload is the JSON object
/// `{"priority":"<n>"}` — the same shape `PgReplicator::tuple_to_json_payload`
/// emits (every value quoted). The where_sql predicate compiles `priority > 5`
/// into a typed `Gt(Number(5))` leaf that coerces the text row value at match
/// time (ADR-0012 slice 2).
fn tasks_event(lsn: u64, priority: i64) -> ReplicationEvent {
    let payload = format!("{{\"priority\":\"{priority}\"}}");
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: format!("task-{priority}"),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        },
    )
}

/// The production-shaped extractor: parse the JSON payload once, lift the
/// requested column as `ColumnValue::text`. The predicate engine coerces text →
/// number for the typed `Gt` leaf. Returning real values (not `Any`) is what
/// makes `priority > 5` actually filter — `Any` would match-equality but never
/// order-compare.
fn extract_json(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    // Tiny flat-object parse: {"priority":"7"} → priority → "7".
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    for pair in s.split(',') {
        let mut kv = pair.splitn(2, ':');
        let k = kv.next()?.trim().trim_matches('"');
        let v = kv.next()?.trim().trim_matches('"');
        if k == col {
            return Some(ColumnValue::text(v));
        }
    }
    None
}

/// A replicator that yields a scripted stream: half low-priority (≤5, filtered
/// by `priority > 5`), half high-priority (>5, delivered).
struct PriorityStream {
    emitted: u64,
}

#[async_trait::async_trait]
impl ReplicatorStream for PriorityStream {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        // Emit a fixed set: priorities 3, 7, 1, 9, 5, 12. Only 7, 9, 12 satisfy
        // `priority > 5` (three rows); 3, 1, 5 are filtered out.
        const PRIORITIES: &[i64] = &[3, 7, 1, 9, 5, 12];
        let i = usize::try_from(self.emitted).ok()?;
        self.emitted += 1;
        let priority = *PRIORITIES.get(i)?;
        // Monotonic LSNs (the resume seed skips already-applied frames).
        Some(tasks_event(10 + i as u64 * 10, priority))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn where_sql_filters_rows_at_the_server() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let (addr, _server) =
        spawn_server_with_existing_store(Arc::clone(&store), Arc::new(AllowAnonymous::new()), 1024)
            .await;
    let url = format!("ws://{addr}/sync");
    let dir = common::tempfile_dir();
    let db_path = format!("{dir}/where_sql.sqlite");

    // Subscribe with where_sql: priority > 5 — the Tier-7 compiler compiles
    // this into a typed Gt(Number(5)) leaf ANDed into the session predicate.
    let config = SyncClientConfig {
        table: "tasks".into(),
        where_sql: Some("priority > 5".into()),
        idle_timeout: Some(Duration::from_millis(500)),
        ..SyncClientConfig::default()
    };
    let client = SyncClient::new(url, SqliteStorage::open(&db_path).unwrap(), config);
    let task = tokio::spawn(async move { client.run_once().await });
    // Let the subscribe land before the pump (FanOut delivers only to sessions
    // registered at fan-out time).
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Drive the scripted stream through the real FanOutService with the
    // JSON-parsing extractor — the predicate is evaluated here, against real
    // decoded priority values.
    let mut stream = PriorityStream { emitted: 0 };
    FanOutService::new(Arc::clone(&store))
        .run(&mut stream, extract_json)
        .await;

    let outcome = task.await.unwrap().expect("client run_once");
    // THE assertion: only the three high-priority rows (7, 9, 12) survived the
    // server-side predicate. The three low-priority rows (3, 1, 5) were filtered
    // out before they ever reached the wire — they don't land in SQLite.
    let storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        storage.row_count_for_test(),
        3,
        "where_sql priority > 5 should deliver only the 3 high-priority rows; \
         received {} frames, {} rows",
        outcome.frames_received,
        storage.row_count_for_test()
    );
    assert!(
        outcome.frames_received >= 3,
        "received {}",
        outcome.frames_received
    );
}
