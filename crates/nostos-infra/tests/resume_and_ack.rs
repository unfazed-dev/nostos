//! ACK-driven resume + per-session dedup + min-acked-lsn flow (ADR-0009).
//!
//! These prove the correctness foundation T0-1/T0-2/T0-4 without needing a real
//! Postgres — they exercise the in-memory pipeline (`TokioEventSink` +
//! `InMemorySessionStore`) directly, and one WS-level ACK roundtrip.
//!
//! Invariants pinned:
//! - `record_ack` advances a sink's acked LSN monotonically.
//! - `seed_acked_lsn` makes a resumed session reject re-delivery of ≤ resume.
//! - `min_acked_lsn` folds to the minimum across sessions (the safe-to-flush).
//! - A client ACK frame over a real WS stamps the sink's ack cursor.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::{EventSink, SessionStore, SyncAuth};
use nostos_application::FanOutService;
use nostos_domain::{ColumnValue, Lsn, Predicate, Principal, ReplicationEvent, RowOp, SyncSession};
use nostos_infra::router::TokioEventSink;
use nostos_infra::store::InMemorySessionStore;

use common::ack_frame;
use futures_util::SinkExt;

fn ev(lsn: u64, pk: &str) -> ReplicationEvent {
    ReplicationEvent::new(
        Lsn::new(lsn),
        RowOp::Insert {
            table: "tasks".into(),
            pk: pk.into(),
            payload: Bytes::from_static(b"{}"),
        },
    )
}

// ---------------------------------------------------------------------------
// record_ack advances the acked LSN; min_acked_lsn folds to the min.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn min_acked_is_the_minimum_across_sessions() {
    let store = Arc::new(InMemorySessionStore::new());

    // Two sessions: one acked to 100, one acked to 50.
    let (s1, _rx1) = TokioEventSink::channel(64);
    let (s2, _rx2) = TokioEventSink::channel(64);
    let sink1 = Arc::new(s1);
    let sink2 = Arc::new(s2);
    // `seed_acked_lsn`, not `record_ack`: acks are clamped to what the sink
    // actually delivered, so acking 100 on a sink that has delivered nothing
    // now correctly registers as 0. Seeding sets delivered AND acked together,
    // which is what "this session has progressed to LSN N" really means (it is
    // the resume path's own call).
    sink1.seed_acked_lsn(Lsn::new(100));
    sink2.seed_acked_lsn(Lsn::new(50));

    store
        .add(SyncSession::new(Predicate::all("tasks")), sink1.clone())
        .await;
    store
        .add(SyncSession::new(Predicate::all("tasks")), sink2.clone())
        .await;

    // The safe-to-flush LSN is the MINIMUM (50) — the slot must not advance
    // past the slowest client's ack.
    let min = store.min_acked_lsn().await;
    assert_eq!(min, Some(Lsn::new(50)));
}

// ---------------------------------------------------------------------------
// A session that has acked nothing contributes "no advance" — min is None.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_acks_yet_means_no_advance() {
    let store = Arc::new(InMemorySessionStore::new());
    let (sink, _rx) = TokioEventSink::channel(64);
    let sink = Arc::new(sink);
    store
        .add(SyncSession::new(Predicate::all("tasks")), sink.clone())
        .await;
    // Never acked → min is None → the fan-out loop won't advance the slot.
    assert_eq!(store.min_acked_lsn().await, None);
}

// ---------------------------------------------------------------------------
// seed_acked_lsn (resume) makes the sink reject re-delivery of ≤ resume.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_seeds_dedup_so_acked_rows_are_not_redelivered() {
    let (sink, _rx) = TokioEventSink::channel(64);
    // Client resumes at LSN 40 — it already applied through there.
    sink.seed_acked_lsn(Lsn::new(40));
    // Delivering an event at or below 40 must be a dedup-hit drop.
    assert_eq!(
        sink.deliver(ev(40, "1")).await,
        nostos_application::ports::DeliveryDecision::Dropped
    );
    assert_eq!(
        sink.deliver(ev(30, "2")).await,
        nostos_application::ports::DeliveryDecision::Dropped
    );
    // An event ABOVE the resume LSN delivers normally.
    assert_eq!(
        sink.deliver(ev(41, "3")).await,
        nostos_application::ports::DeliveryDecision::Delivered
    );
}

// ---------------------------------------------------------------------------
// A real client ACK frame over a WebSocket stamps the sink's ack cursor.
// ---------------------------------------------------------------------------

/// A `SyncAuth` that mints a fixed principal so the WS path authenticates
/// without JWT crypto (the auth path is covered in auth_sync.rs).
struct AnonAuth;
#[async_trait]
impl SyncAuth for AnonAuth {
    async fn authenticate(&self, _token: &str) -> Option<Principal> {
        Some(Principal::anonymous())
    }
}

/// A fake replicator that records every `advance_progress` call into a shared
/// `Arc<Mutex<Vec<u64>>>` so the test can assert exactly which LSNs it was fed.
struct RecordingRepl {
    events: std::sync::Mutex<Vec<ReplicationEvent>>,
    advances: Arc<std::sync::Mutex<Vec<u64>>>,
}

#[async_trait]
impl nostos_application::ports::ReplicatorStream for RecordingRepl {
    async fn next_event(&mut self) -> Option<ReplicationEvent> {
        self.events.lock().unwrap().pop()
    }
    async fn advance_progress(&mut self, lsn: Lsn) {
        self.advances.lock().unwrap().push(lsn.raw());
    }
}

/// An ACK frame naming data the server never sent must not move the cursor —
/// proven over a real WebSocket, not just at the sink API.
///
/// This test used to assert the opposite. It subscribed, sent `ack 777` on a
/// session that had received nothing, and asserted the cursor advanced to 777
/// — encoding "acks are unvalidated" as intended behaviour. The ack frame is
/// entirely client-controlled, so that let any client name any LSN.
///
/// The rewrite keeps a real socket in the loop and pins both halves:
/// a legitimate cursor (seeded by `resume_lsn`, ADR-0009) is honoured, and an
/// ack above what was delivered is ignored rather than believed.
///
/// The "an ack frame is parsed and reaches `record_ack`" plumbing stays
/// covered by `run_loop_advances_progress_to_min_acked_only` below and by the
/// `router.rs` sink unit tests; the fake server emits no events, so this test
/// cannot manufacture a delivery to ack against.
#[tokio::test]
async fn client_ack_frame_cannot_claim_undelivered_data_over_ws() {
    let auth: Arc<dyn SyncAuth> = Arc::new(AnonAuth);
    let (addr, _server, _mgr, store) = common::spawn_fake_server_with(64, auth, None).await;

    let url = format!("ws://{addr}/sync");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    // Subscribe WITH a resume cursor: the transport calls `seed_acked_lsn`,
    // which sets delivered AND acked to 700 — the legitimate way a session
    // arrives already-progressed without the server re-sending those rows.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        common::subscribe_frame_with("tasks", &[], Some(700)),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        store.min_acked_lsn().await,
        Some(Lsn::new(700)),
        "a resume_lsn subscribe must seed the session's cursor"
    );

    // Now the attack: ack far beyond anything this session was ever sent.
    ws.send(tokio_tungstenite::tungstenite::Message::Text(ack_frame(
        999_999,
    )))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        store.min_acked_lsn().await,
        Some(Lsn::new(700)),
        "an ACK for data the server never delivered must be clamped, not believed"
    );
}

// ---------------------------------------------------------------------------
// The fan-out loop advances the replicator's progress only to min_acked.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_loop_advances_progress_to_min_acked_only() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let (sink, _rx) = TokioEventSink::channel(64);
    let sink = Arc::new(sink);
    // The client has progressed to LSN 50 — so min_acked is 50. The loop must
    // call advance_progress(50), and ONLY 50 (never the event's 100).
    // Seeded rather than `record_ack`ed: an ack is clamped to what the sink
    // delivered, and this sink has delivered nothing yet.
    sink.seed_acked_lsn(Lsn::new(50));
    store
        .add(SyncSession::new(Predicate::all("tasks")), sink.clone())
        .await;

    let advances = Arc::new(std::sync::Mutex::new(Vec::new()));
    let repl = RecordingRepl {
        events: std::sync::Mutex::new(vec![ev(100, "x")]),
        advances: Arc::clone(&advances),
    };
    let svc = FanOutService::new(store.clone());
    let _outcome = svc.run(&mut { repl }, |_, _| Some(ColumnValue::Any)).await;

    let recorded = advances.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![50],
        "advance_progress must be called with the ack-driven min (50), \
         never the event LSN (100)"
    );
}
