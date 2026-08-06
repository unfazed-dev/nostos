//! Live sync-rules reload — ADR-0031 D3, Task 14.
//!
//! `spawn_fake_server_with_live_rules` (in `tests/common`) hands back the raw
//! `watch::Sender<u64>` + `Arc<RwLock<ActiveRuleset>>` pair that production's
//! `watch_rules` (`crates/nostos-server/src/main.rs`) owns, so a test can
//! simulate a reload exactly the way the real poller does: swap the
//! `RwLock`, then notify on the channel. That poller itself lives in the
//! `nostos-server` binary crate and isn't reachable from these
//! `nostos-infra` integration tests — see `malformed_reload_keeps_previous_ruleset`
//! for how that boundary is covered instead.
//!
//! D3's contract: a live session whose subscribed-table decisions are
//! unaffected (or, coarsely, "changed in a way this code can verify is a
//! widen") keeps running with the new ruleset swapped in; a session whose
//! scope narrows closes so the client reconnects and re-scopes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nostos_application::ports::SyncAuth;
use nostos_application::{ActiveRuleset, FanOutService};
use nostos_domain::{
    ColumnValue, Lsn, ReplicationEvent, RowOp, SyncRules, TableRule, RULES_VERSION,
};

use common::spawn_fake_server_with_live_rules;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const COLLECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Subscribe (plain, no `where_sql`), then collect data frames until the
/// socket closes or `timeout` elapses — returning the close reason if the
/// server closed it. Same shape as `ws_contract.rs`'s
/// `subscribe_with_where_sql_until_close`; duplicated locally rather than
/// exported cross-test-binary (integration test binaries don't share
/// anything but `tests/common`).
async fn subscribe_until_close_or_timeout(
    addr: std::net::SocketAddr,
    table: &str,
    timeout: Duration,
) -> Result<Vec<serde_json::Value>, String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    let sub = format!("{{\"type\":\"subscribe\",\"table\":\"{table}\"}}");
    ws.send(Message::Text(sub)).await.unwrap();

    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                    if common::is_data_frame(&v) {
                        got.push(v);
                    }
                }
            }
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                return Err(frame.reason.to_string());
            }
            Ok(Some(Ok(Message::Close(None)) | Err(_)) | None) => {
                return Err(String::new());
            }
            _ => {} // per-recv timeout — keep waiting
        }
    }
    Ok(got)
}

fn toggles_rules(tables: Vec<TableRule>) -> SyncRules {
    SyncRules {
        version: RULES_VERSION,
        mode: nostos_domain::SyncMode::Toggles,
        tables,
        hand: Vec::new(),
    }
}

fn insert_event(lsn: u64, table: &str, pk: &str) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: table.into(),
            pk: pk.into(),
            payload: Bytes::from_static(b"{}"),
        },
    )
}

// ---------------------------------------------------------------------------
// A live session's subscribed table narrows (toggled off) → close + reconnect.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn narrowing_change_closes_live_session() {
    let initial = toggles_rules(vec![TableRule {
        table: "tasks".into(),
        sync: true,
        scope: None,
    }]);
    let ruleset = ActiveRuleset::compile(&initial).unwrap();
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server, _mgr, _store, rules_tx, rules_shared) =
        spawn_fake_server_with_live_rules(64, auth, None, ruleset).await;

    let collect = tokio::spawn(subscribe_until_close_or_timeout(
        addr,
        "tasks",
        Duration::from_secs(3),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Narrow: "tasks" toggled off. Allow(_) -> DeniedTable is a per-table
    // RuleDecision inequality, so the live session must close.
    let narrowed = toggles_rules(vec![TableRule {
        table: "tasks".into(),
        sync: false,
        scope: None,
    }]);
    let compiled = ActiveRuleset::compile(&narrowed).unwrap();
    let checksum = compiled.checksum();
    *rules_shared.write().await = compiled;
    rules_tx.send(checksum).unwrap();

    let res = collect.await.unwrap();
    let reason = res.expect_err("a narrowing reload must close the live session");
    assert_eq!(
        reason, "rules changed; reconnect to re-scope",
        "close reason must be transport.rs's RULES_CHANGED_CLOSE_REASON, got: {reason:?}"
    );
}

// ---------------------------------------------------------------------------
// An edit that doesn't touch this socket's subscribed table's decision keeps
// the session open, running under the newly swapped-in ruleset.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn widening_or_unaffected_change_keeps_session_open_with_new_predicate() {
    let initial = toggles_rules(vec![
        TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        },
        TableRule {
            table: "notes".into(),
            sync: true,
            scope: None,
        },
    ]);
    let ruleset = ActiveRuleset::compile(&initial).unwrap();
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server, _mgr, store, rules_tx, rules_shared) =
        spawn_fake_server_with_live_rules(64, auth, None, ruleset).await;

    let collect = tokio::spawn(subscribe_until_close_or_timeout(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Drop "notes" entirely — an edit this socket never subscribed to.
    // "tasks" decide() output is byte-for-byte the same before and after.
    let edited = toggles_rules(vec![TableRule {
        table: "tasks".into(),
        sync: true,
        scope: None,
    }]);
    let compiled = ActiveRuleset::compile(&edited).unwrap();
    let checksum = compiled.checksum();
    *rules_shared.write().await = compiled;
    rules_tx.send(checksum).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));
    svc.fan_out(&insert_event(5, "tasks", "1"), |_, _| {
        Some(ColumnValue::Any)
    })
    .await;

    let res = collect.await.unwrap();
    let frames = res.expect("session must stay open across an unrelated rules edit");
    assert!(
        frames.iter().any(|f| f["pk"] == "1"),
        "session must keep receiving events under the swapped-in ruleset, got: {frames:?}"
    );
}

// ---------------------------------------------------------------------------
// A reload that recompiles to an identical ruleset (e.g. a whitespace-only
// edit upstream) must never close a live session.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identical_reload_does_not_close() {
    let rules = toggles_rules(vec![TableRule {
        table: "tasks".into(),
        sync: true,
        scope: None,
    }]);
    let ruleset = ActiveRuleset::compile(&rules).unwrap();
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server, _mgr, store, rules_tx, rules_shared) =
        spawn_fake_server_with_live_rules(64, auth, None, ruleset).await;

    let collect = tokio::spawn(subscribe_until_close_or_timeout(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Recompile the SAME SyncRules (what a whitespace-only edit produces:
    // identical checksum, identical per-table decisions) and notify anyway.
    // `watch::Sender::send` notifies unconditionally regardless of value
    // equality — production's `watch_rules` skips calling `send` at all on a
    // checksum no-op, so this exercises `write_loop`'s own per-table
    // decision-equality check rather than that upstream dedup.
    let recompiled = ActiveRuleset::compile(&rules).unwrap();
    let checksum = recompiled.checksum();
    *rules_shared.write().await = recompiled;
    rules_tx.send(checksum).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));
    svc.fan_out(&insert_event(9, "tasks", "2"), |_, _| {
        Some(ColumnValue::Any)
    })
    .await;

    let res = collect.await.unwrap();
    let frames = res.expect("an identical reload must never close a live session");
    assert!(
        frames.iter().any(|f| f["pk"] == "2"),
        "session must keep receiving events after a no-op reload, got: {frames:?}"
    );
}

// ---------------------------------------------------------------------------
// A malformed rules file must never replace the previous ruleset.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_reload_keeps_previous_ruleset() {
    // Part 1: the load function `watch_rules` (crates/nostos-server/src/main.rs
    // — a binary-crate poller, not reachable from these nostos-infra
    // integration tests) calls on every poll tick rejects malformed TOML
    // outright rather than returning an empty/default ruleset. This is the
    // guard `watch_rules` relies on to `continue` without touching the
    // shared `RwLock` or sending on the checksum channel.
    let path = std::env::temp_dir().join(format!(
        "nostos-rules-reload-test-{}.toml",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, "this is not valid toml [[[").unwrap();
    let load_result = nostos_infra::rules_file::load(&path);
    let _ = std::fs::remove_file(&path);
    load_result.expect_err("malformed TOML must fail to load, never silently become a ruleset");

    // Part 2: with the shared RwLock/Sender never touched — exactly what
    // happens when `watch_rules` hits that error and `continue`s — a live
    // session must keep running under the original ruleset: no close, no
    // fallback to some default, and it still only receives what that
    // original ruleset actually synced.
    let rules = toggles_rules(vec![TableRule {
        table: "tasks".into(),
        sync: true,
        scope: None,
    }]);
    let ruleset = ActiveRuleset::compile(&rules).unwrap();
    let auth: Arc<dyn SyncAuth> = Arc::new(nostos_infra::AllowAnonymous::new());
    let (addr, _server, _mgr, store, _rules_tx, _rules_shared) =
        spawn_fake_server_with_live_rules(64, auth, None, ruleset).await;

    let collect = tokio::spawn(subscribe_until_close_or_timeout(
        addr,
        "tasks",
        COLLECT_TIMEOUT,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));
    svc.fan_out(&insert_event(3, "tasks", "3"), |_, _| {
        Some(ColumnValue::Any)
    })
    .await;

    let res = collect.await.unwrap();
    let frames = res.expect("a failed reload attempt must never disturb a live session");
    assert!(
        frames.iter().any(|f| f["pk"] == "3"),
        "session must keep receiving events under the untouched original ruleset, got: {frames:?}"
    );
}
