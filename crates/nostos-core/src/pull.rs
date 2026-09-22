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

/// Default page size for one `rpc/pull` call — matches the `max_rows` default
/// on the SQL side.
pub const DEFAULT_MAX_ROWS: usize = 2000;

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

    /// One transaction produced more log rows than a whole page holds, so
    /// holding its tail back leaves nothing to apply and the pull cannot
    /// progress. The caller raises `max_rows`.
    #[error("transaction {xid} exceeds a page of {max_rows} rows — raise max_rows")]
    PageTooSmall {
        /// The oversized transaction.
        xid: String,
        /// The page size that could not hold it.
        max_rows: usize,
    },

    /// The apply did not commit. Rows before it may have; the horizon did not.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The device's pull position: where to resume from, and how much to ask for.
///
/// Request and apply share one cursor so they cannot disagree about `max_rows`.
/// They must not: the request's `max_rows` is what decides whether a page was
/// cut mid-transaction, and an apply that assumed a different number would
/// commit half a transaction.
#[derive(Debug, Clone)]
pub struct PullCursor {
    since: Horizon,
    max_rows: usize,
}

impl PullCursor {
    /// A cursor for a device with no stored horizon.
    #[must_use]
    pub fn fresh() -> Self {
        Self::resume(Horizon::fresh(), DEFAULT_MAX_ROWS)
    }

    /// A cursor resuming from a stored horizon. `max_rows == 0` is treated as
    /// [`DEFAULT_MAX_ROWS`] — a zero page can never make progress.
    #[must_use]
    pub fn resume(since: Horizon, max_rows: usize) -> Self {
        Self {
            since,
            max_rows: if max_rows == 0 {
                DEFAULT_MAX_ROWS
            } else {
                max_rows
            },
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
        serde_json::json!({ "since": self.since.as_str(), "max_rows": self.max_rows }).to_string()
    }

    /// Decode a pull response, apply it through `engine`, and advance the
    /// cursor.
    ///
    /// Rows arrive `order by xid, seq`, so each transaction is a contiguous run
    /// and [`ApplyEngine::feed`] commits at every boundary — one SQLite
    /// transaction per Postgres transaction, which is the cross-table
    /// consistency property.
    ///
    /// **A full page is assumed to be cut mid-transaction.** `limit max_rows`
    /// knows nothing about transaction boundaries, so the trailing `xid` group
    /// is held back and the cursor resumes *at* that xid to re-read it whole.
    /// The `since` bound is inclusive, which is what makes that re-read
    /// possible; on a page that was not full there is nothing to hold back and
    /// the cursor jumps to the response's horizon.
    pub fn apply<S: Storage>(
        &mut self,
        engine: &mut ApplyEngine<S>,
        body: &str,
    ) -> Result<PullOutcome, PullError> {
        let rows: Vec<PullRow> =
            serde_json::from_str(body).map_err(|e| PullError::Decode(e.to_string()))?;

        let more = rows.len() >= self.max_rows;
        // Every row carries the same horizon — one function call, one snapshot.
        let horizon = rows.first().map(|r| Horizon::new(r.horizon.clone()));

        let applicable = if more {
            // `more` implies non-empty, so the tail xid exists.
            let tail = rows[rows.len() - 1].xid.clone();
            let keep = rows.iter().take_while(|r| r.xid != tail).count();
            if keep == 0 {
                return Err(PullError::PageTooSmall {
                    xid: tail,
                    max_rows: self.max_rows,
                });
            }
            &rows[..keep]
        } else {
            &rows[..]
        };

        let mut rows_applied = 0;
        let mut checkpoint = engine.checkpoint()?;
        for row in applicable {
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
            applicable
                .last()
                .map(|r| Horizon::new(r.xid.clone()))
                // `keep > 0` above guarantees a last row.
                .or_else(|| horizon.clone())
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
    /// The page was full: call `rpc/pull` again immediately rather than waiting
    /// for a doorbell.
    pub more: bool,
}

/// One row of `cairn.pull`'s result set.
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
        assert_eq!(c.request_body(), r#"{"max_rows":2000,"since":"0"}"#);

        // The one number that must never become a JS number: past 2^53.
        let big = PullCursor::resume(Horizon::new("9007199254740995"), 10);
        assert_eq!(
            big.request_body(),
            r#"{"max_rows":10,"since":"9007199254740995"}"#
        );
    }

    #[test]
    fn zero_max_rows_falls_back_to_the_default() {
        assert_eq!(PullCursor::resume(Horizon::fresh(), 0).max_rows, 2000);
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
        assert!(!out.more);
        assert_eq!(out.horizon, Some(Horizon::new("500")));
        assert_eq!(c.since(), &Horizon::new("500"));
        assert_eq!(e.storage().row_count(), 3);
        assert_eq!(e.checkpoint().unwrap(), Lsn::new(4));
    }

    #[test]
    fn a_full_page_holds_its_trailing_transaction_back_whole() {
        let mut e = engine();
        let mut c = PullCursor::resume(Horizon::new("99"), 3);
        // xid 101's rows are cut by the limit — 101 must not land at all.
        let body = page(&[
            row("500", 1, "100", "orders", "o1", "insert"),
            row("500", 2, "100", "order_lines", "l1", "insert"),
            row("500", 3, "101", "orders", "o2", "insert"),
        ]);

        let out = c.apply(&mut e, &body).unwrap();
        assert!(out.more, "a full page is assumed cut mid-transaction");
        assert_eq!(out.rows_applied, 2);
        // Resume AT the held-back xid: `since` is inclusive, so it re-reads whole.
        assert_eq!(c.since(), &Horizon::new("100"));
        assert_eq!(e.storage().rows_for("orders").len(), 1);
    }

    #[test]
    fn a_transaction_bigger_than_a_page_is_an_error_not_a_half_apply() {
        let mut e = engine();
        let mut c = PullCursor::resume(Horizon::new("99"), 2);
        let body = page(&[
            row("500", 1, "100", "orders", "o1", "insert"),
            row("500", 2, "100", "orders", "o2", "insert"),
        ]);

        let err = c.apply(&mut e, &body).unwrap_err();
        assert!(matches!(err, PullError::PageTooSmall { max_rows: 2, .. }));
        assert_eq!(e.storage().row_count(), 0, "nothing may land");
        assert_eq!(c.since(), &Horizon::new("99"), "cursor must not move");
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
}
