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
use nostos_application::{ActiveRuleset, FanOutService};
use nostos_domain::{
    ColumnValue, Lsn, Operation, Principal, ReplicationEvent, RowOp, SyncRules, TableRule,
    RULES_VERSION,
};

use nostos_application::ports::SyncAuth;
use common::{
    decode_payload_hex, spawn_fake_server, spawn_fake_server_with, spawn_fake_server_with_rules,
    spawn_fake_server_with_tables,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

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
            old_payload: None,
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
                if common::is_data_frame(&v) {
                    got.push(v);
                }
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

// ---------------------------------------------------------------------------
// Scenario 5+: the safe-SQL-subset `where_sql` predicate (ADR-0012 compiler).
//
// A client may attach a `where_sql` to its subscribe frame; the server compiles
// it (via `nostos_domain::parse_predicate_expr`) and ANDs it into the predicate
// BEFORE the server-enforced tenant clause, so the client expression can never
// widen scope past its own tenant.
// ---------------------------------------------------------------------------

/// `SyncAuth` test-double: token "A" → tenant "A"; token "B" → tenant "B".
/// Used by the where_sql-cannot-shed-tenant test.
struct TenantAuth;
#[async_trait]
impl SyncAuth for TenantAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        match token {
            "A" => Some(Principal::new("user-a", "A")),
            "B" => Some(Principal::new("user-b", "B")),
            _ => None,
        }
    }
}

/// Subscribe with a `where_sql` expression and collect delivered frames.
async fn subscribe_with_where_sql(
    addr: std::net::SocketAddr,
    table: &str,
    where_sql: &str,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    subscribe_with_where_sql_token(addr, table, where_sql, None, timeout).await
}

/// Like [`subscribe_with_where_sql`] but authenticates with a bearer token
/// (query-param form) — used by the tenant-enforcement test.
async fn subscribe_with_where_sql_token(
    addr: std::net::SocketAddr,
    table: &str,
    where_sql: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let url = match token {
        Some(t) => format!("ws://{addr}/sync?token={t}"),
        None => format!("ws://{addr}/sync"),
    };
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    let sub =
        format!("{{\"type\":\"subscribe\",\"table\":\"{table}\",\"where_sql\":\"{where_sql}\"}}");
    ws.send(Message::Text(sub)).await.unwrap();

    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if common::is_data_frame(&v) {
                    got.push(v);
                }
            }
        }
    }
    got
}

/// Subscribe (optionally with a `where_sql`), then collect until the socket
/// closes or the timeout elapses — returning the close reason if the server
/// closed the connection. `where_sql: None` is a plain subscribe, used by the
/// rules-rejection test (Task 10), which has nothing to do with `where_sql`
/// but needs the same "collect until close" shape as the invalid-where_sql
/// rejection test below.
async fn subscribe_with_where_sql_until_close(
    addr: std::net::SocketAddr,
    table: &str,
    where_sql: Option<&str>,
    timeout: Duration,
) -> Result<Vec<serde_json::Value>, String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    let where_sql_field = match where_sql {
        Some(sql) => format!(",\"where_sql\":\"{sql}\""),
        None => String::new(),
    };
    let sub = format!("{{\"type\":\"subscribe\",\"table\":\"{table}\"{where_sql_field}}}");
    ws.send(Message::Text(sub)).await.unwrap();

    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    got.push(v);
                }
            }
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                return Err(frame.reason.to_string());
            }
            // Any other end-of-stream (None, error, bare close) means the socket
            // closed without a readable reason — surface an empty string so the
            // caller's `.contains` assertion still sees a deterministic value.
            Ok(Some(Ok(Message::Close(None)) | Err(_)) | None) => {
                return Err(String::new());
            }
            _ => {} // timeout on a single recv — keep waiting
        }
    }
    Ok(got)
}

/// Extract `priority` (as text, coerced by the typed `>` leaf) from the small
/// JSON payload.
fn extract_priority(e: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    if col != "priority" {
        return None;
    }
    extract_json_field(e, "priority")
}

/// Extract a string field from the event's JSON-ish payload, returning it as a
/// `ColumnValue::Text` (the matcher coerces to the filter's type as needed).
fn extract_json_field(e: &ReplicationEvent, field: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(e.payload_bytes()).ok()?;
    let needle = format!("\"{field}\":\"");
    let start = s.find(&needle)? + needle.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(ColumnValue::text(&rest[..end]))
}

#[tokio::test]
async fn subscribe_with_where_sql_filters_events() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // Subscribe with a where_sql: priority > 5.
    let collect = tokio::spawn(subscribe_with_where_sql(
        addr,
        "tasks",
        "priority > 5",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Publish a row with priority=3 (should be filtered out).
    let svc = Arc::new(FanOutService::new(store.clone()));
    let low = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{\"priority\":\"3\"}"),
        },
    );
    svc.fan_out(&low, extract_priority).await;

    // Publish a row with priority=7 (should be delivered).
    let high = ReplicationEvent::new(
        Lsn::new(2),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "2".into(),
            payload: Bytes::from_static(b"{\"priority\":\"7\"}"),
        },
    );
    svc.fan_out(&high, extract_priority).await;

    let frames = collect.await.unwrap();
    assert!(
        frames.iter().all(|f| f["pk"] != "1"),
        "the priority=3 row must NOT be delivered (where_sql priority > 5), got: {frames:?}"
    );
    assert!(
        frames.iter().any(|f| f["pk"] == "2"),
        "the priority=7 row MUST be delivered (where_sql priority > 5), got: {frames:?}"
    );
}

#[tokio::test]
async fn subscribe_with_invalid_where_sql_is_rejected_before_events() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // "DROP TABLE tasks" is not in the safe-SQL subset (no DROP keyword) — the
    // compiler must reject it and the server must close the socket before any
    // event flows. The close reason must mention "invalid where_sql".
    let res = subscribe_with_where_sql_until_close(
        addr,
        "tasks",
        Some("DROP TABLE tasks"),
        Duration::from_secs(2),
    )
    .await;

    // Either an explicit close frame with a reason, or an empty reason (socket
    // closed without a readable frame). In both cases the reason — when present
    // — must contain the canonical substring.
    let reason = res.expect_err("socket should close on invalid where_sql");
    if !reason.is_empty() {
        assert!(
            reason.contains("invalid where_sql"),
            "close reason must mention 'invalid where_sql', got: {reason:?}"
        );
    }

    // Confirm no event could have been delivered anyway: fan out a row and
    // verify no session is registered to receive it. (The socket closed before
    // registration, so the predicate never entered the store.)
    let svc = Arc::new(FanOutService::new(store.clone()));
    let event = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{}"),
        },
    );
    let outcome = svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;
    assert_eq!(
        outcome.delivered, 0,
        "no event may be delivered — the invalid-subscribe session was never registered"
    );
}

#[tokio::test]
async fn where_sql_cannot_shed_tenant_enforcement() {
    // Auth as tenant A with tenant enforcement on (column "tenant_id").
    let auth: Arc<dyn SyncAuth> = Arc::new(TenantAuth);
    let (addr, _server, _mgr, store) = spawn_fake_server_with(64, auth, Some("tenant_id")).await;

    // The client (tenant A) tries to escape its scope via where_sql:
    //     tenant_id = 'B' OR priority > 0
    // The server compiles this, then ANDs the real tenant clause AFTER (outside)
    // the client expression — so the effective predicate is:
    //     (tenant_id = 'B' OR priority > 0) AND tenant_id = 'A'
    // A tenant-B row matching the OR's first arm must STILL be dropped.
    let collect = tokio::spawn(subscribe_with_where_sql_token(
        addr,
        "tasks",
        "tenant_id = 'B' OR priority > 0",
        Some("A"),
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish a tenant-B row with priority > 0 — it matches the OR arm but must
    // fail the AND'd tenant_id='A'.
    let svc = Arc::new(FanOutService::new(store.clone()));
    let escape_attempt = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{\"tenant_id\":\"B\",\"priority\":\"9\"}"),
        },
    );
    svc.fan_out(&escape_attempt, |e, col| match col {
        "tenant_id" => extract_json_field(e, "tenant_id"),
        "priority" => extract_json_field(e, "priority"),
        _ => None,
    })
    .await;

    let frames = collect.await.unwrap();
    assert!(
        frames.is_empty(),
        "a tenant-B row must NOT be delivered to tenant A even when where_sql matches \
         it — the server ANDs tenant_id='A' outside the client expression. got: {frames:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 10 (ADR-0031): rules-scope enforcement at subscribe time.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subscribe_to_unsynced_table_is_rejected_before_events() {
    // `toggles` mode with only "notes" listed (and synced) — "tasks" is
    // unlisted, so it's fail-closed DeniedTable, not "everything".
    let rules = SyncRules {
        version: RULES_VERSION,
        mode: nostos_domain::SyncMode::Toggles,
        tables: vec![TableRule {
            table: "notes".into(),
            sync: true,
            scope: None,
        }],
        hand: Vec::new(),

        streams: vec![],
    };
    let ruleset = ActiveRuleset::compile(&rules).unwrap();
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server, _mgr, store) = spawn_fake_server_with_rules(64, auth, None, ruleset).await;

    // The SDKs surface the close reason verbatim to app developers — assert
    // the exact rendered `SubscribeRejection::NotSynced` text.
    let res =
        subscribe_with_where_sql_until_close(addr, "tasks", None, Duration::from_secs(2)).await;
    let reason = res.expect_err("socket should close: 'tasks' is not synced by the rules");
    assert!(
        reason.contains("table `tasks` is not synced by the active rules (sync_mode=toggles)"),
        "close reason must match SubscribeRejection::NotSynced's rendered text, got: {reason:?}"
    );

    // No event could have been delivered anyway: the socket closed before
    // subscribe registration, so the predicate never entered the store.
    let svc = Arc::new(FanOutService::new(store.clone()));
    let event = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{}"),
        },
    );
    let outcome = svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;
    assert_eq!(
        outcome.delivered, 0,
        "no event may be delivered — the rejected subscribe was never registered"
    );
}

// ===========================================================================
// D2 write-back contract tests (no PG — fake mode).
//
// Four reject-path contract tests for the new `Write` client message +
// `WriteResult` server frame. These run with the `FakeReplicator` setup
// (`spawn_fake_server`), which injects the `NoWriteBack` stub — so the only
// legitimate *success* path (real PG round-trip) is covered by
// `e2e_pg_writeback.rs`. Here we prove the four reject paths hold before any
// real database is involved:
//
//   (a) `Write` to a non-allowlisted table → `WriteResult{ok:false,
//       error:"table not writable…"}`.
//   (b) `Write` before `Subscribe` → socket closed (same discipline as an
//       early ACK).
//   (c) malformed payload (non-object) → `ok:false`, error mentions
//       "invalid payload".
//   (d) in fake mode with an allowlisted table → `ok:false`,
//       "write-back requires pg replicator" (v1: the fake has no database).
// ===========================================================================

/// A well-formed upsert Write frame for the given table + payload JSON.
fn write_upsert_frame(table: &str, pk: &str, payload: &str) -> String {
    format!(
        "{{\"type\":\"write\",\"table\":\"{table}\",\"op\":\"upsert\",\"pk\":\"{pk}\",\
         \"payload\":{payload},\"client_write_id\":\"w1\"}}"
    )
}

/// (a) Write to a non-allowlisted table → ok:false, "table not writable".
#[tokio::test]
async fn write_to_non_allowlisted_table_is_rejected() {
    let (addr, _server, _mgr, _store) = spawn_fake_server(64).await;

    // Subscribe first (so we're past the handshake; the table-allow check is
    // the thing under test, not Write-before-Subscribe).
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    // Drain the subscribe ack window.
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;

    // "secret_table" is NOT in the allowlist (NoWriteBack's allowlist is empty
    // in fake mode, so every table is non-allowlisted).
    ws.send(Message::Text(write_upsert_frame(
        "secret_table",
        "1",
        r#"{"title":"x"}"#,
    )))
    .await
    .unwrap();

    // Collect the next frame — it must be a WriteResult with ok:false.
    let mut result: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                    result = Some(v);
                    break;
                }
            }
        }
    }
    let result = result.expect("expected a WriteResult frame");
    assert_eq!(
        result["ok"], false,
        "non-allowlisted write must report ok:false"
    );
    let err = result["error"].as_str().expect("error string present");
    assert!(
        err.contains("table not writable"),
        "error must mention 'table not writable', got: {err}"
    );
}

/// (b) Write before Subscribe → socket closed (same discipline as early ACK).
#[tokio::test]
async fn write_before_subscribe_closes_socket() {
    let (addr, _server, _mgr, _store) = spawn_fake_server(64).await;

    // Connect and immediately send a Write WITHOUT subscribing first.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(write_upsert_frame(
        "tasks",
        "1",
        r#"{"title":"x"}"#,
    )))
    .await
    .unwrap();

    // The server must close the socket. Drain until we see a Close / None / Err.
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(Message::Close(_)) | Err(_)) | None) => {
                closed = true;
                break;
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                // A stray frame — must NOT be a successful WriteResult. If the
                // server somehow acked an out-of-order write, that's a hole.
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    assert_ne!(
                        v.get("type").and_then(|t| t.as_str()),
                        Some("write_result"),
                        "server must not ack a Write that arrived before Subscribe"
                    );
                }
            }
            // Text/Ping/Pong frames or a single-recv timeout — keep waiting for
            // the close (or the deadline to expire).
            _ => {}
        }
    }
    assert!(
        closed,
        "socket should close when Write arrives before Subscribe"
    );
}

/// (c) Malformed payload (non-object JSON) → ok:false, "invalid payload".
#[tokio::test]
async fn write_with_non_object_payload_is_rejected() {
    // `tasks` must be allowlisted so the test reaches the payload check (not
    // the allowlist gate). The fake NoWriteBack stub then never runs: the
    // transport rejects the non-object payload first.
    let (addr, _server, _mgr, _store) = spawn_fake_server_with_tables(
        64,
        Arc::new(nostos_infra::AllowAnonymous::new()),
        None,
        vec!["tasks".to_string()],
    )
    .await;

    // Subscribe, then send a Write whose payload is a JSON array (not object).
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;

    // payload is an array — not a JSON object.
    let bad = "{\"type\":\"write\",\"table\":\"tasks\",\"op\":\"upsert\",\
               \"pk\":\"1\",\"payload\":[1,2,3],\"client_write_id\":\"w1\"}";
    ws.send(Message::Text(bad.to_string())).await.unwrap();

    let mut result: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                    result = Some(v);
                    break;
                }
            }
        }
    }
    let result = result.expect("expected a WriteResult frame");
    assert_eq!(
        result["ok"], false,
        "non-object payload must report ok:false"
    );
    let err = result["error"].as_str().expect("error string present");
    assert!(
        err.contains("invalid payload"),
        "error must mention 'invalid payload', got: {err}"
    );
}

/// (d) Fake mode (no PG) with an allowlisted table → ok:false,
/// "write-back requires pg replicator". The NoWriteBack stub always errors.
#[tokio::test]
async fn write_in_fake_mode_reports_pg_required() {
    // `tasks` is allowlisted (so the test reaches NoWriteBack, not the
    // allowlist gate); NoWriteBack then returns its fixed fake-mode error.
    let (addr, _server, _mgr, _store) = spawn_fake_server_with_tables(
        64,
        Arc::new(nostos_infra::AllowAnonymous::new()),
        None,
        vec!["tasks".to_string()],
    )
    .await;

    // Subscribe, THEN write → the NoWriteBack stub returns its fixed error.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;

    ws.send(Message::Text(write_upsert_frame(
        "tasks",
        "1",
        r#"{"title":"x"}"#,
    )))
    .await
    .unwrap();

    let mut found: Option<serde_json::Value> = None;
    let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("type").and_then(|t| t.as_str()) == Some("write_result") {
                    found = Some(v);
                    break;
                }
            }
        }
    }
    let found = found.expect("expected a WriteResult frame from NoWriteBack");
    assert_eq!(found["ok"], false, "fake-mode write must report ok:false");
    let err = found["error"].as_str().expect("error string present");
    assert!(
        err.contains("write-back requires pg replicator"),
        "fake-mode error must mention 'write-back requires pg replicator', got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 8 (C3 batched-writes): when the per-session write task has a
// backlog (≥2 frames queued before it drains), it coalesces them into one
// WebSocket message carrying a JSON array. A client must still receive EVERY
// frame — whether the server sent them as a batched array or as separate
// single-object messages (both are valid wire forms; the timing decides which).
//
// This test fires several events in rapid succession at one session and
// asserts the client decodes the full set. It does NOT assert *which* wire
// form was used (that's an implementation/timing detail) — only that no frame
// is lost across the single↔array boundary, which is the contract that matters.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batched_writes_deliver_every_frame() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // Collector that decodes BOTH wire forms (single object + JSON array) via
    // `decode_frames`, so it tolerates whichever form the server picks.
    let collect = tokio::spawn(async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
            .await
            .expect("ws connect");
        ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
            .await
            .unwrap();
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(Ok(Message::Binary(b)))) =
                tokio::time::timeout(Duration::from_millis(200), ws.next()).await
            {
                for f in nostos_infra::wire::decode_frames(&b) {
                    got.push(f);
                }
            }
        }
        got
    });

    // Give the session time to subscribe + register.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Fire 8 events in rapid succession. The bounded buffer (64) holds them
    // all; the writer task's `recv().await` + non-blocking `try_recv` drain
    // should coalesce some/all into array messages.
    let svc = Arc::new(FanOutService::new(store.clone()));
    for i in 1..=8_u64 {
        let event = ReplicationEvent::new(
            Lsn::new(i),
            RowOp::Insert {
                table: "tasks".into(),
                pk: i.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        );
        svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;
    }

    let frames = collect.await.unwrap();
    // Every one of the 8 frames must arrive, regardless of how they were
    // batched on the wire.
    let mut lsns: Vec<u64> = frames.iter().map(|f| f.lsn).collect();
    lsns.sort_unstable();
    assert_eq!(
        lsns,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "every fanned-out frame must reach the client across the batch boundary; got {lsns:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 9 (D1/ADR-0022 multi-table-per-socket): ONE WebSocket subscribes to
// TWO tables, then fan-out fires one event per table. The client must receive
// BOTH — proving a second `Subscribe` registers an additional session on the
// shared sink (the old path ignored it) and that table-indexed candidates_for
// routes each table's event onto the one socket. The synthetic-LSN snapshot
// collision fix is PG-only and is exercised end-to-end later, not here.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_table_one_socket_receives_both_tables() {
    let (addr, _server, _mgr, store) = spawn_fake_server(64).await;

    // One socket subscribes to two tables back-to-back.
    let collect = tokio::spawn(async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
            .await
            .expect("ws connect");
        ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
            .await
            .unwrap();
        ws.send(Message::Text(common::subscribe_frame("providers", &[])))
            .await
            .unwrap();
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + COLLECT_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(Ok(Message::Binary(b)))) =
                tokio::time::timeout(Duration::from_millis(200), ws.next()).await
            {
                for f in nostos_infra::wire::decode_frames(&b) {
                    got.push(f);
                }
            }
        }
        got
    });

    // Let BOTH subscribes register.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Fan out one event per table.
    let svc = Arc::new(FanOutService::new(store.clone()));
    for (table, lsn) in [("tasks", 1_u64), ("providers", 2_u64)] {
        let event = ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: table.into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        );
        svc.fan_out(&event, |_, _| Some(ColumnValue::Any)).await;
    }

    let frames = collect.await.unwrap();
    let tables: Vec<&str> = frames.iter().map(|f| f.table.as_str()).collect();
    assert!(
        tables.contains(&"tasks"),
        "the tasks event must arrive on the shared socket; got tables {tables:?}"
    );
    assert!(
        tables.contains(&"providers"),
        "the providers event must arrive — a second Subscribe must register an additional session, \
         not be ignored (D1/ADR-0022); got tables {tables:?}"
    );
}
