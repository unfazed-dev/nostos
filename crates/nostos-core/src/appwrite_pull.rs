//! Appwrite's commit-sequence cursor, shared by native and WASM clients (ADR-0050).
//!
//! A private page can contain zero visible rows while its scanned sequence
//! advances. The Supabase `PullCursor` needs a row to carry its horizon; this
//! cursor advances from an explicit `scanned_through` after applying the rows.

use serde::Deserialize;

use crate::{ApplyEngine, Frame, Operation, Storage, StorageError};

/// A malformed or unapplied Appwrite feed page.
#[derive(Debug, thiserror::Error)]
pub enum AppwritePullError {
    /// The JSON envelope or a row has invalid syntax.
    #[error("invalid Appwrite feed: {0}")]
    Decode(String),
    /// The page contradicts its cursor or committed head.
    #[error("invalid Appwrite cursor: {0}")]
    Protocol(String),
    /// Local rows did not commit.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The highest cloud sequence whose visibility has been checked and applied.
#[derive(Debug, Clone, Copy, Default)]
pub struct AppwriteCursor {
    after: u64,
}

/// Outcome of one bounded Function pull page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppwritePullOutcome {
    /// Rows committed locally.
    pub rows_applied: usize,
    /// Highest sequence scanned, including private rows hidden by the Function.
    pub scanned_through: u64,
    /// Cloud head captured before the Function scanned the page.
    pub head: u64,
    /// Whether another page is needed to reach the captured head.
    pub has_more: bool,
}

#[derive(Deserialize)]
struct Page {
    head: String,
    scanned_through: String,
    has_more: bool,
    changes: Vec<Change>,
}

#[derive(Deserialize)]
struct Change {
    seq: String,
    table: String,
    pk: String,
    op: String,
    row: Option<serde_json::Value>,
}

impl AppwriteCursor {
    /// Start at sequence zero, before any retained journal entry.
    #[must_use]
    pub const fn fresh() -> Self {
        Self { after: 0 }
    }

    /// Resume from a sequence already saved beside the local rows.
    #[must_use]
    pub const fn resume(after: u64) -> Self {
        Self { after }
    }

    /// Highest sequence durably scanned.
    #[must_use]
    pub const fn after(&self) -> u64 {
        self.after
    }

    /// Parse, validate, and apply one Function response.
    ///
    /// The Function scans all journal entries in sequence order, filtering
    /// unauthorized rows before returning `changes`. A filtered gap is valid;
    /// a response whose explicit cursor moves backwards or beyond its head is
    /// not. The cursor is persisted only after every visible row commits.
    ///
    /// # Errors
    /// Returns a decode, protocol, or storage error. The cursor stays at its
    /// previous value if an apply fails.
    pub fn apply<S: Storage>(
        &mut self,
        engine: &mut ApplyEngine<S>,
        body: &str,
    ) -> Result<AppwritePullOutcome, AppwritePullError> {
        let page: Page = serde_json::from_str(body)
            .map_err(|error| AppwritePullError::Decode(error.to_string()))?;
        let head = parse_seq(&page.head)?;
        let scanned = parse_seq(&page.scanned_through)?;
        if scanned < self.after || scanned > head || self.after > head {
            return Err(AppwritePullError::Protocol(format!(
                "after {}, scanned {scanned}, head {head}",
                self.after
            )));
        }
        if page.has_more != (scanned < head) || (page.has_more && scanned == self.after) {
            return Err(AppwritePullError::Protocol(
                "inconsistent has_more or no progress".into(),
            ));
        }
        let mut previous = self.after;
        let mut frames = Vec::with_capacity(page.changes.len());
        for change in page.changes {
            let seq = parse_seq(&change.seq)?;
            if seq <= previous || seq > scanned {
                return Err(AppwritePullError::Protocol(format!(
                    "visible sequence {seq} outside ({previous}, {scanned}]"
                )));
            }
            previous = seq;
            let op = match change.op.as_str() {
                "insert" => Operation::Insert,
                "update" => Operation::Update,
                "delete" => Operation::Delete,
                other => return Err(AppwritePullError::Protocol(format!("unknown op {other}"))),
            };
            if change.table.is_empty() || change.pk.is_empty() {
                return Err(AppwritePullError::Protocol(
                    "empty table or primary key".into(),
                ));
            }
            let payload = match (op, change.row) {
                (Operation::Insert | Operation::Update, Some(row)) => Some(
                    serde_json::to_vec(&row)
                        .map_err(|error| AppwritePullError::Decode(error.to_string()))?,
                ),
                (Operation::Delete, _) => None,
                _ => return Err(AppwritePullError::Protocol("row image missing".into())),
            };
            frames.push(Frame {
                lsn: seq,
                op,
                table: change.table,
                pk: change.pk,
                payload,
                txn_id: Some(seq),
            });
        }

        let mut rows_applied = 0;
        for frame in frames {
            if let Some(outcome) = engine.feed(frame)? {
                rows_applied += outcome.rows_applied;
            }
        }
        if let Some(outcome) = engine.flush()? {
            rows_applied += outcome.rows_applied;
        }
        if scanned != self.after {
            // Report a failed save instead of claiming a durable cursor.
            // Applied rows are idempotent if the caller retries this page.
            engine.storage_mut().save_horizon(&scanned.to_string())?;
            self.after = scanned;
        }
        Ok(AppwritePullOutcome {
            rows_applied,
            scanned_through: scanned,
            head,
            has_more: page.has_more,
        })
    }
}

fn parse_seq(value: &str) -> Result<u64, AppwritePullError> {
    value
        .parse()
        .map_err(|_| AppwritePullError::Protocol(format!("invalid decimal sequence {value:?}")))
}
