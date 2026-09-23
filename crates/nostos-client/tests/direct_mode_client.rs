//! The native direct-mode loop, against a fake PostgREST that serves all three
//! endpoints one device talks to: `cairn_pull`, a table (the write), and
//! `cairn_snapshot`.
//!
//! `direct_mode_pull.rs` pins the protocol pieces in isolation. What is only
//! visible here is the ORDER they run in — a queued write must leave the device
//! before the pull that returns its echo, and a pruned horizon must turn into a
//! snapshot inside the same `sync()` rather than surfacing as an error the
//! caller has to know how to handle.

use std::sync::{Arc, Mutex};

use axum::{extract::State, Json, Router};
use nostos_client::{DirectClient, SqliteStorage};
use nostos_core::{Outbox, PendingWrite, Storage, WriteOp};
use serde::Deserialize;

#[derive(Clone)]
struct Fake {
    /// Set once the device has pushed its write, so `cairn_pull` can answer
    /// with the echo exactly as the real trigger would.
    pushed: Arc<Mutex<Option<String>>>,
    /// Every pull before this many has been "pruned away" — the 410 path.
    gone: Arc<Mutex<bool>>,
    pulls: Arc<Mutex<usize>>,
    snapshots: Arc<Mutex<usize>>,
}

#[derive(Deserialize)]
struct PullArgs {
    #[allow(dead_code)]
    since: String,
    #[allow(dead_code)]
    max_txns: usize,
}

async fn pull(
    State(fake): State<Fake>,
    Json(_args): Json<PullArgs>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    *fake.pulls.lock().unwrap() += 1;
    if *fake.gone.lock().unwrap() {
        return Err((
            axum::http::StatusCode::GONE,
            r#"{"message":"nostos: the change log has been pruned past this horizon"}"#.into(),
        ));
    }
    let echo = fake.pushed.lock().unwrap().clone();
    let rows: Vec<serde_json::Value> = echo
        .into_iter()
        .map(|pk| {
            serde_json::json!({
                "horizon": "900",
                "seq": 1,
                "xid": "800",
                "table_name": "orders",
                "pk": pk,
                "op": "insert",
                "row": { "id": pk, "total": 9 },
            })
        })
        .collect();
    Ok(Json(serde_json::Value::Array(rows)))
}

async fn snapshot(State(fake): State<Fake>) -> Json<serde_json::Value> {
    *fake.snapshots.lock().unwrap() += 1;
    *fake.gone.lock().unwrap() = false;
    Json(serde_json::json!([
        { "horizon": "950", "table_name": null, "pk": null, "row": null },
        { "horizon": "950", "table_name": "orders", "pk": null, "row": null },
        { "horizon": "950", "table_name": "orders", "pk": "srv-1",
          "row": { "id": "srv-1", "total": 42 } },
    ]))
}

async fn write(State(fake): State<Fake>, body: String) -> axum::http::StatusCode {
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let pk = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    *fake.pushed.lock().unwrap() = Some(pk);
    axum::http::StatusCode::NO_CONTENT
}

async fn spawn(gone: bool) -> (String, Fake) {
    let fake = Fake {
        pushed: Arc::new(Mutex::new(None)),
        gone: Arc::new(Mutex::new(gone)),
        pulls: Arc::new(Mutex::new(0)),
        snapshots: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/rest/v1/rpc/cairn_pull", axum::routing::post(pull))
        .route("/rest/v1/rpc/cairn_snapshot", axum::routing::post(snapshot))
        .route("/rest/v1/orders", axum::routing::post(write))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);
    (format!("http://{addr}"), fake)
}

/// A device that has never synced: no stored horizon, empty storage.
fn client(base: &str) -> DirectClient<SqliteStorage> {
    DirectClient::new(base, "anon-key", SqliteStorage::open_in_memory().unwrap()).unwrap()
}

/// A device that has synced before, so its first `sync()` resumes the log from
/// the horizon it stored rather than bootstrapping from a snapshot.
fn resuming_client(base: &str) -> DirectClient<SqliteStorage> {
    let mut storage = SqliteStorage::open_in_memory().unwrap();
    storage.save_horizon("700").unwrap();
    DirectClient::new(base, "anon-key", storage).unwrap()
}

/// The change log is not a history of the database: it begins where the
/// trigger was installed, so every row older than that is reachable only
/// through `cairn_snapshot`. A first sync that pulls from `Horizon::fresh`
/// instead of snapshotting therefore shows an empty app forever, not just
/// until the next write.
///
/// Caught against a live Supabase project 2026-09-22: 1000 products and 4
/// sessions in the tables, 4 rows in `cairn.changes`, and a device that
/// rendered "No sessions yet" / "No products yet".
#[tokio::test]
async fn a_fresh_device_bootstraps_from_the_snapshot_not_the_log() {
    let (base, fake) = spawn(false).await;
    let client = client(&base);

    let out = client.sync().await.unwrap();

    assert_eq!(
        *fake.snapshots.lock().unwrap(),
        1,
        "a device with no horizon takes its first picture from the snapshot"
    );
    assert_eq!(out.rows_applied, 1);
    assert!(out.resnapshotted);

    let engine = client.engine().lock().unwrap();
    assert_eq!(
        engine.storage().pks_for_table("orders").unwrap(),
        ["srv-1"],
        "the row that predates the change log arrived anyway"
    );
    assert_eq!(
        engine.storage().horizon().unwrap().as_deref(),
        Some("950"),
        "and the device resumes the log from the snapshot's horizon"
    );
}

/// The bootstrap is once per device, not once per launch: a second sync
/// resumes the log. Without this the app would re-read every synced table on
/// every ring.
#[tokio::test]
async fn the_bootstrap_snapshot_happens_once() {
    let (base, fake) = spawn(false).await;
    let client = client(&base);

    client.sync().await.unwrap();
    let second = client.sync().await.unwrap();

    assert_eq!(*fake.snapshots.lock().unwrap(), 1);
    assert!(!second.resnapshotted, "the second sync pulled");
}

#[tokio::test]
async fn one_sync_pushes_the_queue_then_applies_the_echo_it_caused() {
    let (base, fake) = spawn(false).await;
    let client = resuming_client(&base);

    client
        .engine()
        .lock()
        .unwrap()
        .storage_mut()
        .enqueue(PendingWrite {
            table: "orders".into(),
            op: WriteOp::Upsert,
            pk: "o1".into(),
            payload_json: Some(r#"{"id":"o1","total":9}"#.into()),
        })
        .unwrap();

    let out = client.sync().await.unwrap();

    assert_eq!(out.pushed, 1, "the queued write left the device");
    // The echo is in the same sync, which only happens if the push ran first.
    assert_eq!(out.rows_applied, 1, "the server's version came back");
    assert!(!out.resnapshotted);
    assert_eq!(*fake.pushed.lock().unwrap(), Some("o1".to_string()));

    let engine = client.engine().lock().unwrap();
    assert_eq!(engine.storage().pks_for_table("orders").unwrap(), ["o1"]);
    assert!(
        engine.storage().pending().unwrap().is_empty(),
        "an accepted write leaves the outbox"
    );
    assert_eq!(
        engine.storage().horizon().unwrap().as_deref(),
        Some("900"),
        "the horizon is durable before the sync returns"
    );
}

#[tokio::test]
async fn a_pruned_horizon_re_snapshots_inside_the_same_sync() {
    let (base, fake) = spawn(true).await;
    let client = resuming_client(&base);

    let out = client.sync().await.unwrap();

    assert!(
        out.resnapshotted,
        "the 410 turned into a snapshot, not an error"
    );
    assert_eq!(*fake.snapshots.lock().unwrap(), 1);
    assert_eq!(out.rows_applied, 1);

    let engine = client.engine().lock().unwrap();
    assert_eq!(engine.storage().pks_for_table("orders").unwrap(), ["srv-1"]);
    assert_eq!(
        engine.storage().horizon().unwrap().as_deref(),
        Some("950"),
        "the resumed horizon is the snapshot's, not the pruned one"
    );
}

#[tokio::test]
async fn an_unsendable_write_dead_letters_instead_of_blocking_the_queue() {
    let (base, _fake) = spawn(false).await;
    let client = resuming_client(&base);

    client
        .engine()
        .lock()
        .unwrap()
        .storage_mut()
        .enqueue(PendingWrite {
            // An upsert with no payload has nothing to send and never will.
            table: "orders".into(),
            op: WriteOp::Upsert,
            pk: "bad".into(),
            payload_json: None,
        })
        .unwrap();

    let out = client.sync().await.unwrap();

    assert_eq!(out.pushed, 0);
    assert!(
        client
            .engine()
            .lock()
            .unwrap()
            .storage()
            .pending()
            .unwrap()
            .is_empty(),
        "the queue is not blocked behind a write that can never succeed"
    );
}

/// Local-first is the whole promise: the row must render before the network
/// agrees, not after. Queueing alone does not do that — the outbox is not
/// what `watch()` reads — so a write that only enqueues shows the user
/// nothing until the server echoes it back, and shows nothing *at all* if the
/// server refuses.
///
/// Caught against a live Supabase project 2026-09-23: four `cart_items`
/// upserts sat in the outbox dead-lettered on an RLS 403, and the cart was
/// empty on screen the whole time — the app never rendered its own writes.
#[tokio::test]
async fn a_queued_write_renders_before_the_server_hears_about_it() {
    let (base, _fake) = spawn(false).await;
    let client = resuming_client(&base);

    let ids = client
        .write_batch(&[PendingWrite {
            table: "orders".into(),
            op: WriteOp::Upsert,
            pk: "o1".into(),
            payload_json: Some(r#"{"id":"o1","total":9}"#.into()),
        }])
        .unwrap();

    assert_eq!(ids.len(), 1, "the write is queued for the server");
    let engine = client.engine().lock().unwrap();
    assert_eq!(
        engine.storage().pks_for_table("orders").unwrap(),
        ["o1"],
        "and it is on screen already — no sync has run"
    );
    assert_eq!(
        engine.storage().pending().unwrap().len(),
        1,
        "the optimistic apply does not consume the queue entry"
    );
}

/// The optimistic row is not a commit: it must not advance the checkpoint, or
/// the device would claim to have server-confirmed data it only wrote itself.
#[tokio::test]
async fn the_optimistic_row_does_not_advance_the_checkpoint() {
    let (base, _fake) = spawn(false).await;
    let client = resuming_client(&base);
    let before = client.engine().lock().unwrap().checkpoint().unwrap();

    client
        .write_batch(&[PendingWrite {
            table: "orders".into(),
            op: WriteOp::Upsert,
            pk: "o1".into(),
            payload_json: Some(r#"{"id":"o1","total":9}"#.into()),
        }])
        .unwrap();

    assert_eq!(
        client.engine().lock().unwrap().checkpoint().unwrap(),
        before,
        "only a server echo moves the checkpoint"
    );
}
