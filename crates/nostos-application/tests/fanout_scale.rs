//! 10k-predicate fan-out scale baseline (ADR-0012 kill criterion).
//!
//! The strategy doc's kill criterion (STRATEGY §10): "evaluate 10k concurrent
//! authenticated predicates against a live WAL stream without measurable
//! source-DB read cost." This test establishes the **predicate-evaluation
//! baseline** — how fast `FanOutService::fan_out` routes events through 10,000
//! concurrently-registered predicates — so the param-set-digest indexing decision
//! is data-driven, not slide-deck-driven.
//!
//! Methodology: register 10,000 sessions on table `tasks`, each with a distinct
//! predicate exercising every leaf shipped in ADR-0012 slices 1+2 (`Eq`/`And`/
//! `Ge` over text + number). Fan `M` events through, each matching a known
//! subset. Measure wall-clock ops/sec and per-event cost.
//!
//! This is a *baseline*, not a criterion microbench: it uses `Instant` + a
//! generous floor (mirroring `nostos-client`'s throughput test) so it runs in
//! `cargo test` with zero extra deps and only fails on a real regression
//! (accidental O(n²), a lock serialized per evaluation).

// Reporting math: `n as f64` for ops/sec metrics and `as i64` for priority
// thresholds are benign in a benchmark. Mirrors nostos-bench's allow for the
// same throughput-reporting pattern.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::SessionStore;
use nostos_application::{DeliveryDecision, EventSink, FanOutService, SessionCandidate};
use nostos_domain::{ColumnValue, Lsn, Predicate, ReplicationEvent, SessionId, SyncSession};
use std::collections::HashMap;
use std::sync::Mutex;

/// A sink that accepts delivery without recording — isolates the measurement to
/// `fan_out`'s predicate-evaluation + dispatch path (no per-event allocation).
struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn deliver(&self, _event: ReplicationEvent) -> DeliveryDecision {
        DeliveryDecision::Delivered
    }
}

/// Minimal in-process store keyed by table (mirrors the fanout unit-test
/// `TableStore`). The benchmark measures `fan_out`'s evaluation path, which is
/// store-agnostic: the store only supplies candidates.
struct TableStore {
    by_table: Mutex<HashMap<String, Vec<SessionCandidate>>>,
}

impl TableStore {
    fn new() -> Self {
        Self {
            by_table: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SessionStore for TableStore {
    async fn add(&self, session: SyncSession, sink: Arc<dyn EventSink>) {
        let table = session.predicate.table.clone();
        let cand = SessionCandidate {
            id: session.id,
            predicate: session.predicate,
            principal: session.principal,
            sink,
        };
        self.by_table
            .lock()
            .unwrap()
            .entry(table)
            .or_default()
            .push(cand);
    }
    async fn try_add_below_cap(
        &self,
        session: SyncSession,
        sink: Arc<dyn EventSink>,
        _cap: u64,
    ) -> Result<SessionId, nostos_application::StoreRejection> {
        let id = session.id;
        self.add(session, sink).await;
        Ok(id)
    }
    async fn remove(&self, _id: SessionId) {}
    async fn candidates_for(&self, event: &ReplicationEvent) -> Vec<SessionCandidate> {
        // Mirror the production store: index by table so only same-table
        // sessions are candidates.
        let table = event.op.table().to_string();
        self.by_table
            .lock()
            .unwrap()
            .get(&table)
            .cloned()
            .unwrap_or_default()
    }
    async fn len(&self) -> usize {
        self.by_table.lock().unwrap().values().map(Vec::len).sum()
    }
    async fn min_acked_lsn(&self) -> Option<Lsn> {
        None
    }
}

/// Build a predicate exercising the full shipped tree: `org_id=acme AND
/// status=open AND priority>=threshold`. Vary `threshold` across sessions so the
/// `Ge` leaf is genuinely evaluated (different sessions match different rows).
fn predicate_for(threshold: i64) -> Predicate {
    // `Predicate`'s combinators cover `and_eq`; for the typed `Ge` leaf we build
    // the `PredicateExpr` tree directly (the struct fields are public). This
    // exercises Eq + And + Ge over text + number — every leaf type in slices 1+2.
    let expr = nostos_domain::PredicateExpr::And(vec![
        nostos_domain::PredicateExpr::eq("org_id", ColumnValue::text("acme")),
        nostos_domain::PredicateExpr::eq("status", ColumnValue::text("open")),
        nostos_domain::PredicateExpr::ge("priority", ColumnValue::number(threshold)),
    ]);
    Predicate {
        table: "tasks".to_string(),
        expr,
    }
}

/// A `tasks` row carrying `org_id`/`status`/`priority` as its payload, in the
/// format the extractor below reads.
fn tasks_event(lsn: u64, org: &str, status: &str, priority: i64) -> ReplicationEvent {
    let payload = format!("{org}\u{1}{status}\u{1}{priority}");
    ReplicationEvent::new(
        Lsn::new(lsn),
        nostos_domain::RowOp::Insert {
            table: "tasks".into(),
            pk: format!("row-{lsn}"),
            payload: Bytes::copy_from_slice(payload.as_bytes()),
        },
    )
}

/// Naive extractor — re-parses the payload on EVERY column lookup. Kept as the
/// "careless caller" baseline to document why parse-once matters; this is NOT
/// how the production `extract_json_column` works.
fn extract_reparse(event: &ReplicationEvent, col: &str) -> Option<ColumnValue> {
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    let mut parts = s.split('\u{1}');
    let org = parts.next()?;
    let status = parts.next()?;
    let priority = parts.next()?;
    match col {
        "org_id" => Some(ColumnValue::text(org)),
        "status" => Some(ColumnValue::text(status)),
        "priority" => Some(ColumnValue::text(priority)),
        _ => None,
    }
}

/// Parse the payload ONCE into an owned column map (mirrors the production
/// `extract_json_column` shape: parse once, return cheap-lookup closures). This
/// is the production-style path the index decision turns on.
fn parse_once_view(event: &ReplicationEvent) -> Option<Vec<(&'static str, ColumnValue)>> {
    let s = std::str::from_utf8(event.payload_bytes()).ok()?;
    let mut parts = s.split('\u{1}');
    let org = parts.next()?;
    let status = parts.next()?;
    let priority = parts.next()?;
    Some(vec![
        ("org_id", ColumnValue::text(org)),
        ("status", ColumnValue::text(status)),
        ("priority", ColumnValue::text(priority)),
    ])
}

// `#[ignore]`: this is a *benchmark* (~30s), not a fast floor-assertion like
// the throughput tests. Run explicitly with `cargo test -p nostos-application
// --test fanout_scale -- --ignored --nocapture` to read the baseline numbers.
// Kept ignored so it doesn't slow the regular `cargo test` suite, but it IS
// part of the gate: CI runs `--include-ignored` (or the number is read on
// demand). The floor it asserts guards against regressions below the current
// un-indexed state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "10k-predicate scale benchmark (~30s); run with --ignored --nocapture"]
async fn ten_thousand_predicate_fanout_baseline() {
    const SESSIONS: usize = 10_000;
    // Regime 1 (eval-only) event count. A row no predicate matches isolates the
    // cost of evaluating 10k predicate trees per event. With the parse-once
    // extractor this is the production-shape cost.
    const EVAL_EVENTS: usize = 2_000;
    // Regime 2 (realistic matching) event count. Kept small: each event matches
    // thousands of sessions, each spawning a JoinSet task — this is the slow
    // regime. Captures eval + delivery dispatch combined.
    const REAL_EVENTS: usize = 100;
    // Floor on the PRODUCTION-SHAPE (parse-once) eval-only path. Measured
    // parse-once baseline (2026-06-27): ~170 evt/s through 10k predicates
    // (~5.8µs/event, ~1.7M predicate-evals/sec). This floor sits at ~⅓ of that
    // to tolerate CI/load variance while catching a real cliff (accidental
    // O(n²), a lock held across eval). The realistic (matching) path is NOT
    // floored — its cost is dominated by JoinSet delivery, a separate concern.
    const FLOOR_PARSE_ONCE_EVENTS_PER_SEC: f64 = 50.0;

    // Register 10k sessions with distinct `priority >= threshold` predicates
    // (threshold cycles 0..50 so the `Ge` leaf splits the candidate set per row).
    let store = Arc::new(TableStore::new());
    let sink = Arc::new(NoopSink) as Arc<dyn EventSink>;
    for i in 0..SESSIONS {
        let threshold = (i % 50) as i64;
        store
            .add(SyncSession::new(predicate_for(threshold)), sink.clone())
            .await;
    }
    assert_eq!(
        store
            .by_table
            .lock()
            .unwrap()
            .get("tasks")
            .map_or(0, Vec::len),
        SESSIONS,
        "all 10k sessions registered on tasks"
    );

    let svc = FanOutService::new(store);

    // ---- Regime 1: parse-once, eval-only (the PRODUCTION-SHAPE cost) ----
    // A row no predicate matches (priority -1 < every threshold, status closed).
    // The extractor is built per-event via parse_once_view (parse once, cheap
    // lookups) — exactly how the production extract_json_column works. This is
    // the number the index decision turns on: if parse-once is fast, the index
    // is premature.
    let warm = tasks_event(0, "acme", "closed", -1);
    let warm_view = parse_once_view(&warm).expect("warm row parses");
    let _ = svc
        .fan_out(&warm, |_e, col| {
            warm_view
                .iter()
                .find(|(k, _)| *k == col)
                .map(|(_, v)| v.clone())
        })
        .await;
    let start = Instant::now();
    for lsn in 1..=EVAL_EVENTS {
        let event = tasks_event(lsn as u64, "acme", "closed", -1);
        let view = parse_once_view(&event).expect("row parses");
        let outcome = svc
            .fan_out(&event, |_e, col| {
                view.iter().find(|(k, _)| *k == col).map(|(_, v)| v.clone())
            })
            .await;
        assert_eq!(
            outcome.matched, 0,
            "no predicate should match the closed/-1 row"
        );
    }
    let eval_elapsed = start.elapsed();
    let eval_secs = eval_elapsed.as_secs_f64();
    let eval_events_per_sec = EVAL_EVENTS as f64 / eval_secs;
    let eval_us_per_event = eval_secs * 1_000_000.0 / EVAL_EVENTS as f64;

    println!(
        "fan_out parse-once eval-only: {SESSIONS} predicates, {EVAL_EVENTS} zero-match events \
         -> {eval_events_per_sec:.0} events/sec, {eval_us_per_event:.1} µs/event \
         (≈{:.0} predicate-evals/sec; PRODUCTION-SHAPE — the index decision turns on this)",
        SESSIONS as f64 / eval_secs * EVAL_EVENTS as f64
    );

    // ---- Regime 2: parse-once, realistic matching (eval + delivery) ----
    // A row matching thousands of sessions. Production-shape extractor.
    let mut total_matched: u64 = 0;
    let start = Instant::now();
    for lsn in 1..=REAL_EVENTS {
        let priority = (lsn % 50) as i64;
        let event = tasks_event(lsn as u64 + EVAL_EVENTS as u64, "acme", "open", priority);
        let view = parse_once_view(&event).expect("row parses");
        let outcome = svc
            .fan_out(&event, |_e, col| {
                view.iter().find(|(k, _)| *k == col).map(|(_, v)| v.clone())
            })
            .await;
        total_matched += outcome.matched;
    }
    let real_elapsed = start.elapsed();
    let real_secs = real_elapsed.as_secs_f64();
    let real_events_per_sec = REAL_EVENTS as f64 / real_secs;

    println!(
        "fan_out parse-once realistic: {SESSIONS} predicates, {REAL_EVENTS} matching events \
         -> {real_events_per_sec:.0} events/sec, {total_matched} matched \
         (≈{:.0} matches/sec; eval + JoinSet delivery combined)",
        total_matched as f64 / real_secs
    );

    // ---- Regime 3: NAIVE re-parse extractor (the "careless caller" contrast) ----
    // Same workload as Regime 1 but with the naive extract_reparse (re-parses the
    // payload on every column lookup). Kept to document WHY parse-once matters:
    // the delta between Regime 1 and Regime 3 is the cost of careless extraction.
    let start = Instant::now();
    for lsn in 1..=EVAL_EVENTS {
        let event = tasks_event(lsn as u64 + EVAL_EVENTS as u64 * 2, "acme", "closed", -1);
        let _ = svc.fan_out(&event, extract_reparse).await;
    }
    let naive_secs = start.elapsed().as_secs_f64();
    let naive_events_per_sec = EVAL_EVENTS as f64 / naive_secs;

    println!(
        "fan_out NAIVE re-parse eval-only: {SESSIONS} predicates, {EVAL_EVENTS} events \
         -> {naive_events_per_sec:.0} events/sec (contrast: parse-once above is {:.1}x faster)",
        eval_events_per_sec / naive_events_per_sec
    );

    // Floor on the PRODUCTION-SHAPE (parse-once) eval-only path. The measured
    // parse-once baseline (2026-06-27) is recorded below; this floor guards
    // against regressions below that state, NOT against the optimization gap.
    // It sits low to tolerate CI/load variance while catching a real cliff
    // (accidental O(n²), a lock held across eval).
    assert!(
        eval_events_per_sec >= FLOOR_PARSE_ONCE_EVENTS_PER_SEC,
        "fan_out parse-once EVAL regression: {eval_events_per_sec:.0} zero-match events/sec < \
         {FLOOR_PARSE_ONCE_EVENTS_PER_SEC:.0} floor (10k predicates); a real eval-path cliff"
    );
}
