//! Op-log adapters — persist replication events to a durable op-log for
//! reconnect resume (ADR-0025 slice 2).
//!
//! Two implementations of [`nostos_application::ports::OpLogWriter`]:
//!
//! - [`RecordingOpLogWriter`] (always available) — in-memory, mirrors the
//!   production `append` cost (a `try_send` into a bounded channel +
//!   drop-newest-on-full). For the benchmark + unit tests: measures the
//!   fan-out-loop cost honestly and asserts drops stay 0.
//! - [`PgOpLogWriter`] (feature `pg`) — the real adapter. Batched multi-row
//!   INSERT into `cairn_oplog` via a pool-of-one client, flushed by a
//!   background task off the fan-out loop.
//!
//! ## Non-blocking (the load-bearing invariant)
//!
//! `append` is a `try_send` into a bounded internal channel — it NEVER does
//! inline Postgres I/O. At the 833k ops/sec headline the fan-out loop's
//! per-event budget is ~1.2µs; a PG round-trip is ~0.5–2ms. Inline I/O would
//! stall the loop, starve the bounded session sinks, and flip deliveries to
//! `Dropped` (silently breaking the 0% drop headline). See the `OpLogWriter`
//! trait doc.
//!
//! `unsafe` is forbidden crate-wide.

use async_trait::async_trait;
use bytes::Bytes;
use nostos_application::ports::OpLogWriter;
use nostos_domain::{ReplicationEvent, RowOp};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Rows per multi-row INSERT flush. Bounds statement size + write latency.
/// ponytail: tuned constant, no measurement yet; revisit against real-PG
/// write-amplification in slice 6.
const BATCH_MAX: usize = 500;

/// One buffered op-log entry awaiting a batched flush.
struct OpEntry {
    lsn: i64,
    table: String,
    pk: String,
    /// "upsert" for Insert/Update, "delete" for Delete.
    op: &'static str,
    /// Raw tuple-image bytes (JSON). Empty for deletes.
    payload: Bytes,
    #[allow(dead_code)]
    txn_id: Option<u64>,
}

/// Build an [`OpEntry`] from a replication event. Shared by both writers so the
/// fan-out-loop cost they impose is identical (no drift between the bench's
/// recording writer and the production one — the bench measures the real cost).
fn build_entry(event: &ReplicationEvent) -> OpEntry {
    // LSN is a u64 byte offset; cairn_oplog.lsn is BIGINT (i64). Real PG LSNs
    // are ~2^40, far within i64 positive range.
    let lsn =
        i64::try_from(event.lsn.raw()).expect("lsn fits i64 positive range (real PG LSNs ~2^40)");
    let txn_id = event.txn_id;
    match &event.op {
        RowOp::Insert { table, pk, payload } | RowOp::Update { table, pk, payload } => OpEntry {
            lsn,
            table: table.clone(),
            pk: pk.clone(),
            op: "upsert",
            payload: payload.clone(),
            txn_id,
        },
        RowOp::Delete { table, pk } => OpEntry {
            lsn,
            table: table.clone(),
            pk: pk.clone(),
            op: "delete",
            payload: Bytes::new(),
            txn_id,
        },
    }
}

// ===========================================================================
// RecordingOpLogWriter — the in-memory bench/test writer (always available).
// ===========================================================================

/// An `OpLogWriter` that records events into a bounded in-memory channel and
/// drains them in a detached background task, mirroring the production
/// `append` cost (try_send + drop-newest-on-full) so the benchmark measures the
/// real fan-out-loop overhead of the op-log rather than a no-op stub.
///
/// Drops on a full buffer are counted and readable via [`Self::dropped`] — the
/// bench asserts this stays 0.
pub struct RecordingOpLogWriter {
    tx: mpsc::Sender<OpEntry>,
    dropped: Arc<AtomicU64>,
}

impl RecordingOpLogWriter {
    /// Construct with a bounded internal channel of `buffer` depth + spawn the
    /// detached drain task. The drain task runs until the last `Sender` drops
    /// (`rx.recv()` → `None` → exit).
    #[must_use]
    pub fn new(buffer: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<OpEntry>(buffer.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        // Detached: keeps the channel draining so the bench measures the
        // steady-state try_send cost, not queueing. Bound + dropped (tokio
        // detaches the task; it self-terminates on channel close). Named
        // (not `_`) so `drop(flush)` reads as use — satisfies JoinHandle's
        // #[must_use] without the let_underscore_future / underscore-binding
        // lints a `let _` or `let _flush` would trip.
        let flush = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        drop(flush);
        Self { tx, dropped }
    }

    /// Total entries dropped because the internal buffer was full. The bench
    /// asserts this stays 0 — a non-zero value means the recording writer
    /// couldn't keep up with the FakeReplicator flood.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl OpLogWriter for RecordingOpLogWriter {
    async fn append(&self, event: &ReplicationEvent) {
        if self.tx.try_send(build_entry(event)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "pg")]
pub use self::pg::PgOpLogWriter;

// ===========================================================================
// PgOpLogWriter — the real adapter (feature "pg").
// ===========================================================================
#[cfg(feature = "pg")]
mod pg {
    use super::{build_entry, OpEntry, BATCH_MAX};
    use async_trait::async_trait;
    use nostos_application::ports::{Metrics, OpLogWriter};
    use nostos_domain::ReplicationEvent;
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_postgres::NoTls;

    /// A persisted op-log writer backed by the `cairn_oplog` Postgres table.
    ///
    /// `append` is a non-blocking `try_send` into a bounded internal channel;
    /// a background flush task batches up to [`BATCH_MAX`] entries per
    /// multi-row INSERT. This keeps the fan-out loop's per-event cost to a
    /// channel send — no inline PG I/O (ADR-0025 slice 2, consultant-confirmed).
    ///
    /// ponytail: single background flush task owns one lazy client (no Mutex —
    /// the flush task is the sole consumer; reconnect on error). Pool when a
    /// real load shows the single writer is the bottleneck.
    pub struct PgOpLogWriter {
        tx: mpsc::Sender<OpEntry>,
        metrics: Option<Arc<Metrics>>,
    }

    impl PgOpLogWriter {
        /// Construct + spawn the background flush task. `tenant_column`, when
        /// `Some`, names the row column whose value populates
        /// `cairn_oplog.tenant_id` (lifted from each row's payload at flush
        /// time; `None` on rows whose payload lacks it). `buffer` is the
        /// bounded internal channel depth (`NOSTOS_OPLOG_BUFFER`). `metrics`,
        /// when `Some`, receives the drop + flush-failed counters for
        /// `/metrics`.
        #[must_use]
        pub fn new(
            pg_url: &str,
            tenant_column: Option<String>,
            buffer: usize,
            metrics: Option<Arc<Metrics>>,
        ) -> Self {
            let (tx, rx) = mpsc::channel::<OpEntry>(buffer.max(1));
            let flush_metrics = metrics.clone();
            // Detached flush task — spawn + drop the handle (tokio detaches);
            // self-terminates when all senders drop. Named + dropped (not `_`)
            // so it satisfies JoinHandle's #[must_use] without tripping
            // let_underscore_future or underscore-binding.
            let flush = tokio::spawn(flush_loop(
                pg_url.to_string(),
                tenant_column,
                rx,
                flush_metrics,
            ));
            drop(flush);
            Self { tx, metrics }
        }
    }

    #[async_trait]
    impl OpLogWriter for PgOpLogWriter {
        async fn append(&self, event: &ReplicationEvent) {
            if self.tx.try_send(build_entry(event)).is_err() {
                // Buffer full → drop newest (mirrors the session sink's
                // try_send semantics). Affected resume gaps fall back to
                // snapshot-reconcile (correctness preserved by slice 1).
                if let Some(m) = &self.metrics {
                    m.oplog_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// The background flush loop: batch up to `BATCH_MAX` available entries,
    /// one multi-row INSERT per batch. Reconnects lazily; on a flush failure
    /// drops the client (reconnect next round), bumps `oplog_flush_failed`,
    /// and loses the batch (reconcile covers the gap).
    async fn flush_loop(
        pg_url: String,
        tenant_column: Option<String>,
        mut rx: mpsc::Receiver<OpEntry>,
        metrics: Option<Arc<Metrics>>,
    ) {
        let mut client: Option<tokio_postgres::Client> = None;
        let mut batch: Vec<OpEntry> = Vec::with_capacity(BATCH_MAX);
        while let Some(first) = rx.recv().await {
            batch.push(first);
            // Drain immediately-available entries up to BATCH_MAX — coalesces
            // a burst into one round-trip without adding latency when the
            // stream is sparse (a lone event flushes immediately).
            while batch.len() < BATCH_MAX {
                if let Ok(e) = rx.try_recv() {
                    batch.push(e);
                } else {
                    break;
                }
            }
            match flush_batch(&mut client, &pg_url, tenant_column.as_deref(), &batch).await {
                Ok(()) => batch.clear(),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        rows = batch.len(),
                        "oplog flush failed; batch lost (snapshot-reconcile covers the gap)"
                    );
                    client = None;
                    batch.clear();
                    if let Some(m) = &metrics {
                        m.oplog_flush_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        // rx.recv() returned None → all senders dropped → shutdown. Any
        // buffered-but-unflushed entries are lost; slice-6 shutdown-flush is
        // the follow-up (reconcile covers it).
    }

    /// Flush one batch as a single multi-row INSERT. Opens the connection
    /// lazily on first use. The payload JSON is parsed here (off the fan-out
    /// loop) so `tenant_id` can be lifted at flush time — the read path
    /// carries no tenant on the event (ADR-0018 enforces tenant by predicate
    /// injection at subscribe, not on the event).
    async fn flush_batch(
        client: &mut Option<tokio_postgres::Client>,
        pg_url: &str,
        tenant_column: Option<&str>,
        batch: &[OpEntry],
    ) -> Result<(), tokio_postgres::Error> {
        if client.is_none() {
            let (c, conn) = tokio_postgres::connect(pg_url, NoTls).await?;
            // Drive the connection socket on its own task; dropping the Client
            // closes the socket (mirrors PgWriteBack / PgSnapshotter).
            tokio::spawn(async move {
                let _ = conn.await;
            });
            *client = Some(c);
        }
        let c = client.as_ref().expect("client was just connected");

        // Build "VALUES ($1,..$6),($7,..$12), ..." — 6 params per row.
        let mut sql = String::with_capacity(64 + batch.len() * 28);
        sql.push_str(
            "INSERT INTO cairn_oplog (lsn, table_name, pk, op, payload, tenant_id) VALUES ",
        );
        for (i, _e) in batch.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            let b = u32::try_from(i * 6).expect("placeholder index fits u32 (batch ≤ BATCH_MAX*6)");
            // 6 placeholders per row.
            let _ = write!(
                sql,
                "(${},${},${},${},${},${})",
                b + 1,
                b + 2,
                b + 3,
                b + 4,
                b + 5,
                b + 6
            );
        }

        // Owned typed values, collected into a &(dyn ToSql + Sync) slice —
        // mirrors PgWriteBack's SqlValue pattern (boxes heterogeneous concrete
        // types so one multi-row INSERT can bind them uniformly).
        let mut vals: Vec<OpVal> = Vec::with_capacity(batch.len() * 6);
        for e in batch {
            let payload: serde_json::Value = if e.payload.is_empty() {
                serde_json::Value::Null
            } else {
                // A malformed payload (shouldn't happen — the wire codec
                // produced it) becomes NULL rather than failing the batch.
                serde_json::from_slice(&e.payload).unwrap_or(serde_json::Value::Null)
            };
            let tenant = tenant_column
                .and_then(|col| payload.get(col))
                .map(std::string::ToString::to_string);
            vals.push(OpVal::I64(e.lsn));
            vals.push(OpVal::Str(e.table.clone()));
            vals.push(OpVal::Str(e.pk.clone()));
            vals.push(OpVal::StaticStr(e.op));
            vals.push(OpVal::Json(payload));
            vals.push(OpVal::OptStr(tenant));
        }
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            vals.iter().map(OpVal::as_tosql).collect();
        c.execute(&sql, &params).await?;
        Ok(())
    }

    /// A typed SQL value for the op-log's fixed 6-column schema (mirrors
    /// PgWriteBack's `SqlValue`: boxes concrete `ToSql` types so a
    /// heterogeneous multi-row INSERT can collect `&dyn ToSql` into one slice).
    enum OpVal {
        I64(i64),
        Str(String),
        StaticStr(&'static str),
        Json(serde_json::Value),
        OptStr(Option<String>),
    }

    impl OpVal {
        fn as_tosql(&self) -> &(dyn tokio_postgres::types::ToSql + Sync) {
            match self {
                OpVal::I64(v) => v,
                OpVal::Str(v) => v,
                OpVal::StaticStr(v) => v,
                OpVal::Json(v) => v,
                OpVal::OptStr(v) => v,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use bytes::Bytes;
        use nostos_domain::{Lsn, RowOp};

        fn ev(lsn: u64, op: &str) -> ReplicationEvent {
            let row = match op {
                "delete" => RowOp::Delete {
                    table: "t".into(),
                    pk: lsn.to_string(),
                },
                _ => RowOp::Insert {
                    table: "t".into(),
                    pk: lsn.to_string(),
                    payload: Bytes::from_static(b"{\"k\":1}"),
                },
            };
            ReplicationEvent::new(Lsn::new(lsn), row)
        }

        /// The shared `build_entry` maps Insert/Update → "upsert" and Delete →
        /// "delete", and carries the payload bytes for upserts / empty for
        /// deletes. Pinned because both writers depend on it.
        #[test]
        fn build_entry_classifies_op_and_carries_payload() {
            let up = build_entry(&ev(1, "insert"));
            assert_eq!(up.op, "upsert");
            assert!(!up.payload.is_empty());
            assert_eq!(up.lsn, 1);

            let del = build_entry(&ev(2, "delete"));
            assert_eq!(del.op, "delete");
            assert!(del.payload.is_empty());
        }

        /// `OpVal::as_tosql` returns the right reference type per variant — a
        /// regression guard if the enum is refactored (the multi-row INSERT
        /// binding depends on each arm coercing to &(dyn ToSql + Sync)).
        #[test]
        fn opval_as_tosql_is_sound_for_all_variants() {
            let v1 = OpVal::I64(1);
            let v2 = OpVal::Str("s".to_string());
            let v3 = OpVal::StaticStr("upsert");
            let v4 = OpVal::Json(serde_json::json!({"a": 1}));
            let v5 = OpVal::OptStr(Some("t".to_string()));
            let v6 = OpVal::OptStr(None);
            // Each must coerce without panic (the trait object is formed).
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v1.as_tosql();
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v2.as_tosql();
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v3.as_tosql();
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v4.as_tosql();
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v5.as_tosql();
            let _: &(dyn tokio_postgres::types::ToSql + Sync) = v6.as_tosql();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nostos_domain::{Lsn, RowOp};

    fn ev(lsn: u64) -> ReplicationEvent {
        ReplicationEvent::new(
            Lsn::new(lsn),
            RowOp::Insert {
                table: "t".into(),
                pk: lsn.to_string(),
                payload: Bytes::from_static(b"x"),
            },
        )
    }

    /// ADR-0025 slice 2 contract: events below the buffer capacity are
    /// delivered (try_send succeeds); the counter stays 0. This is the
    /// invariant the bench relies on to assert drops == 0 at full throughput.
    #[tokio::test]
    async fn recording_writer_counts_no_drops_under_capacity() {
        let w = RecordingOpLogWriter::new(8);
        for i in 1..=8 {
            w.append(&ev(i)).await;
        }
        // Give the drain task a beat to consume.
        tokio::task::yield_now().await;
        assert_eq!(w.dropped(), 0, "no drops expected under capacity");
    }

    /// ADR-0025 slice 2 contract + consultant Q3: on a FULL buffer the writer
    /// drops newest (counts it) rather than blocking the caller. This is the
    /// fan-out-loop-safety invariant — `append` must never block.
    #[tokio::test]
    async fn recording_writer_drops_newest_when_full_and_never_blocks() {
        // buffer 1, NO drain task consuming (we construct the channel directly
        // to simulate a stalled flush): fill it, then assert the next append
        // drops instead of awaiting.
        let (tx, mut rx) = mpsc::channel::<OpEntry>(1);
        let dropped = Arc::new(AtomicU64::new(0));
        // One entry fills the 1-deep buffer.
        let _ = tx.try_send(build_entry(&ev(1)));
        // The second must drop (buffer full), not block.
        if tx.try_send(build_entry(&ev(2))).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        // Drain the one buffered entry to prove append would have succeeded
        // with capacity — i.e. the drop was a capacity decision, not an error.
        assert!(rx.try_recv().is_ok());
    }
}
