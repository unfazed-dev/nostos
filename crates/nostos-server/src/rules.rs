//! The live ruleset (ADR-0031): the `nostos_rules.toml` reload watcher and the
//! `GET`/`PUT /rules` handlers.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use nostos_application::ports::Metrics;
use nostos_domain::SyncMode;
use nostos_infra::transport::SyncRouterState;
use tracing::{info, warn};

use crate::admin_auth;
use crate::endpoints::metadata_protected;

/// Poll `path` every `poll_interval` and swap the shared ruleset when its
/// canonical checksum changes (ADR-0031 D3 — no engine restart). A malformed
/// or unreadable file is logged and skipped: the previous ruleset stays
/// authoritative, never silently widened. `rules_tx` both stores the current
/// checksum (read back each tick to detect no-op reloads) and wakes every
/// live socket's `write_loop` select arm (`crates/nostos-infra/src/
/// transport.rs`) so it can re-verify its own subscriptions.
pub(crate) async fn watch_rules(
    path: std::path::PathBuf,
    rules: Arc<tokio::sync::RwLock<nostos_application::ActiveRuleset>>,
    poll_interval: std::time::Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    rules_tx: tokio::sync::watch::Sender<u64>,
) {
    // Warn on the present -> missing transition only: a zero-config deploy
    // (no file, `all` mode) must not log every poll.
    let mut present = path.exists();
    loop {
        tokio::select! {
            () = tokio::time::sleep(poll_interval) => {}
            res = shutdown.changed() => {
                match res {
                    Ok(()) if *shutdown.borrow() => return,
                    Ok(()) => continue,
                    Err(_) => return, // sender dropped; nothing left to watch for
                }
            }
        }
        let loaded = match nostos_infra::rules_file::load(&path) {
            Ok(Some(raw)) => {
                present = true;
                raw
            }
            Ok(None) => {
                if std::mem::take(&mut present) {
                    warn!(path = %path.display(), "nostos_rules.toml missing on reload poll; keeping previous ruleset");
                }
                continue;
            }
            Err(e) => {
                present = true;
                warn!(error = %e, path = %path.display(), "nostos_rules.toml reload failed to load; keeping previous ruleset");
                continue;
            }
        };
        let compiled = match nostos_application::ActiveRuleset::compile(&loaded) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "nostos_rules.toml reload failed to compile; keeping previous ruleset");
                continue;
            }
        };
        let new_checksum = compiled.checksum();
        if new_checksum == *rules_tx.borrow() {
            continue; // canonical form unchanged (e.g. only whitespace edited)
        }
        info!(
            sync_mode = compiled.mode().as_str(),
            tables = compiled.synced_tables().len(),
            old_checksum = format!("{:x}", *rules_tx.borrow()),
            new_checksum = format!("{:x}", new_checksum),
            "sync rules reloaded"
        );
        // Swap BEFORE notifying: a session woken by `rules_tx.send` must see
        // the new ruleset when it reads `rules`, never a stale one.
        *rules.write().await = compiled;
        let _ = rules_tx.send(new_checksum);
    }
}

/// `GET /rules` — what the server is ENFORCING right now (ADR-0031). The read
/// half of the route; Task 20 adds `.put()` on the same path, so this handler
/// is read-only, the *route* is not.
///
/// Auth parity with `/schema`: v1 is unauthenticated. `/rules` discloses
/// strictly less than `/schema` already does — table names plus scope column
/// text, no columns, no types — so it matches `/schema`'s policy exactly; if
/// `/schema` is ever gated, gate `/rules` identically in the same commit.
///
/// Never echoes claim *values* — `scope_text` only ever renders column
/// names, operators, and `claims.<name>` references. No principal data, no
/// row counts.
pub(crate) async fn rules_handler(
    State(state): State<SyncRouterState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // `/rules` is an OPERATOR endpoint — its only production reader is the web
    // admin panel, which already holds the admin token for the PUT on the same
    // path. So it gates on the admin token, not sync auth: the ruleset is the
    // tenant model, and no application user should be able to read it back.
    if metadata_protected() {
        let Some(token) = admin_auth::admin_token_from_env() else {
            // Same fail-closed shape as PUT: no admin token configured means
            // there is no way to authorise a read, so the route is not there.
            return Err(StatusCode::NOT_FOUND);
        };
        if !admin_auth::AdminAuth::check(&headers, &token).await {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let ruleset = state.rules.read().await;
    Ok(Json(rules_body(&ruleset, &state.metrics)))
}

/// Shared response shape for `GET /rules` and a successful `PUT /rules`
/// (Task 20) — the client re-renders from the PUT response without a second
/// fetch, so both routes must produce byte-identical JSON for the same
/// `ActiveRuleset`.
fn rules_body(ruleset: &nostos_application::ActiveRuleset, metrics: &Metrics) -> serde_json::Value {
    let slot_epoch = metrics
        .slot_epoch
        .load(std::sync::atomic::Ordering::Relaxed);
    let checksum = ruleset.checksum();
    let sync_epoch = nostos_domain::compose_sync_epoch(slot_epoch, checksum);
    let tables: Vec<_> = ruleset
        .synced_tables()
        .into_iter()
        .map(|table| {
            serde_json::json!({
                "table": table,
                "scope": ruleset.scope_text(table).unwrap_or_default(),
            })
        })
        .collect();
    serde_json::json!({
        "sync_mode": ruleset.mode().as_str(),
        "checksum": format!("0x{checksum:x}"),
        "sync_epoch": format!("0x{sync_epoch:x}"),
        "tables": tables,
    })
}

/// `PUT /rules` request body — the toggle model, not raw TOML: the server
/// owns serialization so the file shape stays canonical (Task 20).
#[derive(serde::Deserialize)]
struct PutRulesRequest {
    /// `"all" | "toggles" | "hand"` — but `"hand"` is REJECTED here (422).
    /// Hand-written `[[rules]]` are the CLI's surface; the toggle editor must
    /// never rewrite a file it cannot faithfully round-trip.
    sync_mode: String,
    tables: Vec<PutRulesTable>,
}

#[derive(serde::Deserialize)]
struct PutRulesTable {
    table: String,
    sync: bool,
    scope: Option<String>,
}

fn rules_422(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": message.into() })),
    )
}

/// `PUT /rules` — the authenticated entry point (Task 21). Ordering matters
/// and is deliberate: the admin-token check runs before anything else,
/// including Content-Type and JSON parsing, using `HeaderMap` + raw `Bytes`
/// instead of axum's `Json<T>` extractor — `Json<T>` runs before a handler
/// body even starts, so an unauthenticated caller sending a malformed body
/// would get a 400/415 from the extractor and learn the route exists before
/// ever reaching the 404. See `admin_auth.rs` for the gate itself and
/// `apply_put_rules` below for the actual mutation (unchanged from Task 20).
pub(crate) async fn put_rules_handler(
    State(state): State<SyncRouterState>,
    headers: axum::http::HeaderMap,
    raw_body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 1. Token unset -> the route is not mounted. Checked before anything
    // else touches `headers` or `raw_body`.
    let Some(admin_token) = admin_auth::admin_token_from_env() else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        ));
    };

    // 2. Bearer token mismatch -> 401. Constant-time compare, no logging.
    if !admin_auth::AdminAuth::check(&headers, &admin_token).await {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        ));
    }

    // 3. CSRF stance (ADR-0031 addendum, Task 21 §2): bearer-header auth has
    // no ambient credential for a cross-site form to ride on, so the one
    // enforceable defence left is rejecting any body that isn't actually
    // JSON — it blocks the simple-form vector outright.
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({ "error": "Content-Type must be application/json" })),
        ));
    }

    let body: PutRulesRequest = serde_json::from_slice(&raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid JSON body: {e}") })),
        )
    })?;

    // Audit "before" snapshot, taken ahead of the mutation. The old-tables
    // read is a second, non-authoritative load purely for the
    // `tables_changed` count below — `apply_put_rules` does its own
    // authoritative load and is the only writer.
    let mode_before = state.rules.read().await.mode().as_str().to_string();
    let checksum_before = state.rules.read().await.checksum();
    let old_tables: Vec<(String, bool, Option<String>)> =
        nostos_infra::rules_file::load(&state.rules_file_path)
            .ok()
            .flatten()
            .map(|r| {
                r.tables
                    .into_iter()
                    .map(|t| (t.table, t.sync, t.scope))
                    .collect()
            })
            .unwrap_or_default();
    let new_tables: Vec<(String, bool, Option<String>)> = body
        .tables
        .iter()
        .map(|t| (t.table.clone(), t.sync, t.scope.clone()))
        .collect();

    let response = apply_put_rules(&state, body).await?;

    // Audit line — success path only (a `?` above returns before this runs
    // on any error). `actor` is a non-secret fingerprint, never the token;
    // `source` is always "api" for now: there is no header distinguishing
    // the web panel from a direct API caller, so both are indistinguishable
    // here (ponytail: add `X-Cairn-Source` if the panel needs separating).
    let mode_after = state.rules.read().await.mode().as_str().to_string();
    let checksum_after = state.rules.read().await.checksum();
    let tables_changed = count_changed_tables(&old_tables, &new_tables);
    let actor = admin_auth::actor_id(&admin_token);
    tracing::info!(
        target: "nostos::audit",
        actor = %actor,
        source = "api",
        mode_before = %mode_before,
        mode_after = %mode_after,
        checksum_before = %format!("0x{checksum_before:x}"),
        checksum_after = %format!("0x{checksum_after:x}"),
        tables_changed = %tables_changed,
        "rules_mutation"
    );

    Ok(response)
}

/// Tables whose (sync, scope) differ between `before` and `after`, plus any
/// table added or removed — a union, not two separate counts, so a table
/// that appears on both sides with a changed scope is counted once.
fn count_changed_tables(
    before: &[(String, bool, Option<String>)],
    after: &[(String, bool, Option<String>)],
) -> usize {
    use std::collections::{BTreeMap, BTreeSet};
    let before_map: BTreeMap<&str, (bool, Option<&str>)> = before
        .iter()
        .map(|(t, s, sc)| (t.as_str(), (*s, sc.as_deref())))
        .collect();
    let after_map: BTreeMap<&str, (bool, Option<&str>)> = after
        .iter()
        .map(|(t, s, sc)| (t.as_str(), (*s, sc.as_deref())))
        .collect();
    let mut changed = BTreeSet::new();
    for (name, v) in &before_map {
        if after_map.get(name) != Some(v) {
            changed.insert(*name);
        }
    }
    for (name, v) in &after_map {
        if before_map.get(name) != Some(v) {
            changed.insert(*name);
        }
    }
    changed.len()
}

/// The actual mutation (Task 20, unchanged): validate → atomically write
/// `nostos_rules.toml` → only then swap the in-process `ActiveRuleset` and
/// notify live sessions. Split out from `put_rules_handler` so the Task 20
/// unit tests below can exercise it directly without going through the
/// Task 21 auth gate, which has its own dedicated tests.
///
/// ponytail: no optimistic concurrency between the CLI editor and PUT
/// /rules — last write wins. Ceiling: a concurrent edit can be silently
/// overwritten. Upgrade path: ETag from the checksum + `If-Match`.
///
/// Ordering (write-then-swap, not the reverse): validate → atomically write
/// `nostos_rules.toml` → only then swap the in-process `ActiveRuleset` and
/// notify live sessions. If the process dies between the write and the swap,
/// the file is truth and startup reloads it — a crash costs a reload, never a
/// divergence between the file and what's enforced. Swapping first could
/// enforce a ruleset that was never persisted.
async fn apply_put_rules(
    state: &SyncRouterState,
    body: PutRulesRequest,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if body.sync_mode == "hand" {
        return Err(rules_422(
            "PUT /rules cannot write sync_mode \"hand\" — hand-written [[rules]] are the \
             CLI's surface; the toggle editor must never rewrite a file it cannot faithfully \
             round-trip. Use `nostos rules edit --mode hand` instead.",
        ));
    }
    let mode = SyncMode::parse(&body.sync_mode).ok_or_else(|| {
        rules_422(nostos_domain::RulesError::UnknownMode(body.sync_mode.clone()).to_string())
    })?;

    let tables: Vec<nostos_domain::TableRule> = body
        .tables
        .into_iter()
        .map(|t| nostos_domain::TableRule {
            table: t.table,
            sync: t.sync,
            scope: t.scope,
        })
        .collect();

    // Truth-switching must never delete an artifact (rules_file.rs): the
    // toggle editor only ever owns `[tables.*]`, so any hand-authored
    // `[[rules]]` already on disk must survive this write untouched. Same
    // for `[streams.*]` (P5): the toggle editor does not own stream
    // definitions either — a PUT must never silently delete them.
    let (hand, streams) = match nostos_infra::rules_file::load(&state.rules_file_path) {
        Ok(Some(existing)) => (existing.hand, existing.streams),
        Ok(None) => (Vec::new(), Vec::new()),
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!(
                        "reading current {}: {e}",
                        state.rules_file_path.display()
                    )
                })),
            ));
        }
    };

    let rules = nostos_domain::SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode,
        tables,
        hand,
        streams,
    };

    // Step 1: validate via the same compile path `nostos rules check` uses —
    // invalid → 422 with that exact error text, nothing touched.
    let compiled =
        nostos_application::ActiveRuleset::compile(&rules).map_err(|e| rules_422(e.to_string()))?;

    // Step 2: atomically write BEFORE swapping in-process state.
    if let Err(e) = nostos_infra::rules_file::save(&state.rules_file_path, &rules) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("writing {}: {e}", state.rules_file_path.display())
            })),
        ));
    }

    // Step 3: only after the write succeeds, swap + notify (Task 14's live-
    // session invalidation runs off this same `watch::Sender` — swap before
    // send, exactly like `watch_rules`, so a woken session never reads stale
    // state). The Task 14 poller dedupes on checksum, so its next tick over
    // this same file is a no-op — no double invalidation.
    let new_checksum = compiled.checksum();
    *state.rules.write().await = compiled;
    let _ = state.rules_tx.send(new_checksum);

    // Step 4: 200 with the same body shape as GET /rules.
    let ruleset = state.rules.read().await;
    Ok(Json(rules_body(&ruleset, &state.metrics)))
}

/// `watch_rules`'s own error-handling and swap/notify ordering, exercised
/// directly (no live socket, no `nostos-infra` test server — that boundary is
/// covered by `crates/nostos-infra/tests/rules_reload.rs` instead). A short
/// `poll_interval` plus one bounded sleep gives every case at least one poll
/// tick before shutdown is signaled; `watch_rules` re-`select!`s the
/// shutdown channel every loop iteration, so it stops promptly once signaled.
#[cfg(test)]
mod watch_rules_tests {
    use super::watch_rules;
    use nostos_application::ActiveRuleset;
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{watch, RwLock};

    fn toggles_rules(tables: Vec<TableRule>) -> SyncRules {
        SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables,
            hand: Vec::new(),
            streams: Vec::new(),
        }
    }

    /// A fresh path under `std::env::temp_dir()`, unique for this test
    /// binary's lifetime. `nostos-server` has no `uuid` dependency (only
    /// `nostos-infra`'s own tests use it) — pid + a monotonic counter is
    /// enough to avoid collisions here.
    fn temp_rules_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nostos-watch-rules-test-{tag}-{}-{n}.toml",
            std::process::id()
        ))
    }

    /// Spawn `watch_rules` against `path`, let it run for a handful of
    /// 5ms poll ticks, then signal shutdown and join.
    async fn run_a_few_ticks(
        path: std::path::PathBuf,
        rules: Arc<RwLock<ActiveRuleset>>,
        rules_tx: watch::Sender<u64>,
    ) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(watch_rules(
            path,
            rules,
            Duration::from_millis(5),
            shutdown_rx,
            rules_tx,
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = shutdown_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn checksum_unchanged_reload_does_not_notify() {
        let rules = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let path = temp_rules_path("unchanged");
        nostos_infra::rules_file::save(&path, &rules).unwrap();

        let compiled = ActiveRuleset::compile(&rules).unwrap();
        let checksum = compiled.checksum();
        let shared = Arc::new(RwLock::new(compiled));
        let (tx, rx) = watch::channel(checksum); // seeded to match what's on disk
                                                 // Keep a sender clone alive in the test: `watch_rules` is handed its
                                                 // own clone and drops it when it returns, and a `Receiver` can't
                                                 // distinguish "sender dropped, no notify" from "sender dropped after
                                                 // notifying" once the channel is fully closed — `has_changed()`
                                                 // reports `Err` either way. Holding one clone open past that point
                                                 // keeps the channel open so `has_changed()` reflects the real state.
        let _tx_keepalive = tx.clone();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            !rx.has_changed().unwrap_or(false),
            "an on-disk file whose compiled checksum matches what's already loaded must never notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            checksum,
            "a checksum-unchanged reload must never touch the shared ruleset"
        );
    }

    #[tokio::test]
    async fn malformed_file_leaves_shared_state_untouched_and_does_not_notify() {
        let rules = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let compiled = ActiveRuleset::compile(&rules).unwrap();
        let checksum = compiled.checksum();
        let shared = Arc::new(RwLock::new(compiled));
        let (tx, rx) = watch::channel(checksum);
        let _tx_keepalive = tx.clone(); // see comment in checksum_unchanged_reload_does_not_notify

        let path = temp_rules_path("malformed");
        std::fs::write(&path, "this is not valid toml [[[").unwrap();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            !rx.has_changed().unwrap_or(false),
            "a malformed rules file must never notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            checksum,
            "a malformed rules file must never touch the shared ruleset"
        );
    }

    #[tokio::test]
    async fn real_change_swaps_then_notifies() {
        let before = toggles_rules(vec![TableRule {
            table: "tasks".into(),
            sync: true,
            scope: None,
        }]);
        let before_compiled = ActiveRuleset::compile(&before).unwrap();
        let before_checksum = before_compiled.checksum();
        let shared = Arc::new(RwLock::new(before_compiled));
        let (tx, rx) = watch::channel(before_checksum);
        let _tx_keepalive = tx.clone(); // see comment in checksum_unchanged_reload_does_not_notify

        let after = toggles_rules(vec![
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
        let after_compiled = ActiveRuleset::compile(&after).unwrap();
        let after_checksum = after_compiled.checksum();
        assert_ne!(
            before_checksum, after_checksum,
            "test fixture bug: the two rulesets must actually differ"
        );

        let path = temp_rules_path("real-change");
        nostos_infra::rules_file::save(&path, &after).unwrap();

        run_a_few_ticks(path.clone(), Arc::clone(&shared), tx).await;
        let _ = std::fs::remove_file(&path);

        assert!(
            rx.has_changed().unwrap_or(false),
            "a real checksum change must notify"
        );
        assert_eq!(
            shared.read().await.checksum(),
            after_checksum,
            "a real reload must swap the shared ruleset to the newly compiled rules"
        );
    }

    /// Zero-config (no file ever) polls silently; a file deleted mid-run
    /// warns once, not every tick. Current-thread runtime, so the
    /// thread-local subscriber sees the spawned task's events.
    #[tokio::test]
    async fn missing_file_warns_only_on_the_present_to_missing_transition() {
        #[derive(Clone, Default)]
        struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .finish(),
        );
        let warns = || {
            String::from_utf8_lossy(&buf.0.lock().unwrap())
                .matches("missing on reload poll")
                .count()
        };

        let rules = toggles_rules(Vec::new());
        let shared = Arc::new(RwLock::new(ActiveRuleset::compile(&rules).unwrap()));
        let (tx, _rx) = watch::channel(0);

        run_a_few_ticks(temp_rules_path("absent"), Arc::clone(&shared), tx.clone()).await;
        assert_eq!(
            warns(),
            0,
            "a never-present file is zero-config, not an error"
        );

        let path = temp_rules_path("deleted");
        nostos_infra::rules_file::save(&path, &rules).unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(watch_rules(
            path.clone(),
            shared,
            Duration::from_millis(5),
            shutdown_rx,
            tx,
        ));
        tokio::task::yield_now().await; // let it see the file at start
        std::fs::remove_file(&path).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = shutdown_tx.send(true);
        let _ = handle.await;
        assert_eq!(warns(), 1, "present -> missing warns exactly once");
    }
}

#[cfg(test)]
mod rules_handler_tests {
    use super::rules_handler;
    use axum::extract::State;
    use axum::response::Json;
    use nostos_application::{ActiveRuleset, SessionManager};
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use nostos_infra::store::InMemorySessionStore;
    use nostos_infra::transport::SyncRouterState;
    use nostos_infra::AllowAnonymous;
    use std::sync::Arc;

    fn state_with(ruleset: ActiveRuleset) -> SyncRouterState {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn nostos_application::ports::SyncAuth> = Arc::new(AllowAnonymous::new());
        let (tx, rx) = tokio::sync::watch::channel(ruleset.checksum());
        SyncRouterState::new(manager, auth).with_rules(
            Arc::new(tokio::sync::RwLock::new(ruleset)),
            rx,
            tx,
        )
    }

    #[tokio::test]
    async fn rules_handler_reports_mode_and_tables() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![
                TableRule {
                    table: "tasks".to_string(),
                    sync: true,
                    scope: Some("owner_id = claims.sub".to_string()),
                },
                TableRule {
                    table: "projects".to_string(),
                    sync: true,
                    scope: Some("org_id = claims.org_id".to_string()),
                },
            ],
            hand: vec![],
            streams: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let checksum = ruleset.checksum();
        let state = state_with(ruleset);

        // Unauthenticated read: the gate is off by default, which is the
        // shape every existing deployment gets.
        let Json(body) = rules_handler(State(state), axum::http::HeaderMap::new())
            .await
            .expect("rules read is open while NOSTOS_PROTECT_METADATA is unset");

        assert_eq!(body["sync_mode"], "toggles");
        assert_eq!(body["checksum"], format!("0x{checksum:x}"));
        let tables = body["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        assert!(tables.contains(&serde_json::json!({
            "table": "tasks",
            "scope": "owner_id = claims.sub",
        })));
        assert!(tables.contains(&serde_json::json!({
            "table": "projects",
            "scope": "org_id = claims.org_id",
        })));
    }

    #[tokio::test]
    async fn rules_handler_under_all_mode_lists_no_tables() {
        let state = state_with(ActiveRuleset::all_mode());

        // Unauthenticated read: the gate is off by default, which is the
        // shape every existing deployment gets.
        let Json(body) = rules_handler(State(state), axum::http::HeaderMap::new())
            .await
            .expect("rules read is open while NOSTOS_PROTECT_METADATA is unset");

        assert_eq!(body["sync_mode"], "all");
        assert_eq!(body["tables"], serde_json::json!([]));
    }
}

#[cfg(test)]
mod put_rules_handler_tests {
    use super::{apply_put_rules, PutRulesRequest, PutRulesTable};
    use axum::http::StatusCode;
    use axum::response::Json;
    use nostos_application::{ActiveRuleset, SessionManager};
    use nostos_domain::{SyncMode, SyncRules, TableRule, RULES_VERSION};
    use nostos_infra::store::InMemorySessionStore;
    use nostos_infra::transport::SyncRouterState;
    use nostos_infra::AllowAnonymous;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A fresh path under `std::env::temp_dir()`, unique for this test
    /// binary's lifetime — mirrors `watch_rules_tests::temp_rules_path`.
    fn temp_rules_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nostos-put-rules-test-{tag}-{}-{n}.toml",
            std::process::id()
        ))
    }

    fn state_with_file(ruleset: ActiveRuleset, path: std::path::PathBuf) -> SyncRouterState {
        let store: Arc<dyn nostos_application::ports::SessionStore> =
            Arc::new(InMemorySessionStore::new());
        let manager = Arc::new(SessionManager::new(store, nostos_domain::Tier::Enterprise));
        let auth: Arc<dyn nostos_application::ports::SyncAuth> = Arc::new(AllowAnonymous::new());
        let (tx, rx) = tokio::sync::watch::channel(ruleset.checksum());
        SyncRouterState::new(manager, auth)
            .with_rules(Arc::new(tokio::sync::RwLock::new(ruleset)), rx, tx)
            .with_rules_file_path(path)
    }

    fn req(sync_mode: &str, tables: Vec<(&str, bool, Option<&str>)>) -> PutRulesRequest {
        PutRulesRequest {
            sync_mode: sync_mode.to_string(),
            tables: tables
                .into_iter()
                .map(|(table, sync, scope)| PutRulesTable {
                    table: table.to_string(),
                    sync,
                    scope: scope.map(str::to_string),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn put_rules_writes_file_and_swaps_active() {
        let path = temp_rules_path("write-and-swap");
        let state = state_with_file(ActiveRuleset::all_mode(), path.clone());

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let result = apply_put_rules(&state, body).await;
        let Json(response) = result.expect("valid PUT must succeed");

        assert_eq!(response["sync_mode"], "toggles");

        // File on disk reflects the new ruleset.
        let on_disk = nostos_infra::rules_file::load(&path)
            .expect("load")
            .expect("file exists");
        assert_eq!(on_disk.mode, SyncMode::Toggles);
        assert_eq!(on_disk.tables.len(), 1);
        assert_eq!(on_disk.tables[0].table, "tasks");

        // In-process ruleset swapped too.
        let active = state.rules.read().await;
        assert_eq!(active.mode(), SyncMode::Toggles);
        assert_eq!(response["checksum"], format!("0x{:x}", active.checksum()));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn put_rules_rejects_hand_mode() {
        let path = temp_rules_path("reject-hand");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path.clone());

        let body = req("hand", vec![]);
        let result = apply_put_rules(&state, body).await;
        let Err((status, Json(err))) = result else {
            panic!("hand mode must be rejected");
        };

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err["error"].as_str().unwrap().contains("hand"));

        // Nothing touched: no file written, active ruleset unchanged.
        assert!(!path.exists());
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }

    #[tokio::test]
    async fn put_rules_rejects_invalid_scope() {
        let path = temp_rules_path("reject-invalid-scope");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path.clone());

        // Same fixture as the Task 17 CLI test
        // (`check_flags_invalid_scope_with_table_name`): a top-level `OR`
        // scope, which the scope compiler rejects.
        let bad_rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![TableRule {
                table: "tasks".to_string(),
                sync: true,
                scope: Some("a = 1 OR b = 2".to_string()),
            }],
            hand: vec![],
            streams: vec![],
        };
        let expected_error = ActiveRuleset::compile(&bad_rules)
            .expect_err("fixture must be invalid")
            .to_string();

        let body = req("toggles", vec![("tasks", true, Some("a = 1 OR b = 2"))]);
        let result = apply_put_rules(&state, body).await;
        let Err((status, Json(err))) = result else {
            panic!("invalid scope must be rejected");
        };

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err["error"], expected_error);

        assert!(!path.exists());
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }

    #[tokio::test]
    async fn put_rules_does_not_double_invalidate() {
        let path = temp_rules_path("no-double-invalidate");
        let state = state_with_file(ActiveRuleset::all_mode(), path.clone());
        let mut rules_changed = state.rules_changed.clone();
        // Mark the current value seen so `changed()` only fires on a real update.
        rules_changed.borrow_and_update();

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let _ = apply_put_rules(&state, body)
            .await
            .expect("valid PUT must succeed");

        // The PUT itself notifies exactly once.
        assert!(rules_changed.has_changed().unwrap());
        rules_changed.borrow_and_update();

        // A watcher poll tick over the SAME file (now on disk with the
        // ruleset the PUT already swapped to) computes the same checksum —
        // it must dedupe, mirroring `watch_rules`'s own guard (`if
        // new_checksum == *rules_tx.borrow() { continue; }`) — no second
        // `send`, so `rules_changed` observes no further change.
        let checksum_on_disk = nostos_infra::rules_file::load(&path)
            .expect("load")
            .expect("file exists")
            .checksum();
        assert_eq!(checksum_on_disk, *state.rules_tx.borrow());
        assert!(!rules_changed.has_changed().unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn put_rules_write_failure_leaves_active_ruleset_untouched() {
        // A path under a directory that does not exist: `save` -> `write_mirror`
        // fails at `File::create` (ENOENT) before ever reaching the swap.
        let path = std::env::temp_dir()
            .join(format!(
                "nostos-put-rules-test-nonexistent-dir-{}",
                std::process::id()
            ))
            .join("nostos_rules.toml");
        let initial = ActiveRuleset::all_mode();
        let initial_checksum = initial.checksum();
        let state = state_with_file(initial, path);

        let body = req(
            "toggles",
            vec![("tasks", true, Some("owner_id = claims.sub"))],
        );
        let result = apply_put_rules(&state, body).await;
        let Err((status, _)) = result else {
            panic!("write failure must surface as an error");
        };

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(state.rules.read().await.checksum(), initial_checksum);
    }
}
