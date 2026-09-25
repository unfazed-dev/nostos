//! Direct mode's pull half — pure, sync, no I/O.
//!
//! Direct mode (`docs/plans/direct-mode-sync-protocol.md`) replaces the sync
//! server with two primitives: one HTTPS POST to PostgREST's `rpc/pull`, and
//! one WebSocket carrying a contentless doorbell. This module owns everything
//! between those two — building the request body, decoding the response,
//! grouping rows into their source transactions, and advancing the horizon.
//!
//! It lives in `nostos-core` and not `nostos-client` for the reason ADR-0020
//! settled: `nostos-client` (tokio + rusqlite) and `nostos-ffi-wasm` are
//! permanently two separate consumers, so logic in either one does not reach
//! the other. Both own their own socket; neither should own this.
//!
//! ## The horizon, not the checkpoint, is the resume point
//!
//! Server mode resumes from an LSN. Direct mode resumes from an `xid8`
//! *snapshot horizon* — `pg_snapshot_xmin(pg_current_snapshot())`, the lowest
//! transaction id still in progress. Every xid below it is finished, so nothing
//! new can ever appear there: a gapless, monotonic checkpoint that no clock
//! touches.
//!
//! The `Lsn` still flows, carrying the log's `seq`, but only to drive the
//! storage's per-row `>=` gate. That gate needs `seq` to be monotonic *per pk*,
//! which it is: two transactions writing one row serialize on that row's lock,
//! so the second one appends its log row after the first commits. `seq` is
//! **not** monotonic across the whole stream (`order by xid, seq` can step
//! backwards when an older transaction committed later), so the global
//! checkpoint stalls at the highest `seq` ever seen. That is harmless here and
//! deliberate — direct mode never resumes from it.
//!
//! ## Crash safety
//!
//! [`PullCursor::apply`] saves the horizon *after* the rows commit. A crash in
//! between leaves the horizon behind the applied rows, so the next pull
//! re-reads and re-applies them — idempotent by the `apply_batch` contract. The
//! reverse order would advance past rows that never landed, which is silent
//! loss.

use serde::Deserialize;

use crate::{ApplyEngine, Frame, Storage, StorageError};
use nostos_domain::{Lsn, Operation};

/// Default page size for one `rpc/pull` call, in **transactions** — matches the
/// `max_txns` default on the SQL side.
///
/// ponytail: rows per page is bounded only by the largest transaction in it. No
/// pagination scheme can fix that — an atomic apply has to hold a whole
/// transaction regardless — so the knob is transactions and the number is a
/// round guess, not a measurement.
pub const DEFAULT_MAX_TXNS: usize = 200;

/// The floor `rpc/pull` clamps `max_txns` to, mirrored from the SQL's
/// `greatest(max_txns, 2)`.
///
/// **Two is what guarantees progress.** `since` is inclusive, so a full page's
/// cursor resumes at its own last transaction; if a page could hold only one
/// transaction, that resume point would equal the page's only transaction and
/// the next pull would return the identical page forever. At two or more, the
/// last xid is strictly above the first, which is at or above `since`.
pub const MIN_MAX_TXNS: usize = 2;

/// The `xid8` snapshot horizon, carried as an opaque string.
///
/// Opaque on purpose. The client stores it and hands it back, never doing
/// arithmetic on it, so it never has to survive a round trip through a JS
/// `number` — where anything past 2^53 is silently wrong. (The existing
/// `Lsn(u64)` derives `Serialize` and so crosses the wire as a JSON number;
/// direct mode deliberately does not copy that.)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Horizon(String);

impl Horizon {
    /// The value a device with no stored horizon sends.
    ///
    /// `xid8` comparison makes `>= 0` match the entire log, so a fresh device
    /// pulls everything still retained.
    ///
    /// ponytail: that is the whole retained log, not a snapshot. A fresh device
    /// and a device offline past the retention window want the same thing — a
    /// PostgREST table snapshot, then a horizon reset — and that path belongs
    /// to the `ChangeSource` that owns the HTTP client (plan step 3), not here.
    #[must_use]
    pub fn fresh() -> Self {
        Self("0".to_string())
    }

    /// Wrap a horizon read back from storage or from a pull response.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw text, for the request body and for persistence.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Horizon {
    fn default() -> Self {
        Self::fresh()
    }
}

/// What went wrong turning a pull response into storage writes.
#[derive(Debug, thiserror::Error)]
pub enum PullError {
    /// The response body was not the JSON array of rows `rpc/pull` returns.
    #[error("malformed pull response: {0}")]
    Decode(String),

    /// `op` was not one of `insert` / `update` / `delete`.
    #[error("unknown op {op:?} on {table}/{pk}")]
    UnknownOp {
        /// The unrecognised `op` value.
        op: String,
        /// The table the row belongs to.
        table: String,
        /// The row's primary key.
        pk: String,
    },

    /// An `xid` that will not parse as a `u64` cannot be used to group a
    /// transaction, and guessing would risk applying one half of it. Hard error.
    #[error("xid {0:?} is not a u64 — cannot group the transaction it belongs to")]
    BadXid(String),

    /// The apply did not commit. Rows before it may have; the horizon did not.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The device's pull position: where to resume from, and how much to ask for.
///
/// Request and apply share one cursor so they cannot disagree about `max_txns`.
/// They must not: the request's `max_txns` is what tells the apply whether the
/// page was full, and a page believed short is a cursor that jumps to the
/// horizon without having consumed everything below it.
#[derive(Debug, Clone)]
pub struct PullCursor {
    since: Horizon,
    max_txns: usize,
}

impl PullCursor {
    /// A cursor for a device with no stored horizon.
    #[must_use]
    pub fn fresh() -> Self {
        Self::resume(Horizon::fresh(), DEFAULT_MAX_TXNS)
    }

    /// A cursor resuming from a stored horizon. `max_txns` is clamped up to
    /// [`MIN_MAX_TXNS`], the same clamp the SQL applies.
    #[must_use]
    pub fn resume(since: Horizon, max_txns: usize) -> Self {
        Self {
            since,
            max_txns: max_txns.max(MIN_MAX_TXNS),
        }
    }

    /// Where the next pull resumes from.
    #[must_use]
    pub fn since(&self) -> &Horizon {
        &self.since
    }

    /// The JSON body for `POST /rest/v1/rpc/pull` — PostgREST takes a
    /// function's named arguments as a JSON object.
    ///
    /// The horizon goes out as a JSON *string*. What PostgREST emits for an
    /// `xid8` is a W0 check against a live project, not an assumption, which is
    /// why the decoder below accepts either form.
    #[must_use]
    pub fn request_body(&self) -> String {
        serde_json::json!({ "since": self.since.as_str(), "max_txns": self.max_txns }).to_string()
    }

    /// Decode a pull response, apply it through `engine`, and advance the
    /// cursor.
    ///
    /// Rows arrive `order by xid, seq`, so each transaction is a contiguous run
    /// and [`ApplyEngine::feed`] commits at every boundary — one SQLite
    /// transaction per Postgres transaction, which is the cross-table
    /// consistency property.
    ///
    /// **Every page is whole transactions**, which the SQL guarantees by
    /// limiting on distinct `xid` rather than on rows. So nothing is ever
    /// truncated here: a client-side truncation plus an inclusive `since`
    /// livelocks (the re-read returns the same rows, fills the page again, and
    /// cuts the same tail forever), and that is the whole reason the function
    /// pages the way it does. `nostos doctor` checks the deployed function
    /// against this contract — a row-limited `pull` would silently apply half a
    /// transaction, which no client-side check can detect.
    ///
    /// Where the cursor lands:
    ///
    /// - **Short page** → the response's horizon. Everything below it is
    ///   consumed, and the horizon's own transaction is still in flight, so the
    ///   bound stays inclusive to catch its rows next time.
    /// - **Full page** → the page's last `xid`, re-read next time and absorbed
    ///   idempotently. It cannot jump to the horizon: transactions between the
    ///   page's end and the horizon have not been seen. [`MIN_MAX_TXNS`] is
    ///   what makes this strictly forward.
    pub fn apply<S: Storage>(
        &mut self,
        engine: &mut ApplyEngine<S>,
        body: &str,
    ) -> Result<PullOutcome, PullError> {
        let rows: Vec<PullRow> =
            serde_json::from_str(body).map_err(|e| PullError::Decode(e.to_string()))?;

        // Rows come back `order by xid, seq`, so each transaction is one
        // contiguous run and counting boundaries counts transactions.
        let txns = rows.windows(2).filter(|w| w[0].xid != w[1].xid).count()
            + usize::from(!rows.is_empty());
        let more = txns >= self.max_txns;

        // Every row carries the same horizon — one function call, one snapshot.
        let horizon = rows.first().map(|r| Horizon::new(r.horizon.clone()));

        let mut rows_applied = 0;
        let mut checkpoint = engine.checkpoint()?;
        for row in &rows {
            if let Some(out) = engine.feed(row.to_frame()?)? {
                rows_applied += out.rows_applied;
                checkpoint = out.checkpoint;
            }
        }
        if let Some(out) = engine.flush()? {
            rows_applied += out.rows_applied;
            checkpoint = out.checkpoint;
        }

        // Rows are durable; only now may the horizon move past them.
        let next = if more {
            rows.last().map(|r| Horizon::new(r.xid.clone()))
        } else {
            horizon
        };
        let advanced = next.filter(|h| *h != self.since);
        if let Some(h) = &advanced {
            self.since = h.clone();
            // Non-fatal, exactly like `Storage::save_epoch`: a failure costs a
            // re-read of rows that re-apply idempotently, and must not lose a
            // commit that already landed.
            let _ = engine.storage_mut().save_horizon(h.as_str());
        }

        Ok(PullOutcome {
            rows_applied,
            checkpoint,
            horizon: advanced,
            more,
        })
    }
}

/// One row of a `nostos_snapshot()` response. `table_name` null marks the
/// horizon-only row; `pk` null marks a table header (the table is covered by
/// this snapshot, and may legitimately be empty).
#[derive(Debug, serde::Deserialize)]
struct SnapshotRow {
    horizon: String,
    table_name: Option<String>,
    pk: Option<String>,
    row: Option<serde_json::Value>,
}

impl PullCursor {
    /// Rebuild from a `nostos_snapshot()` response and resume from its horizon.
    ///
    /// This is the answer to [`crate::pull`]'s one unrecoverable error: a
    /// device offline longer than the retention window gets a 410, and no
    /// amount of pulling will ever fix it because the rows it needs are gone.
    /// Only a fresh picture of the current state can.
    ///
    /// Applied as a **snapshot window** per table, not as a stream of inserts.
    /// The difference is deletions: the snapshot carries present rows only, so
    /// a row deleted server-side while the device was away is absent rather
    /// than tombstoned, and only the window's end-of-table reap removes it.
    /// `exempt_pks` is the outbox's pending-local set — the user's own unacked
    /// writes are not in the server's picture yet and must survive the reap
    /// (ADR-0025 hole #1).
    ///
    /// # Errors
    /// [`PullError::Decode`] if the body is not a `nostos_snapshot()` array, or
    /// [`PullError::Storage`] if a table's apply does not commit. A failure
    /// part-way leaves the horizon untouched, so the next attempt re-snapshots
    /// rather than resuming from a picture that was never finished.
    pub fn apply_snapshot<S: Storage>(
        &mut self,
        engine: &mut ApplyEngine<S>,
        body: &str,
        exempt_pks: &[String],
    ) -> Result<PullOutcome, PullError> {
        let rows: Vec<SnapshotRow> =
            serde_json::from_str(body).map_err(|e| PullError::Decode(e.to_string()))?;
        let Some(horizon) = rows.first().map(|r| Horizon::new(r.horizon.clone())) else {
            return Err(PullError::Decode(
                "empty snapshot: nostos_snapshot always returns at least the horizon row".into(),
            ));
        };

        // Group by table so each window opens once. The generated SQL already
        // emits them contiguously; grouping here means a future change to that
        // ordering cannot silently reopen a window and reap live rows.
        let mut tables: Vec<String> = Vec::new();
        let mut by_table: std::collections::HashMap<String, Vec<&SnapshotRow>> =
            std::collections::HashMap::new();
        for r in &rows {
            let Some(table) = r.table_name.as_ref() else {
                continue;
            };
            if !by_table.contains_key(table) {
                tables.push(table.clone());
            }
            by_table.entry(table.clone()).or_default().push(r);
        }

        let mut rows_applied = 0;
        let mut checkpoint = engine.checkpoint()?;
        for table in &tables {
            engine.snapshot_boundary(table, true, exempt_pks)?;
            for r in by_table.get(table).into_iter().flatten() {
                let (Some(pk), Some(value)) = (r.pk.as_ref(), r.row.as_ref()) else {
                    continue; // the table header row
                };
                if let Some(out) = engine.feed(Frame {
                    // Zero, not a made-up counter: the lsn is stored per row as
                    // `applied_lsn` and gates every later write to that row. A
                    // snapshot row is the state AT the horizon, so any log row
                    // the pull delivers after it is newer and must win — at any
                    // `seq`. A counter here outran the log's `seq` and silently
                    // dropped every update to a snapshotted row (atlet,
                    // 2026-09-23). The snapshot itself still lands, because
                    // snapshot tables apply unconditionally (design D).
                    lsn: 0,
                    op: Operation::Insert,
                    table: table.clone(),
                    pk: pk.clone(),
                    payload: Some(
                        serde_json::to_vec(value).map_err(|e| PullError::Decode(e.to_string()))?,
                    ),
                    txn_id: None,
                })? {
                    rows_applied += out.rows_applied;
                    checkpoint = out.checkpoint;
                }
            }
            if let Some(out) = engine.flush()? {
                rows_applied += out.rows_applied;
                checkpoint = out.checkpoint;
            }
            // Reaps every local pk this snapshot did not re-confirm.
            engine.snapshot_boundary(table, false, &[])?;
        }

        // Only now: the picture is complete and durable.
        self.since = horizon.clone();
        let _ = engine.storage_mut().save_horizon(horizon.as_str());

        Ok(PullOutcome {
            rows_applied,
            checkpoint,
            horizon: Some(horizon),
            more: false,
        })
    }
}

impl Default for PullCursor {
    fn default() -> Self {
        Self::fresh()
    }
}

/// The result of applying one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    /// Rows committed to storage by this page.
    pub rows_applied: usize,
    /// The engine's checkpoint after the last commit (the log's `seq`; see the
    /// module docs on why this is not the resume point).
    pub checkpoint: Lsn,
    /// The cursor's new position, or `None` if it did not move — an empty
    /// response, or a page whose horizon equalled the one we asked from.
    pub horizon: Option<Horizon>,
    /// The page held `max_txns` transactions, so there is probably more log
    /// behind it: call `rpc/pull` again immediately rather than waiting for a
    /// doorbell.
    pub more: bool,
}

/// One row of `nostos.pull`'s result set.
#[derive(Debug, Deserialize)]
struct PullRow {
    #[serde(deserialize_with = "opaque_id")]
    horizon: String,
    seq: u64,
    #[serde(deserialize_with = "opaque_id")]
    xid: String,
    table_name: String,
    pk: String,
    op: String,
    /// The full row image; `null` on a delete.
    #[serde(default)]
    row: Option<serde_json::Value>,
}

impl PullRow {
    fn to_frame(&self) -> Result<Frame, PullError> {
        let op = match self.op.as_str() {
            "insert" => Operation::Insert,
            "update" => Operation::Update,
            "delete" => Operation::Delete,
            other => {
                return Err(PullError::UnknownOp {
                    op: other.to_string(),
                    table: self.table_name.clone(),
                    pk: self.pk.clone(),
                })
            }
        };
        // Grouping is the atomicity guarantee, so an unparseable xid is fatal
        // rather than a `None` that would let half a transaction through.
        let txn_id: u64 = self
            .xid
            .parse()
            .map_err(|_| PullError::BadXid(self.xid.clone()))?;
        Ok(Frame {
            lsn: self.seq,
            op,
            table: self.table_name.clone(),
            pk: self.pk.clone(),
            // The payload stays opaque bytes, same as the logical-replication
            // tuple image server mode delivers (column-level decoding is
            // ADR-0012). `serde_json`'s map is ordered, so the bytes are stable
            // and a re-apply is byte-identical.
            payload: match op {
                Operation::Delete => None,
                Operation::Insert | Operation::Update => Some(
                    serde_json::to_vec(&self.row).map_err(|e| PullError::Decode(e.to_string()))?,
                ),
            },
            txn_id: Some(txn_id),
        })
    }
}

/// Accept an id as either a JSON string or a JSON number, keep it as text.
///
/// PostgREST's rendering of `xid8` is unverified against a live project, and
/// this is the one place the ambiguity can be absorbed. Text either way, so
/// nothing downstream ever sees a number it could round.
fn opaque_id<'de, D>(de: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Num(u64),
    }
    Ok(match Raw::deserialize(de)? {
        Raw::Text(s) => s,
        Raw::Num(n) => n.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryStorage;

    /// One log row as PostgREST would render it.
    fn row(horizon: &str, seq: u64, xid: &str, table: &str, pk: &str, op: &str) -> String {
        let img = if op == "delete" {
            "null".to_string()
        } else {
            format!(r#"{{"id":"{pk}"}}"#)
        };
        format!(
            r#"{{"horizon":"{horizon}","seq":{seq},"xid":"{xid}","table_name":"{table}","pk":"{pk}","op":"{op}","row":{img}}}"#
        )
    }

    fn page(rows: &[String]) -> String {
        format!("[{}]", rows.join(","))
    }

    fn engine() -> ApplyEngine<InMemoryStorage> {
        ApplyEngine::new(InMemoryStorage::new())
    }

    #[test]
    fn request_body_sends_the_horizon_as_a_string() {
        let c = PullCursor::fresh();
        assert_eq!(c.request_body(), r#"{"max_txns":200,"since":"0"}"#);

        // The one number that must never become a JS number: past 2^53.
        let big = PullCursor::resume(Horizon::new("9007199254740995"), 10);
        assert_eq!(
            big.request_body(),
            r#"{"max_txns":10,"since":"9007199254740995"}"#
        );
    }

    #[test]
    fn max_txns_is_clamped_to_the_progress_floor() {
        // A one-transaction page would resume at its own only transaction and
        // re-read the identical page forever.
        assert_eq!(PullCursor::resume(Horizon::fresh(), 1).max_txns, 2);
        assert_eq!(PullCursor::resume(Horizon::fresh(), 0).max_txns, 2);
    }

    #[test]
    fn two_transactions_across_three_tables_apply_and_advance_the_horizon() {
        let mut e = engine();
        let mut c = PullCursor::fresh();
        let body = page(&[
            row("500", 1, "100", "orders", "o1", "insert"),
            row("500", 2, "100", "order_lines", "l1", "insert"),
            row("500", 3, "100", "audit", "a1", "insert"),
            row("500", 4, "101", "orders", "o1", "update"),
        ]);

        let out = c.apply(&mut e, &body).unwrap();
        assert_eq!(out.rows_applied, 4);
        assert!(!out.more, "2 txns is well short of the 200-txn page");
        assert_eq!(out.horizon, Some(Horizon::new("500")));
        assert_eq!(c.since(), &Horizon::new("500"));
        assert_eq!(e.storage().row_count(), 3);
        assert_eq!(e.checkpoint().unwrap(), Lsn::new(4));
    }

    #[test]
    fn a_full_page_resumes_at_its_last_transaction_not_the_horizon() {
        let mut e = engine();
        let mut c = PullCursor::resume(Horizon::new("99"), 2);
        let body = page(&[
            row("500", 1, "100", "orders", "o1", "insert"),
            row("500", 2, "100", "order_lines", "l1", "insert"),
            row("500", 3, "101", "orders", "o2", "insert"),
        ]);

        let out = c.apply(&mut e, &body).unwrap();
        assert!(out.more, "the page held max_txns transactions");
        assert_eq!(
            out.rows_applied, 3,
            "whole transactions — nothing held back"
        );
        // NOT the horizon: transactions between 101 and 500 are unseen.
        assert_eq!(c.since(), &Horizon::new("101"));
    }

    /// The livelock regression: a row-limited page plus an inclusive `since`
    /// re-reads the same truncated page forever. Three pages, each full until
    /// the last, must walk the cursor strictly forward and land on the horizon.
    #[test]
    fn paging_makes_strict_progress_and_finishes_on_the_horizon() {
        let mut e = engine();
        let mut c = PullCursor::resume(Horizon::fresh(), 2);

        let p1 = page(&[
            row("500", 1, "100", "orders", "o1", "insert"),
            row("500", 2, "101", "orders", "o2", "insert"),
        ]);
        assert!(c.apply(&mut e, &p1).unwrap().more);
        assert_eq!(c.since(), &Horizon::new("101"));

        // `since` is inclusive, so 101 comes back — and re-applies idempotently.
        let p2 = page(&[
            row("500", 2, "101", "orders", "o2", "insert"),
            row("500", 3, "102", "orders", "o3", "insert"),
        ]);
        assert!(c.apply(&mut e, &p2).unwrap().more);
        assert_eq!(c.since(), &Horizon::new("102"), "strictly forward");

        // Last page: one transaction, short of max_txns → caught up.
        let p3 = page(&[row("500", 3, "102", "orders", "o3", "insert")]);
        let out = c.apply(&mut e, &p3).unwrap();
        assert!(!out.more);
        assert_eq!(c.since(), &Horizon::new("500"));
        assert_eq!(e.storage().row_count(), 3, "the overlap did not duplicate");
    }

    #[test]
    fn a_delete_row_carries_no_payload_and_removes_the_row() {
        let mut e = engine();
        let mut c = PullCursor::fresh();
        c.apply(
            &mut e,
            &page(&[row("500", 1, "100", "orders", "o1", "insert")]),
        )
        .unwrap();
        assert_eq!(e.storage().row_count(), 1);

        c.apply(
            &mut e,
            &page(&[row("600", 2, "101", "orders", "o1", "delete")]),
        )
        .unwrap();
        assert_eq!(e.storage().row_count(), 0);
        assert_eq!(c.since(), &Horizon::new("600"));
    }

    #[test]
    fn an_empty_response_leaves_the_cursor_alone() {
        let mut e = engine();
        let mut c = PullCursor::resume(Horizon::new("400"), 10);
        let out = c.apply(&mut e, "[]").unwrap();
        assert_eq!(out.rows_applied, 0);
        assert_eq!(out.horizon, None);
        assert!(!out.more);
        assert_eq!(c.since(), &Horizon::new("400"));
    }

    #[test]
    fn ids_decode_from_json_numbers_too() {
        // The W0 hedge: PostgREST may render xid8 unquoted.
        let mut e = engine();
        let mut c = PullCursor::fresh();
        let body = r#"[{"horizon":500,"seq":1,"xid":100,"table_name":"orders","pk":"o1","op":"insert","row":{"id":"o1"}}]"#;
        let out = c.apply(&mut e, body).unwrap();
        assert_eq!(out.horizon, Some(Horizon::new("500")));
        assert_eq!(e.storage().row_count(), 1);
    }

    #[test]
    fn an_unknown_op_is_rejected() {
        let mut e = engine();
        let mut c = PullCursor::fresh();
        let body = page(&[row("500", 1, "100", "orders", "o1", "truncate")]);
        assert!(matches!(
            c.apply(&mut e, &body).unwrap_err(),
            PullError::UnknownOp { .. }
        ));
    }

    #[test]
    fn an_unparseable_xid_is_fatal() {
        let mut e = engine();
        let mut c = PullCursor::fresh();
        let body = page(&[row("500", 1, "not-an-xid", "orders", "o1", "insert")]);
        assert!(matches!(
            c.apply(&mut e, &body).unwrap_err(),
            PullError::BadXid(_)
        ));
    }

    #[test]
    fn the_horizon_is_persisted_through_storage() {
        let mut e = engine();
        let mut c = PullCursor::fresh();
        c.apply(
            &mut e,
            &page(&[row("777", 1, "100", "orders", "o1", "insert")]),
        )
        .unwrap();
        assert_eq!(e.storage().horizon().unwrap().as_deref(), Some("777"));
    }

    #[test]
    fn a_log_update_after_the_bootstrap_snapshot_is_not_gated_out() {
        // Caught live 2026-09-23 (atlet): a 1000-row snapshot stamped its rows
        // lsn 1..1024, the log's `seq` was ~60, and every status update to a
        // snapshotted order was dropped by the per-row gate — only brand-new
        // pks ever landed.
        let mut e = engine();
        let mut c = PullCursor::fresh();
        let snap = r#"[{"horizon":"500","table_name":null,"pk":null,"row":null},
            {"horizon":"500","table_name":"products","pk":null,"row":null},
            {"horizon":"500","table_name":"products","pk":"p1","row":{"id":"p1"}},
            {"horizon":"500","table_name":"products","pk":"p2","row":{"id":"p2"}},
            {"horizon":"500","table_name":"orders","pk":null,"row":null},
            {"horizon":"500","table_name":"orders","pk":"o1","row":{"status":"paid"}}]"#;
        c.apply_snapshot(&mut e, snap, &[]).unwrap();

        // seq 1: lower than any lsn the snapshot could have made up.
        let upd = r#"[{"horizon":"501","seq":1,"xid":"500","table_name":"orders","pk":"o1","op":"update","row":{"status":"failed"}}]"#;
        c.apply(&mut e, upd).unwrap();
        assert_eq!(
            e.storage().payload("orders", "o1"),
            Some(br#"{"status":"failed"}"#.as_slice())
        );
    }
}
