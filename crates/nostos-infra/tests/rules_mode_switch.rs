//! Mode-switch truth-transfer tests (ADR-0031) — Task 18.
//!
//! Acceptance gate for the ratified truth-switching semantics: `sync_mode`
//! selects WHICH section of `nostos_rules.toml` is truth, but switching away
//! from a mode must never delete that mode's section (`rules_file::save`
//! always writes both; `set_mode` only ever rewrites `sync_mode`), and any
//! flip must change the composed sync epoch so every connected client
//! resyncs against the new decisions. This file proves those two properties
//! hold across every mode pair, plus that `all` mode — despite disabling the
//! rules engine — never disables tenant scoping (ADR-0011, Global Constraint
//! 11).
//!
//! These are pure acceptance tests: no production code lives here. If one
//! fails, the bug is in Tasks 4 (`nostos_domain::rules`), 6/7
//! (`nostos_infra::rules_file`), or 14 (live rules reload / transport
//! wiring) — fix it there, not here.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use nostos_application::ports::SyncAuth;
use nostos_application::{ActiveRuleset, FanOutService, RuleDecision};
use nostos_domain::{
    compose_sync_epoch, ColumnValue, HandRule, Lsn, Principal, ReplicationEvent, RowOp, SyncMode,
    SyncRules, TableRule, RULES_VERSION,
};
use nostos_infra::rules_file;

use common::spawn_fake_server_with_rules;

/// A fresh, non-colliding rules-file path under `std::env::temp_dir()` — same
/// pattern `rules_file.rs`'s own tests use, so parallel test runs never race
/// on the same file.
fn temp_rules_path() -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("nostos-rules-mode-switch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(rules_file::RULES_FILE_NAME)
}

fn principal() -> Principal {
    Principal::new("u1", "t1")
}

/// Compile `rules` and ask whether `table` is synced for a claim-less
/// principal. Scopes in this file are all `None` (whole-table), so a missing
/// claim can never be the reason for a denial here — only `DeniedTable` is in
/// play, which is exactly what "mode switch changed what's read" means.
fn is_allowed(rules: &SyncRules, table: &str) -> bool {
    let ruleset = ActiveRuleset::compile(rules).expect("compile validated rules");
    matches!(ruleset.decide(table, &principal()), RuleDecision::Allow(_))
}

#[test]
fn toggles_to_hand_transfers_truth() {
    let path = temp_rules_path();
    let rules = SyncRules {
        version: RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![TableRule {
            table: "tasks".to_string(),
            sync: true,
            scope: None,
        }],
        hand: vec![HandRule {
            table: "notes".to_string(),
            scope: None,
        }],

        streams: vec![],
    };
    rules_file::save(&path, &rules).expect("save");

    let loaded = rules_file::load(&path).expect("load").expect("file exists");
    assert!(
        is_allowed(&loaded, "tasks"),
        "toggles: tasks must be allowed"
    );
    assert!(
        !is_allowed(&loaded, "notes"),
        "toggles: notes is hand-only, must be denied while toggles is active"
    );

    rules_file::set_mode(&path, SyncMode::Hand).expect("set_mode");

    let after = rules_file::load(&path).expect("load").expect("file exists");
    assert!(
        is_allowed(&after, "notes"),
        "hand: notes must now be allowed — truth transferred to the hand section"
    );
    assert!(
        !is_allowed(&after, "tasks"),
        "hand: tasks is toggles-only, must now be denied — toggles truth is inert"
    );
}

#[test]
fn hand_to_toggles_deactivates_hand_file() {
    let path = temp_rules_path();
    let rules = SyncRules {
        version: RULES_VERSION,
        mode: SyncMode::Hand,
        tables: vec![TableRule {
            table: "tasks".to_string(),
            sync: true,
            scope: None,
        }],
        hand: vec![HandRule {
            table: "notes".to_string(),
            scope: None,
        }],

        streams: vec![],
    };
    rules_file::save(&path, &rules).expect("save");

    let loaded = rules_file::load(&path).expect("load").expect("file exists");
    assert!(is_allowed(&loaded, "notes"), "hand: notes must be allowed");
    assert!(
        !is_allowed(&loaded, "tasks"),
        "hand: tasks is toggles-only, must be denied while hand is active"
    );

    rules_file::set_mode(&path, SyncMode::Toggles).expect("set_mode");

    let after = rules_file::load(&path).expect("load").expect("file exists");
    // The hand section is still present on disk...
    assert_eq!(
        after.hand, rules.hand,
        "the [[rules]] hand section must survive the switch to toggles"
    );
    // ...but has no effect: notes is denied even though [[rules]] still lists it.
    assert!(
        !is_allowed(&after, "notes"),
        "toggles: notes must be denied — the hand file is on disk but deactivated"
    );
    assert!(
        is_allowed(&after, "tasks"),
        "toggles: tasks must now be allowed — toggles truth is active again"
    );
}

#[test]
fn all_ignores_but_preserves_both_sections() {
    let path = temp_rules_path();
    let rules = SyncRules {
        version: RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![
            TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: None,
            },
            TableRule {
                table: "notes".to_string(),
                sync: false,
                scope: None,
            },
        ],
        hand: vec![HandRule {
            table: "notes".to_string(),
            scope: Some("org_id = claims.org_id".to_string()),
        }],

        streams: vec![],
    };
    rules_file::save(&path, &rules).expect("save");
    rules_file::set_mode(&path, SyncMode::All).expect("set_mode");

    let loaded = rules_file::load(&path).expect("load").expect("file exists");
    assert_eq!(loaded.mode, SyncMode::All);
    assert!(is_allowed(&loaded, "tasks"), "all: every table is allowed");
    assert!(
        is_allowed(&loaded, "notes"),
        "all: even a toggled-off table is allowed — the toggle section is ignored"
    );
    assert!(
        is_allowed(&loaded, "some_unlisted_table"),
        "all: even a table absent from both sections is allowed"
    );

    // Preserved, not merely re-derivable. `[tables.*]` is a name-keyed TOML
    // table (`BTreeMap<String, TableEntry>` in `RulesFileMirror`), so it
    // round-trips re-sorted by table name — compare contents order-
    // insensitively. `[[rules]]` is a plain TOML array and preserves order,
    // so exact `Vec` equality applies there.
    let mut loaded_tables = loaded.tables.clone();
    let mut expected_tables = rules.tables.clone();
    loaded_tables.sort_by(|a, b| a.table.cmp(&b.table));
    expected_tables.sort_by(|a, b| a.table.cmp(&b.table));
    assert_eq!(
        loaded_tables, expected_tables,
        "toggles section must be preserved under all mode (order-insensitive: \
         [tables.*] is a name-keyed TOML table)"
    );
    assert_eq!(
        loaded.hand, rules.hand,
        "hand section must be byte-preserved under all mode"
    );
}

#[test]
fn all_to_toggles_restores_the_toggle_artifact() {
    let path = temp_rules_path();
    let rules = SyncRules {
        version: RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![
            TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: None,
            },
            TableRule {
                table: "notes".to_string(),
                sync: false,
                scope: None,
            },
        ],
        hand: Vec::new(),

        streams: vec![],
    };
    rules_file::save(&path, &rules).expect("save");

    let pre_all = rules_file::load(&path).expect("load").expect("file exists");
    let pre_all_tasks = is_allowed(&pre_all, "tasks");
    let pre_all_notes = is_allowed(&pre_all, "notes");
    assert!(pre_all_tasks, "sanity: tasks starts allowed under toggles");
    assert!(!pre_all_notes, "sanity: notes starts denied under toggles");

    rules_file::set_mode(&path, SyncMode::All).expect("set_mode");
    rules_file::set_mode(&path, SyncMode::Toggles).expect("set_mode");

    let after = rules_file::load(&path).expect("load").expect("file exists");
    assert_eq!(
        is_allowed(&after, "tasks"),
        pre_all_tasks,
        "tasks decision after the all round-trip must match the pre-all decision"
    );
    assert_eq!(
        is_allowed(&after, "notes"),
        pre_all_notes,
        "notes decision after the all round-trip must match the pre-all decision"
    );
}

#[test]
fn mode_flip_alone_changes_checksum() {
    // Same two sections in every case — only `mode` (which section is
    // "active" for canonicalization) changes between the three rulesets.
    let rules_for = |mode: SyncMode| SyncRules {
        version: RULES_VERSION,
        mode,
        tables: vec![TableRule {
            table: "tasks".to_string(),
            sync: true,
            scope: Some("owner_id = claims.sub".to_string()),
        }],
        hand: vec![HandRule {
            table: "notes".to_string(),
            scope: Some("org_id = claims.org_id".to_string()),
        }],

        streams: vec![],
    };

    let all_sum = rules_for(SyncMode::All).checksum();
    let toggles_sum = rules_for(SyncMode::Toggles).checksum();
    let hand_sum = rules_for(SyncMode::Hand).checksum();

    assert_ne!(all_sum, toggles_sum, "all vs toggles checksums must differ");
    assert_ne!(all_sum, hand_sum, "all vs hand checksums must differ");
    assert_ne!(
        toggles_sum, hand_sum,
        "toggles vs hand checksums must differ"
    );

    // Fold each into a composed sync epoch (Task 5, ADR-0025 slice 4b sibling)
    // against the SAME slot epoch, so any difference is attributable only to
    // the rules-mode flip.
    let slot_epoch = 42u64;
    let all_epoch = compose_sync_epoch(slot_epoch, all_sum);
    let toggles_epoch = compose_sync_epoch(slot_epoch, toggles_sum);
    let hand_epoch = compose_sync_epoch(slot_epoch, hand_sum);

    assert_ne!(
        all_epoch, toggles_epoch,
        "flipping all -> toggles must change the composed sync epoch (forces resync)"
    );
    assert_ne!(
        all_epoch, hand_epoch,
        "flipping all -> hand must change the composed sync epoch (forces resync)"
    );
    assert_ne!(
        toggles_epoch, hand_epoch,
        "flipping toggles -> hand must change the composed sync epoch (forces resync)"
    );
}

// ---------------------------------------------------------------------------
// Test 6: `all` mode disables the rules engine, never tenant scoping
// (ADR-0011, Global Constraint 11).
//
// The unit-level half of this invariant already lives in
// `crates/nostos-infra/src/transport.rs::all_mode_still_applies_tenant_scope`
// (Task 10) — it drives the private `build_predicate` composer directly with
// `ActiveRuleset::all_mode()` and a tenant column, and asserts a foreign
// tenant's `ColumnValue` fails to match. This test supplies the
// integration-level half: a real WebSocket session, through the production
// axum handler, must never receive a foreign tenant's row even though the
// active ruleset allows every table unconditionally.
// ---------------------------------------------------------------------------

/// `SyncAuth` test-double: token "A" -> tenant "A"; token "B" -> tenant "B".
/// Mirrors `ws_contract.rs`'s `TenantAuth` (not exported cross-test-binary —
/// integration test binaries only share `tests/common`).
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

/// Extract a string field from the event's small JSON-ish payload.
fn extract_json_field(e: &ReplicationEvent, field: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(e.payload_bytes()).ok()?;
    let needle = format!("\"{field}\":\"");
    let start = s.find(&needle)? + needle.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    Some(ColumnValue::text(&rest[..end]))
}

/// Connect authenticated as `token`, subscribe to `table`, and collect
/// delivered data frames until `timeout` elapses.
async fn subscribe_with_token_and_collect(
    addr: std::net::SocketAddr,
    table: &str,
    token: &str,
    timeout: Duration,
) -> Vec<serde_json::Value> {
    let url = format!("ws://{addr}/sync?token={token}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    let sub = format!("{{\"type\":\"subscribe\",\"table\":\"{table}\"}}");
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

#[tokio::test]
async fn all_mode_never_bypasses_tenant_scope() {
    let auth: Arc<dyn SyncAuth> = Arc::new(TenantAuth);
    let (addr, _server, _mgr, store) =
        spawn_fake_server_with_rules(64, auth, Some("tenant_id"), ActiveRuleset::all_mode()).await;

    // Tenant A subscribes to `tasks`. The active ruleset is `all` mode: it
    // has no opinion on `tasks` at all — every table is unconditionally
    // allowed at the rules layer. Tenant enforcement must still apply.
    let collect = tokio::spawn(subscribe_with_token_and_collect(
        addr,
        "tasks",
        "A",
        Duration::from_secs(2),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let svc = Arc::new(FanOutService::new(store.clone()));

    let own_tenant_row = ReplicationEvent::new(
        Lsn::new(1),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "1".into(),
            payload: Bytes::from_static(b"{\"tenant_id\":\"A\"}"),
        },
    );
    svc.fan_out(&own_tenant_row, extract_json_field).await;

    let foreign_tenant_row = ReplicationEvent::new(
        Lsn::new(2),
        RowOp::Insert {
            table: "tasks".into(),
            pk: "2".into(),
            payload: Bytes::from_static(b"{\"tenant_id\":\"B\"}"),
        },
    );
    svc.fan_out(&foreign_tenant_row, extract_json_field).await;

    let frames = collect.await.unwrap();
    assert!(
        frames.iter().any(|f| f["pk"] == "1"),
        "own-tenant (A) row must be delivered under all mode, got: {frames:?}"
    );
    assert!(
        frames.iter().all(|f| f["pk"] != "2"),
        "foreign-tenant (B) row must NOT be delivered — all mode disables rules, \
         not tenancy (ADR-0011). got: {frames:?}"
    );
}
