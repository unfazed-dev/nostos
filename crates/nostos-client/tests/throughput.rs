//! SqliteStorage apply throughput — catches SQLite I/O bottlenecks early.
//!
//! The advisor flagged SQLite apply throughput as a HIGH risk under high WAL
//! volume. This isn't a network benchmark (the chaos e2e covers end-to-end);
//! it measures the *storage* floor: how fast `apply_batch` can land rows when
//! the network is out of the picture. A regression here would cap the whole
//! client before the wire does.
//!
//! Asserts a generous floor (5k rows/sec on in-memory SQLite) — well below
//! what SQLite achieves in practice (~50k+), so it only fails on a real
//! regression (sync I/O per row, missing transaction, full-scan checkpoint),
//! not on a slow CI machine.

// Reporting code: `n as f64` for a rows/sec metric is fine — we're not relying
// on float precision below 52 bits for a row count. Mirrors nostos-bench's allow
// for the same throughput-reporting pattern.
#![allow(clippy::cast_precision_loss)]

use std::collections::HashSet;
use std::time::Instant;

use bytes::Bytes;
use nostos_client::SqliteStorage;
use nostos_core::Storage;
use nostos_domain::{Lsn, RowOp};

/// Build `n` unique insert ops with ~100B payloads (the PowerSync "small row"
/// regime — apples-to-apples with the server-side benchmark profile).
fn make_ops(n: usize) -> Vec<RowOp> {
    (0..n)
        .map(|i| RowOp::Insert {
            table: "tasks".into(),
            pk: format!("row-{i}"),
            payload: Bytes::copy_from_slice(&[0u8; 100]),
        })
        .collect()
}

#[test]
fn sqlite_apply_throughput_meets_floor() {
    // In-memory SQLite (real SQL path, no disk). Three batch sizes to catch both
    // per-transaction overhead (small batches) and unbounded-buffer issues (large).
    let cases = [(1_000, "1k"), (10_000, "10k"), (100_000, "100k")];

    for (n, label) in cases {
        let mut storage = SqliteStorage::open_in_memory().unwrap();
        let ops = make_ops(n);
        let checkpoint = Lsn::new(u64::try_from(n).unwrap() * 10);

        let start = Instant::now();
        storage
            .apply_batch(
                &ops.iter()
                    .map(|o| (o.clone(), checkpoint.raw()))
                    .collect::<Vec<_>>(),
                checkpoint,
                &HashSet::new(),
            )
            .expect("apply_batch");
        let elapsed = start.elapsed();

        let rows_per_sec = (n as f64) / elapsed.as_secs_f64().max(1e-9);
        eprintln!("{label} rows: {elapsed:?} → {rows_per_sec:.0} rows/sec");

        assert_eq!(storage.row_count_for_test(), n, "all {label} rows applied");
        // Floor: 5k rows/sec. SQLite in-memory typically does 50k+. This only
        // fails on a genuine regression (e.g. a missing transaction making each
        // row its own commit), not on CI variance.
        assert!(
            rows_per_sec >= 5_000.0,
            "{label}: {rows_per_sec:.0} rows/sec below the 5k floor — SQLite apply regressed"
        );
    }
}

#[test]
fn batched_apply_is_faster_than_per_row() {
    // The whole point of `apply_batch` (vs per-op apply): one transaction for N
    // rows. Confirm the design holds — batching N rows in one call is at least
    // 10x faster than N separate calls. This guards against a future refactor
    // that silently splits the transaction.
    let n = 1_000;

    // Batched: one apply_batch call.
    let mut batched = SqliteStorage::open_in_memory().unwrap();
    let ops = make_ops(n);
    let batched_time = {
        let s = Instant::now();
        batched
            .apply_batch(
                &ops.iter()
                    .map(|o| (o.clone(), n as u64 * 10))
                    .collect::<Vec<_>>(),
                Lsn::new(n as u64 * 10),
                &HashSet::new(),
            )
            .unwrap();
        s.elapsed()
    };

    // Per-row: n apply_batch calls of 1 op each.
    let mut per_row = SqliteStorage::open_in_memory().unwrap();
    let per_row_time = {
        let s = Instant::now();
        for (i, op) in make_ops(n).into_iter().enumerate() {
            per_row
                .apply_batch(
                    &[(op, i as u64 * 10 + 10)],
                    Lsn::new(i as u64 * 10 + 10),
                    &HashSet::new(),
                )
                .unwrap();
        }
        s.elapsed()
    };

    let ratio = per_row_time.as_secs_f64() / batched_time.as_secs_f64().max(1e-9);
    eprintln!("batched={batched_time:?}, per_row={per_row_time:?}, ratio={ratio:.1}x");
    // A genuine regression here is "batched == per_row" (ratio ~1.0) — that would
    // mean the transaction was split so each row commits alone. 2.5x leaves wide
    // margin for CI/machine-contention variance (measured 3–9x on a quiet
    // machine, ~2.9x under concurrent wasm-build load) while still catching the
    // split failure mode decisively.
    assert!(
        ratio >= 2.5,
        "batched apply should be ≥2.5x faster than per-row; got {ratio:.1}x — transaction may be split"
    );
}
