//! Tier 3 WS contract smoke tests — real axum WebSocket, no PG.
//!
//! These spin up the production axum sync server on an ephemeral loopback port
//! with a `FakeReplicator` (or direct `fan_out`) feeding the **shared** store
//! the live WS transport reads from. They cover the boundaries the isolated
//! unit tests in `wire.rs` can't:
//!
//! - **wire frame contract**: a received binary frame deserializes to the exact
//!   `WireFrame` JSON shape (`lsn`, `op`, `table`, `pk`, optional `payload`
//!   hex, optional `txn_id`); deletes omit `payload`;
//! - **selective delivery over real WS**: three clients with distinct
//!   predicates; only the matching one receives an event;
//! - **subscribe-then-receive ordering**: the client must subscribe *before*
//!   events flow (the registration window the benchmark relies on).
//!
//! Runs on every push. No PG.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nostos_application::FanOutService;
use nostos_domain::{ColumnValue, Lsn, Operation, ReplicationEvent, RowOp};

use common::{decode_payload_hex, spawn_fake_server};

const COLLECT_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Scenario 1: the wire frame shape is exactly the documented contract.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wire_frame_contract_is_exact() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // Subscribe a client FIRST so the session is registered before fan-out.
    let collect = tokio::spawn(common::subscribe_and_collect(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Fan out one insert with a known payload + a txn id.
    let svc = Arc::new(FanOutService::new(store.clone()));
    let event = ReplicationEvent::new(
        Lsn::new(42),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "7".into(),
            payload: Bytes::from_static(b"hi"),
        },
    )
    .with_txn(99);
    svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;

    let frames = collect.await.unwrap();
    assert!(!frames.is_empty(), "expected at least one frame");
    let frame = &frames[0];

    // Every field of the WireFrame contract.
    assert_eq!(frame["lsn"], 42, "lsn field");
    assert_eq!(frame["op"], "insert", "op field is lowercase verb");
    assert_eq!(frame["table"], "tasks", "table field");
    assert_eq!(frame["pk"], "7", "pk field");
    assert_eq!(frame["txn_id"], 99, "txn_id field");
    // payload is lowercase hex of b"hi" == "6869".
    let payload = frame["payload"]
        .as_str()
        .expect("payload present for insert");
    assert_eq!(payload, "6869", "payload is lowercase hex");
    assert_eq!(
        decode_payload_hex(payload),
        b"hi",
        "hex decodes to the original bytes"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: deletes omit the payload field entirely (skip_serializing_if).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_frame_omits_payload() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;
    let collect = tokio::spawn(common::subscribe_and_collect(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));
    let event = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Delete {
            table: "tasks".into(),
            pk: "9".into(),
        },
    );
    svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;

    let frames = collect.await.unwrap();
    let del = frames
        .iter()
        .find(|f| f["op"] == "delete")
        .expect("a delete frame was delivered");
    assert_eq!(del["op"], "delete");
    assert_eq!(del["pk"], "9");
    // payload must be absent OR null — never a string.
    assert!(
        del.get("payload").is_none() || del["payload"].is_null(),
        "delete must omit payload, got: {del}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: selective delivery over a real WebSocket.
// Three clients on `tasks`: one wants org_id=acme, one org_id=other, one
// match-all. We fan out an event matching ONLY org_id=acme. Exactly the acme
// client (and the match-all client) should receive it; `other` should not.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn selective_delivery_routes_over_real_ws() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // Connect the match-all client first (proves subscribe-before-receive).
    let all_frames = tokio::spawn(common::subscribe_and_collect(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));

    // Connect the acme client with an org_id filter.
    let acme_addr = addr;
    let acme_frames = tokio::spawn(async move {
        subscribe_with_filter(acme_addr, "tasks", "org_id", "acme", COLLECT_TIMEOUT).await
    });

    // Connect the other client with a different filter.
    let other_addr = addr;
    let other_frames = tokio::spawn(async move {
        subscribe_with_filter(other_addr, "tasks", "org_id", "other", COLLECT_TIMEOUT).await
    });

    // Give all three time to subscribe.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Fan out an event carrying org_id=acme in a JSON-ish payload.
    let svc = Arc::new(FanOutService::new(store.clone()));
    let event = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{\"org_id\":\"acme\"}"),
        },
    );
    svc.fan_out(&event, extract_org_id).await;

    let all = all_frames.await.unwrap();
    let acme = acme_frames.await.unwrap();
    let other = other_frames.await.unwrap();

    // match-all + acme clients receive; `other` does not.
    assert!(!all.is_empty(), "match-all client should receive the event");
    assert!(
        !acme.is_empty(),
        "acme client should receive its matching event"
    );
    assert!(
        other.is_empty(),
        "other client must NOT receive an event it didn't subscribe to (got {} frames)",
        other.len()
    );
}

/// Connect, subscribe with a single column=value filter, and collect frames.
async fn subscribe_with_filter(
    addr: std::net::SocketAddr,
    table: &str,
    column: &str,
    value: &str,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    let sub = common::subscribe_frame(table, &[(column, value)]);
    ws.send(Message::Text(sub)).await.unwrap();

    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                got.push(v);
            }
        }
    }
    got
}

/// Extractor that reads `org_id` out of the small JSON payload.
fn extract_org_id(e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    if col != "org_id" {
        return None;
    }
    let s = std::str::from_utf8(e.payload_bytes()).ok()?;
    let needle = "\"org_id\":\"";
    let start = s.find(needle)? + needle.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(ColumnValue::text(&rest[..end]))
}

// ---------------------------------------------------------------------------
// Scenario 4: the operation enum round-trips as the lowercase wire verb.
// (Guards against a future change to Operation's serde rename_all breaking
// the wire contract that clients depend on.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operation_serializes_as_lowercase_verb() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;
    let collect = tokio::spawn(common::subscribe_and_collect(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));
    // An update — distinct verb from insert/delete.
    let event = ReplicationEvent::new(
        Lsn::new(5),
        RowOp::Update {
            table: "tasks".into(),
            pk: "u".into(),
            payload: Bytes::from_static(b"p"),
        },
    );
    svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;

    let frames = collect.await.unwrap();
    let upd = frames
        .iter()
        .find(|f| f["op"] == "update")
        .expect("an update frame was delivered");
    assert_eq!(upd["op"], "update");
    assert_eq!(upd["pk"], "u");
    // Confirm the domain enum still maps the same way (belt + braces).
    assert_eq!(
        serde_json::to_string(&Operation::Update).unwrap(),
        "\"update\""
    );
}
