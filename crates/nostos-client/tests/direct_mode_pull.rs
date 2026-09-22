//! Direct mode end to end against a fake PostgREST — no Nostos server anywhere
//! in this test, which is the whole point of the mode.
//!
//! The fake implements `cairn.pull`'s contract in Rust: the horizon filter, the
//! **distinct-xid** page limit, and `order by xid, seq`. That is deliberate
//! duplication — it pins the contract the real SQL function must satisfy, and
//! the paging livelock it exists to prevent (a row-limited page plus an
//! inclusive `since` re-reads forever) is a whole-system property that neither
//! the SQL nor `nostos-core` can demonstrate alone.

use std::sync::{Arc, Mutex};

use axum::{extract::State, Json, Router};
use nostos_client::{PostgrestError, PostgrestSource, SqliteStorage};
use nostos_core::{ApplyEngine, Horizon, PullCursor, Storage};
use serde::Deserialize;

/// One row of `cairn.changes` as the fake holds it.
#[derive(Clone, Copy)]
struct Change {
    seq: u64,
    xid: u64,
    table: &'static str,
    pk: &'static str,
    op: &'static str,
}

#[derive(Clone)]
struct Fake {
    rows: Arc<Vec<Change>>,
    /// `pg_snapshot_xmin(pg_current_snapshot())` — rows at or above it are from
    /// transactions still in flight and must not be served.
    horizon: u64,
    /// How many times the endpoint was hit (the paging assertion).
    hits: Arc<Mutex<usize>>,
}

#[derive(Deserialize)]
struct PullArgs {
    since: String,
    max_txns: usize,
}

/// `cairn.pull`, reimplemented: horizon filter, whole-transaction page, ordered.
async fn pull(State(fake): State<Fake>, Json(args): Json<PullArgs>) -> Json<serde_json::Value> {
    *fake.hits.lock().unwrap() += 1;
    let since: u64 = args.since.parse().expect("since must be an xid8");
    let max_txns = args.max_txns.max(2); // greatest(max_txns, 2)

    let mut rows: Vec<Change> = fake
        .rows
        .iter()
        .copied()
        .filter(|r| r.xid >= since && r.xid < fake.horizon)
        .collect();
    rows.sort_by_key(|r| (r.xid, r.seq));

    // `limit` on distinct xid, not on rows — the contract that keeps a
    // transaction from being cut in half.
    let mut xids: Vec<u64> = rows.iter().map(|r| r.xid).collect();
    xids.dedup();
    xids.truncate(max_txns);

    let out: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| xids.contains(&r.xid))
        .map(|r| {
            serde_json::json!({
                "horizon": fake.horizon.to_string(),
                "seq": r.seq,
                "xid": r.xid.to_string(),
                "table_name": r.table,
                "pk": r.pk,
                "op": r.op,
                "row": if r.op == "delete" { serde_json::Value::Null }
                       else { serde_json::json!({ "id": r.pk }) },
            })
        })
        .collect();
    Json(serde_json::Value::Array(out))
}

async fn spawn_fake(rows: Vec<Change>, horizon: u64) -> (String, Arc<Mutex<usize>>) {
    let hits = Arc::new(Mutex::new(0));
    let fake = Fake {
        rows: Arc::new(rows),
        horizon,
        hits: Arc::clone(&hits),
    };
    let app = Router::new()
        .route("/rest/v1/rpc/pull", axum::routing::post(pull))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);
    (format!("http://{addr}"), hits)
}

fn change(seq: u64, xid: u64, table: &'static str, pk: &'static str, op: &'static str) -> Change {
    Change {
        seq,
        xid,
        table,
        pk,
        op,
    }
}

fn engine_and_cursor(
    max_txns: usize,
) -> (
    Arc<Mutex<ApplyEngine<SqliteStorage>>>,
    Arc<Mutex<PullCursor>>,
) {
    let storage = SqliteStorage::open_in_memory().unwrap();
    (
        Arc::new(Mutex::new(ApplyEngine::new(storage))),
        Arc::new(Mutex::new(PullCursor::resume(Horizon::fresh(), max_txns))),
    )
}

#[tokio::test]
async fn a_drain_pages_to_the_horizon_and_never_splits_a_transaction() {
    // Five transactions; xid 104 is still in flight (>= horizon) so its rows
    // must not appear, exactly like an uncommitted write in real Postgres.
    let rows = vec![
        change(1, 100, "orders", "o1", "insert"),
        change(2, 100, "order_lines", "l1", "insert"),
        change(3, 101, "orders", "o2", "insert"),
        change(4, 102, "orders", "o3", "insert"),
        change(5, 102, "order_lines", "l2", "insert"),
        change(6, 103, "orders", "o1", "delete"),
        change(7, 104, "orders", "o9", "insert"), // in flight — invisible
    ];
    let (base, hits) = spawn_fake(rows, 104).await;

    let src = PostgrestSource::new(&base, "anon-key").unwrap();
    // Two transactions per page forces the paging path.
    let (engine, cursor) = engine_and_cursor(2);

    let out = src.drain(&engine, &cursor).await.unwrap();

    assert!(!out.more, "the drain caught up inside the page cap");
    assert!(*hits.lock().unwrap() >= 3, "paging actually paged");
    assert_eq!(
        out.horizon,
        Some(Horizon::new("104")),
        "a short final page lands the cursor on the horizon"
    );

    let engine = engine.lock().unwrap();
    let storage = engine.storage();
    // o1 inserted then deleted; o2, o3 survive; o9 was never committed.
    let mut orders = storage.pks_for_table("orders").unwrap();
    orders.sort();
    assert_eq!(orders, vec!["o2".to_string(), "o3".to_string()]);
    let mut lines = storage.pks_for_table("order_lines").unwrap();
    lines.sort();
    assert_eq!(lines, vec!["l1".to_string(), "l2".to_string()]);

    // The durable resume point is the horizon, not the LSN.
    assert_eq!(storage.horizon().unwrap().as_deref(), Some("104"));
}

#[tokio::test]
async fn a_second_drain_with_nothing_new_is_a_no_op() {
    let rows = vec![change(1, 100, "orders", "o1", "insert")];
    let (base, _) = spawn_fake(rows, 101).await;
    let src = PostgrestSource::new(&base, "anon-key").unwrap();
    let (engine, cursor) = engine_and_cursor(200);

    let first = src.drain(&engine, &cursor).await.unwrap();
    assert_eq!(first.rows_applied, 1);

    // Idempotent: the cursor sits at the horizon, whose transaction never
    // committed anything, so the page comes back empty.
    let second = src.drain(&engine, &cursor).await.unwrap();
    assert_eq!(second.rows_applied, 0);
    assert_eq!(second.horizon, None, "the cursor did not move");
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .storage()
            .pks_for_table("orders")
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_rejected_jwt_surfaces_as_a_status_not_as_missing_rows() {
    // The failure mode that matters operationally: bad JWT / unexposed schema /
    // missing grants all come back as an HTTP status with a usable body, and
    // must never be mistaken for "no changes".
    let app = Router::new().route(
        "/rest/v1/rpc/pull",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                r#"{"message":"JWT expired"}"#,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);

    let mut src = PostgrestSource::new(&format!("http://{addr}"), "anon-key").unwrap();
    src.set_token("expired.jwt.here");
    let (engine, cursor) = engine_and_cursor(200);

    match src.drain(&engine, &cursor).await {
        Err(PostgrestError::Status { status, body }) => {
            assert_eq!(status, 401);
            assert!(body.contains("JWT expired"), "body is preserved: {body}");
        }
        other => panic!("expected a 401 Status, got {other:?}"),
    }
    // Nothing applied, and the cursor did not move — a retry re-reads.
    assert_eq!(cursor.lock().unwrap().since(), &Horizon::fresh());
}
