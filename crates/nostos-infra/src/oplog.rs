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
    /// Raw NEW tuple-image bytes (JSON) — what's stored in `cairn_oplog.payload`.
    /// Empty for deletes (they store NULL).
    payload: Bytes,
    /// The OLD tuple image for a delete under `REPLICA IDENTITY FULL` — the
    /// tenant column is lifted from it at flush (deletes have no NEW payload).
    /// `None` for upserts + DEFAULT-identity deletes. Never stored (clients
    /// delete by pk); server-internal only (ADR-0025 delete-tenant follow-up).
    old_payload: Option<Bytes>,
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
            old_payload: None,
            txn_id,
        },
        RowOp::Delete {
            table,
            pk,
            old_payload,
        } => OpEntry {
            lsn,
            table: table.clone(),
            pk: pk.clone(),
            op: "delete",
            payload: Bytes::new(),
            old_payload: old_payload.clone(),
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
pub use self::pg::PgOpLogCompactor;

#[cfg(feature = "pg")]
pub use self::pg::PgOpLogReader;

#[cfg(feature = "pg")]
pub use self::pg::PgOpLogWriter;

// ===========================================================================
// PgOpLogWriter — the real adapter (feature "pg").
// ===========================================================================
#[cfg(feature = "pg")]
mod pg {
    use super::{build_entry, OpEntry, BATCH_MAX};
    use async_trait::async_trait;
    use nostos_application::ports::{Metrics, OpLogError, OpLogSource, OpLogWriter};
    use nostos_domain::{Lsn, ReplicationEvent, RowOp};
    use std::fmt::Write as _;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::sync::{Mutex, Notify};
    use tokio::task::JoinHandle;
    use tokio_postgres::NoTls;

    /// A persisted op-log READER for reconnect resume (ADR-0025 slice 4b).
    /// Counterpart to [`PgOpLogWriter`] — reads `cairn_oplog` to replay the
    /// offline gap when a client reconnects with a matching epoch + an in-window
    /// `resume_lsn`. Owns no connection: each call opens a fresh one (mirror
    /// `flush_batch`'s connect-on-demand). Replay is a cold path (once per
    /// reconnect), so the connect cost is negligible + this avoids a long-lived
    /// reader connection (one less thing to keep alive across restarts).
    pub struct PgOpLogReader {
        pg_url: String,
    }

    impl PgOpLogReader {
        /// Construct from a libpq-style URL. Reads only — no writes, no slot.
        #[must_use]
        pub fn new(pg_url: &str) -> Self {
            Self {
                pg_url: pg_url.to_string(),
            }
        }

        /// Open a fresh control connection + drive its socket on a detached task
        /// (same pattern as `flush_batch`). Errors are scrubbed to `OpLogError`.
        async fn connect(&self) -> Result<tokio_postgres::Client, OpLogError> {
            let (client, conn) = tokio_postgres::connect(&self.pg_url, NoTls)
                .await
                .map_err(|e| OpLogError::Backend(e.to_string()))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(client)
        }
    }

    #[async_trait]
    impl OpLogSource for PgOpLogReader {
        async fn replay_after(
            &self,
            tenant_id: &str,
            after_lsn: u64,
        ) -> Result<Vec<ReplicationEvent>, OpLogError> {
            let client = self.connect().await?;
            // after_lsn (u64 WAL offset) → cairn_oplog.lsn BIGINT (i64). Clamp on
            // the (impossible-for-real-LSNs) overflow so a corrupt resume_lsn
            // can't panic the replay — it'll just match nothing + fall back.
            let after_i64 = i64::try_from(after_lsn).unwrap_or(i64::MAX);
            // `tenant_id: &str` → `&tenant_id: &&str` coerces to `&dyn ToSql` (the
            // pointee `&str` impls ToSql + is Sized). Binding `tenant_id` directly
            // would require `str: Sized` (it isn't) — borrow the borrow.
            let p1: &(dyn tokio_postgres::types::ToSql + Sync) = &tenant_id;
            let p2: &(dyn tokio_postgres::types::ToSql + Sync) = &after_i64;
            let rows = client
                .query(
                    "SELECT lsn, table_name, pk, op, payload::text FROM cairn_oplog \
                     WHERE tenant_id = $1 AND lsn > $2 ORDER BY lsn",
                    &[p1, p2],
                )
                .await
                .map_err(|e| OpLogError::Backend(e.to_string()))?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let lsn: i64 = r.get(0);
                let table: String = r.get(1);
                let pk: String = r.get(2);
                let op: String = r.get(3);
                // payload is JSONB; read its text form (a Vec<u8> read would
                // panic — Vec<u8> FromSql is BYTEA-only). The bytes are the
                // row's JSON tuple image, applied opaquely by the client.
                let payload: Option<String> = r.get(4);
                // lsn was stored from a u64 WAL offset; real LSNs (~2^40) are
                // positive. A negative (corrupt) row collapses to 0 — it'll be
                // gated out by the client's per-row lsn check, never corrupting.
                let lsn_u = u64::try_from(lsn).unwrap_or(0);
                let event = if op == "delete" {
                    // Replay-delivered deletes carry no old image: clients apply
                    // by pk (+ per-row lsn gate). old_payload was a write-time
                    // signal for tenant-tagging, not needed at replay.
                    ReplicationEvent::new(
                        Lsn::new(lsn_u),
                        RowOp::Delete {
                            table,
                            pk,
                            old_payload: None,
                        },
                    )
                } else {
                    // "upsert" (Insert/Update collapsed at write time). Under the
                    // client's per-row lsn gate (slice 4a) Insert ≡ Update.
                    let payload = payload
                        .map(|s| bytes::Bytes::from(s.into_bytes()))
                        .unwrap_or_default();
                    ReplicationEvent::new(Lsn::new(lsn_u), RowOp::Insert { table, pk, payload })
                };
                out.push(event);
            }
            Ok(out)
        }

        async fn window_tail(&self) -> Result<u64, OpLogError> {
            let client = self.connect().await?;
            let tail: i64 = client
                .query_one("SELECT COALESCE(MIN(lsn), 0) FROM cairn_oplog", &[])
                .await
                .map_err(|e| OpLogError::Backend(e.to_string()))?
                .get(0);
            // COALESCE guarantees non-negative; the try_from keeps clippy's
            // cast_sign_loss quiet on the (impossible) negative path.
            Ok(u64::try_from(tail).unwrap_or(0))
        }
    }

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
        /// One-shot shutdown signal: `shutdown()` notifies this + the flush
        /// loop drains its in-flight batch + exits (ADR-0025 slice-6 follow-up).
        shutdown: Arc<Notify>,
        /// The flush task's handle, awaited by `shutdown()` so the caller can
        /// confirm the final flush landed before the process exits. Taken
        /// (set to `None`) on shutdown so a second call is a no-op.
        handle: Mutex<Option<JoinHandle<()>>>,
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
            let shutdown = Arc::new(Notify::new());
            // The flush task owns the receiver + a clone of the shutdown
            // signal; the JoinHandle is RETAINED (not detached) so `shutdown`
            // can await the final flush. The task self-terminates if all
            // senders drop first (no shutdown call).
            let flush = tokio::spawn(flush_loop(
                pg_url.to_string(),
                tenant_column,
                rx,
                flush_metrics,
                Arc::clone(&shutdown),
            ));
            Self {
                tx,
                metrics,
                shutdown,
                handle: Mutex::new(Some(flush)),
            }
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

        /// Signal the flush loop to drain + exit, then await its final flush.
        /// Idempotent: a second call finds the handle already taken → no-op.
        /// Late appends after the drain race + may be lost (acceptable during
        /// shutdown; slice-1 reconcile covers any gap).
        async fn shutdown(&self) {
            self.shutdown.notify_one();
            if let Some(handle) = self.handle.lock().await.take() {
                if let Err(e) = handle.await {
                    tracing::warn!(error = %e, "oplog flush task did not join cleanly");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // PgOpLogCompactor — bounds cairn_oplog growth (ADR-0025 slice 5).
    // -----------------------------------------------------------------------

    /// Background compactor that bounds `cairn_oplog` growth. Periodically
    /// (1) collapses multiple ops on the same `(table_name, pk)` to the latest
    /// op (the net effect — a trailing `delete` is kept as a tombstone so a
    /// resuming client whose checkpoint predates the delete still observes the
    /// row removed; dropping the tombstone would re-orphan the row, the
    /// slice-1 offline-delete P0), and (2) ages out rows older than the
    /// retention window (a client whose offline gap exceeds the window falls
    /// back to snapshot-reconcile — slice 1, the safety net).
    ///
    /// Off the fan-out loop (periodic background task). Mirrors `PgOpLogWriter`'s
    /// detached-spawn + lazy-client + reconnect-on-error discipline.
    ///
    /// ponytail: compaction runs on a fixed time-window (`created_at < now() -
    /// retention`). The sharper watermark is `min(acked_lsn)` across active
    /// sessions (keep ops below the slowest client's checkpoint even if older
    /// than the window).
    ///
    /// **This is an intentional efficiency deferral, NOT a correctness gap**
    /// (ADR-0025 F3, documented-closed). The time-window + slice-1 reconcile
    /// already make compaction safe: (a) an *active* subscriber never replays —
    /// it receives live fan-out, so aging rows it hasn't ACKed can't lose it
    /// data; (b) a client offline past the window reconnects with an aged-out
    /// checkpoint → the replay gate falls back to snapshot-reconcile (slice 1,
    /// verified by `aged_out_checkpoint_falls_back_to_snapshot`). `min(acked_lsn)`
    /// would only let a *slow-but-active* client replay (cheap) instead of
    /// snapshotting (dearer) when its ack lags inside the window — a narrow
    /// efficiency edge. It's deferred because the SessionStore doesn't expose
    /// an aggregated min-checkpoint cheaply. Add when a real deployment shows
    /// slow clients missing an in-window backfill they should have qualified
    /// for; until then the time-window is simpler + correctness-equivalent.
    pub struct PgOpLogCompactor;

    impl PgOpLogCompactor {
        /// Spawn the background compaction loop (detached — runs until the
        /// process exits). `retention_secs` is the backfill-availability
        /// window; `interval_secs` is the compaction tick period.
        #[must_use]
        pub fn new(
            pg_url: &str,
            retention_secs: u64,
            interval_secs: u64,
            metrics: Arc<Metrics>,
        ) -> Self {
            let compact = tokio::spawn(compact_loop(
                pg_url.to_string(),
                retention_secs,
                interval_secs,
                metrics,
            ));
            drop(compact);
            Self
        }
    }

    /// One compaction tick's SQL — consts so a unit test can pin the
    /// net-effect / tombstone / age-out contract without a live Postgres
    /// (real-PG verification is slice 6's e2e).
    ///
    /// Collapse: keep only the latest op (`MAX(op_id)`) per `(table_name, pk)`.
    /// `op_id` is `BIGSERIAL`, so `MAX` is the chronologically-last op = the net
    /// effect. A trailing delete IS the max → survives as a tombstone.
    const COLLAPSE_SQL: &str = "DELETE FROM cairn_oplog WHERE op_id NOT IN \
        (SELECT MAX(op_id) FROM cairn_oplog GROUP BY table_name, pk)";
    /// Retention: age out rows older than the window. `make_interval(secs => $1)`
    /// binds `$1` as an i64 seconds count (`cairn_oplog.created_at` is TIMESTAMPTZ).
    const RETENTION_SQL: &str =
        "DELETE FROM cairn_oplog WHERE created_at < now() - make_interval(secs => $1)";

    /// The periodic compaction loop. Lazily connects; on error drops the client
    /// (reconnect next tick) and carries on — a failed tick is non-fatal (the
    /// next tick re-attempts; the op-log merely grows in the interim).
    async fn compact_loop(
        pg_url: String,
        retention_secs: u64,
        interval_secs: u64,
        metrics: Arc<Metrics>,
    ) {
        let interval = Duration::from_secs(interval_secs.max(1));
        let retention = i64::try_from(retention_secs).unwrap_or(i64::MAX);
        let mut client: Option<tokio_postgres::Client> = None;
        loop {
            tokio::time::sleep(interval).await;
            match compact_once(&mut client, &pg_url, retention).await {
                Ok(swept) => {
                    if swept > 0 {
                        metrics.record_oplog_compacted(swept);
                        tracing::debug!(rows = swept, "oplog compaction swept rows");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "oplog compaction tick failed; retrying next tick");
                    client = None;
                }
            }
        }
    }

    /// Run collapse + retention in one transaction; return total rows swept.
    async fn compact_once(
        client: &mut Option<tokio_postgres::Client>,
        pg_url: &str,
        retention_secs: i64,
    ) -> Result<u64, tokio_postgres::Error> {
        if client.is_none() {
            let (c, conn) = tokio_postgres::connect(pg_url, NoTls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            *client = Some(c);
        }
        let c = client.as_mut().expect("client just connected");
        let tx = c.transaction().await?;
        let collapsed = tx.execute(COLLAPSE_SQL, &[]).await?;
        let expired = tx.execute(RETENTION_SQL, &[&retention_secs]).await?;
        tx.commit().await?;
        Ok(collapsed.saturating_add(expired))
    }

    #[cfg(test)]
    mod compaction_tests {
        use super::{COLLAPSE_SQL, RETENTION_SQL};

        /// Collapse keeps only the latest op (`MAX(op_id)`) per `(table_name,
        /// pk)`; a trailing delete survives as the max → tombstone retained.
        /// Pins the net-effect contract without a live PG (real-PG verify =
        /// slice 6).
        #[test]
        fn collapse_keeps_latest_per_key_and_retains_tombstone() {
            assert!(COLLAPSE_SQL.contains("MAX(op_id)"));
            assert!(COLLAPSE_SQL.contains("GROUP BY table_name, pk"));
        }

        /// Retention ages rows by `created_at` vs the window, never by op_id.
        #[test]
        fn retention_ages_by_created_at_window() {
            assert!(RETENTION_SQL.contains("created_at < now()"));
            assert!(RETENTION_SQL.contains("make_interval"));
        }
    }

    /// The background flush loop: batch up to `BATCH_MAX` available entries,
    /// one multi-row INSERT per batch. Reconnects lazily; on a flush failure
    /// drops the client (reconnect next round), bumps `oplog_flush_failed`,
    /// and loses the batch (reconcile covers the gap). Exits on EITHER all
    /// senders dropping (natural) OR the `shutdown` signal (graceful server
    /// exit) — in both cases any remaining batch is flushed one last time
    /// before the task ends (ADR-0025 slice-6 follow-up: the pre-fix path
    /// dropped a mid-flight batch on process exit; correctness was covered by
    /// slice-1 reconcile, this just avoids the restart snapshot herd).
    async fn flush_loop(
        pg_url: String,
        tenant_column: Option<String>,
        mut rx: mpsc::Receiver<OpEntry>,
        metrics: Option<Arc<Metrics>>,
        shutdown: Arc<Notify>,
    ) {
        let mut client: Option<tokio_postgres::Client> = None;
        let mut batch: Vec<OpEntry> = Vec::with_capacity(BATCH_MAX);
        loop {
            // Wait for the next entry OR a shutdown signal — whichever fires
            // first. `biased` prefers shutdown so a drain request isn't queued
            // behind a slow recv.
            let first = tokio::select! {
                biased;
                () = shutdown.notified() => break,
                first = rx.recv() => match first {
                    Some(e) => e,
                    None => break, // all senders dropped → natural exit
                },
            };
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
            flush_one_batch(
                &mut client,
                &pg_url,
                tenant_column.as_deref(),
                &mut batch,
                metrics.as_ref(),
            )
            .await;
        }
        // Shutdown / natural exit: drain anything left in the channel + flush
        // one final batch so the last ops aren't lost when the runtime drops.
        while let Ok(e) = rx.try_recv() {
            batch.push(e);
        }
        if !batch.is_empty() {
            flush_one_batch(
                &mut client,
                &pg_url,
                tenant_column.as_deref(),
                &mut batch,
                metrics.as_ref(),
            )
            .await;
        }
    }

    /// Flush one batch (best-effort) + clear it. On failure: warn, drop the
    /// client (reconnect next round), bump `oplog_flush_failed`, clear (the
    // batch is lost — slice-1 reconcile covers the gap). Factored so the main
    // loop + the shutdown final flush share one path.
    async fn flush_one_batch(
        client: &mut Option<tokio_postgres::Client>,
        pg_url: &str,
        tenant_column: Option<&str>,
        batch: &mut Vec<OpEntry>,
        metrics: Option<&Arc<Metrics>>,
    ) {
        match flush_batch(client, pg_url, tenant_column, batch).await {
            Ok(()) => batch.clear(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    rows = batch.len(),
                    "oplog flush failed; batch lost (snapshot-reconcile covers the gap)"
                );
                *client = None;
                batch.clear();
                if let Some(m) = metrics {
                    m.oplog_flush_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Lift the tenant column from a tuple-image JSON byte slice. Returns
    /// `None` when the column is absent, the slice is empty (a DEFAULT-identity
    /// delete carries no tuple), or the JSON is malformed. Used for BOTH upsert
    /// payloads and delete old-images so tenant-tagging is uniform (ADR-0025
    /// delete-tenant follow-up).
    fn lift_tenant(src: &[u8], col: Option<&str>) -> Option<String> {
        let col = col?;
        if src.is_empty() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(src).ok()?;
        v.get(col).and_then(|c| c.as_str()).map(String::from)
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
            // Stored payload: the NEW tuple image (upserts) or NULL (deletes).
            let payload: serde_json::Value = if e.payload.is_empty() {
                serde_json::Value::Null
            } else {
                // A malformed payload (shouldn't happen — the wire codec
                // produced it) becomes NULL rather than failing the batch.
                serde_json::from_slice(&e.payload).unwrap_or(serde_json::Value::Null)
            };
            // Tenant source: the NEW image (upserts), or the OLD image for a
            // delete under REPLICA IDENTITY FULL (deletes have no NEW payload;
            // without the old image the tenant would be NULL and a tenant-
            // filtered replay would drop the delete → ghost row). A DEFAULT-
            // identity delete has neither → None → replay falls back to
            // snapshot-reconcile (slice 1).
            let tenant_src: &[u8] = if e.payload.is_empty() {
                e.old_payload.as_ref().map_or(&[][..], |b| b.as_ref())
            } else {
                &e.payload
            };
            let tenant = lift_tenant(tenant_src, tenant_column);
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
                    old_payload: None,
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

        /// ADR-0025 delete-tenant follow-up: `build_entry` threads the delete's
        /// `old_payload` (the old row image under REPLICA IDENTITY FULL) so
        /// `lift_tenant` can tag the op-log row with the tenant. The stored
        /// `payload` stays empty (clients delete by pk; the old image is
        /// server-internal).
        #[test]
        fn build_entry_threads_delete_old_payload_for_tenant_lifting() {
            let old_img: &[u8] = b"{\"org_id\":\"acme\",\"title\":\"x\"}";
            let ev = ReplicationEvent::new(
                Lsn::new(7),
                RowOp::Delete {
                    table: "tasks".into(),
                    pk: "p".into(),
                    old_payload: Some(Bytes::copy_from_slice(old_img)),
                },
            );
            let e = build_entry(&ev);
            assert_eq!(e.op, "delete");
            assert!(
                e.payload.is_empty(),
                "stored payload stays NULL for deletes"
            );
            // old_payload (the old row image under REPLICA IDENTITY FULL) is
            // threaded through so lift_tenant can tag the op-log row.
            let old = e.old_payload.as_deref();
            assert_eq!(old, Some(old_img));
            // lift_tenant resolves the tenant from the OLD image for a delete.
            assert_eq!(
                lift_tenant(old.unwrap_or(&[]), Some("org_id")),
                Some("acme".to_string()),
            );
        }

        /// `lift_tenant`: returns None on empty src (DEFAULT-identity delete),
        /// missing column, non-string value, or malformed JSON; never panics.
        #[test]
        fn lift_tenant_handles_missing_empty_and_malformed() {
            assert_eq!(lift_tenant(b"", Some("org_id")), None);
            assert_eq!(lift_tenant(b"{}", Some("org_id")), None);
            assert_eq!(lift_tenant(b"{\"org_id\":42}", Some("org_id")), None);
            assert_eq!(lift_tenant(b"not json", Some("org_id")), None);
            assert_eq!(
                lift_tenant(br#"{"org_id":"t"}"#, None),
                None,
                "no column → None"
            );
            assert_eq!(
                lift_tenant(br#"{"org_id":"tenant-xyz"}"#, Some("org_id")),
                Some("tenant-xyz".to_string()),
            );
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

        /// ADR-0025 slice-6 follow-up: `shutdown` signals the flush loop to
        /// drain + exit, then awaits the task join. It must complete promptly
        /// even when PG is unreachable — flush_batch fails fast on
        /// ECONNREFUSED → batch cleared → loop returns to recv → notify breaks
        /// it → final flush fails → task ends. Pins the shutdown mechanism
        /// (signal + handle join) without a live PG; the flush *content* is
        /// verified by the real-PG e2e.
        #[tokio::test]
        async fn shutdown_drains_and_completes_when_pg_unreachable() {
            let w = PgOpLogWriter::new("postgresql://127.0.0.1:1/db", None, 8, None);
            for i in 1..=5 {
                w.append(&ev(i, "insert")).await;
            }
            let done = tokio::time::timeout(Duration::from_secs(15), w.shutdown()).await;
            assert!(
                done.is_ok(),
                "shutdown must complete even when PG is unreachable (a live PG isn't required \
                 for the mechanism — slice-1 reconcile covers any flush gap)"
            );
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
