//! **D4 Step 0 — the idempotency premise.**
//!
//! The entire no-echo-suppression design (ADR-0013 addendum) rests on ONE
//! property: [`nostos_core::Storage::apply_batch`] is idempotent for the same
//! `(table, pk, payload)`. When client A writes a row, the write goes to the
//! server, comes back through replication, and is delivered to A — *the
//! writer*. A's apply MUST be a no-op upsert. If apply is NOT idempotent, A
//! would see a duplicate of its own write (the "echo"), and the whole
//! 2-way-sync-without-echo-suppression design collapses.
//!
//! This is a test, not a doc claim, because it's load-bearing: D2 (write-back),
//! D3 (outbox), and D4 (chaos write-resume) all assume it. If this fails, the
//! plan says STOP — echo suppression must be designed before proceeding.
//!
//! The two cases below exercise the premise directly against
//! [`SqliteStorage::apply_batch`] (the production apply path, real SQLite):
//!
//! 1. **Same batch, delivered twice.** Build a batch of TWO identical
//!    `RowOp::Upsert`s (same table + pk + payload, same LSN), apply it, then
//!    apply it AGAIN. Row count MUST be 1, payload MUST be unchanged. This is
//!    what an echo looks like inside a single delivery: the same tuple, twice,
//!    in the LSN window the writer already covered.
//! 2. **Two separate batches.** Apply one `RowOp::Upsert`, then apply a SECOND
//!    batch carrying the identical op + LSN (a replay, or a re-delivery after
//!    reconnect). Row count MUST stay 1, payload MUST be identical to case 1.
//!
//! Both cases land at the same final state (one row, one payload) — that's the
//! premise. If they don't, the upsert-by-`(table, pk)` is not collapsing
//! duplicates and the design is broken.

use std::collections::HashSet;

use bytes::Bytes;

use nostos_client::SqliteStorage;
use nostos_core::Storage;
use nostos_domain::{Lsn, RowOp};

/// Build the canonical upsert the premise is checked against: same table, same
/// pk, same payload. Every case in this file reuses this so the "identical op"
/// invariant is literal, not coincidental.
fn echo_upsert() -> RowOp {
    RowOp::Insert {
        table: "tasks".into(),
        pk: "row-x".into(),
        payload: Bytes::from_static(b"the-payload"),
    }
}

/// The LSN the premise batches carry. The SAME value for both applies in a case
/// — an echo / replay arrives at the writer with the writer's own LSN window,
/// so the checkpoint argument is identical across deliveries.
const ECHO_LSN: u64 = 1234;

/// Read the payload stored for `(table, pk)` straight out of `cairn_data`.
/// Mirrors the round-trip check in `offline_writes.rs`.
fn stored_payload(storage: &SqliteStorage, table: &str, pk: &str) -> Vec<u8> {
    let conn = storage.conn_for_test();
    conn.query_row(
        "SELECT payload FROM cairn_data WHERE table_name = ?1 AND pk = ?2",
        rusqlite::params![table, pk],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .expect("row present")
}

// ===========================================================================
// Case 1: deliver the SAME 2-row batch (two identical upserts, same LSN)
// twice. An "echo" inside one delivery window. Row count MUST be 1.
// ===========================================================================
#[test]
fn identical_batch_applied_twice_collapses_to_one_row() {
    let mut s = SqliteStorage::open_in_memory().expect("open sqlite");
    let batch = [(echo_upsert(), ECHO_LSN), (echo_upsert(), ECHO_LSN)]; // two identical ops, same LSN

    s.apply_batch(&batch, Lsn::new(ECHO_LSN), &HashSet::new())
        .expect("first apply");
    s.apply_batch(&batch, Lsn::new(ECHO_LSN), &HashSet::new())
        .expect("second apply (the echo / replay)");

    assert_eq!(
        s.row_count_for_test(),
        1,
        "two identical ops in one batch + an identical re-delivery MUST collapse \
         to a single row — idempotency holds inside a delivery window"
    );
    let payload = stored_payload(&s, "tasks", "row-x");
    assert_eq!(
        payload, b"the-payload",
        "payload unchanged after the re-delivery"
    );
}

// ===========================================================================
// Case 2: two SEPARATE batches, each carrying the identical op + LSN. The
// reconnect-replay shape (and the writer-echo shape). Row count MUST stay 1,
// payload identical to Case 1.
// ===========================================================================
#[test]
fn identical_op_in_two_separate_batches_collapses_to_one_row() {
    let mut s = SqliteStorage::open_in_memory().expect("open sqlite");
    let pair = (echo_upsert(), ECHO_LSN);

    s.apply_batch(
        std::slice::from_ref(&pair),
        Lsn::new(ECHO_LSN),
        &HashSet::new(),
    )
    .expect("first batch");
    s.apply_batch(
        std::slice::from_ref(&pair),
        Lsn::new(ECHO_LSN),
        &HashSet::new(),
    )
    .expect("second batch (the echo, separately delivered)");

    assert_eq!(
        s.row_count_for_test(),
        1,
        "the identical op re-delivered in a second batch MUST NOT create a \
         second row — idempotency holds across delivery boundaries"
    );
    let payload = stored_payload(&s, "tasks", "row-x");
    assert_eq!(
        payload, b"the-payload",
        "payload identical to the single-batch case"
    );
}

// ===========================================================================
// Case 3: the convergence check. The two paths (one big batch vs two batches)
// MUST land at the SAME final state — one row, the same payload. If they
// diverge, the premise is broken in a subtler way than "it duplicated."
// ===========================================================================
#[test]
fn both_delivery_shapes_converge_to_identical_state() {
    let final_payload = b"the-payload";

    // Path A: one batch of two identical ops.
    let mut a = SqliteStorage::open_in_memory().expect("open sqlite A");
    a.apply_batch(
        &[(echo_upsert(), ECHO_LSN), (echo_upsert(), ECHO_LSN)],
        Lsn::new(ECHO_LSN),
        &HashSet::new(),
    )
    .expect("apply A");
    let a_count = a.row_count_for_test();
    let a_payload = stored_payload(&a, "tasks", "row-x");

    // Path B: two separate batches, one identical op each.
    let mut b = SqliteStorage::open_in_memory().expect("open sqlite B");
    let pair = (echo_upsert(), ECHO_LSN);
    b.apply_batch(
        std::slice::from_ref(&pair),
        Lsn::new(ECHO_LSN),
        &HashSet::new(),
    )
    .expect("apply B1");
    b.apply_batch(
        std::slice::from_ref(&pair),
        Lsn::new(ECHO_LSN),
        &HashSet::new(),
    )
    .expect("apply B2");
    let b_count = b.row_count_for_test();
    let b_payload = stored_payload(&b, "tasks", "row-x");

    assert_eq!(a_count, 1, "path A: one row");
    assert_eq!(b_count, 1, "path B: one row");
    assert_eq!(
        a_count, b_count,
        "both paths converge on the same row count"
    );
    assert_eq!(
        a_payload, b_payload,
        "both paths converge on the same payload"
    );
    assert_eq!(
        &a_payload, final_payload,
        "final payload is the written one"
    );
}
