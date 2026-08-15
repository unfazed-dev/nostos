//! T6 attachment queue — the pure state machine (ADR-0034).
//!
//! This is the **one** shared definition of the attachment lifecycle that both
//! the Flutter (Dart) and Web (TS) driver loops bind. It is pure Rust: no
//! tokio, no async, no I/O — like the rest of `nostos-core` it compiles to WASM
//! unchanged. The actual blob transfer (calling `upload`/`download`/`delete` on
//! a platform adapter) is async + platform-specific, so it lives per-SDK; this
//! module holds only the transition invariants both drivers share. See ADR-0034
//! for the two-plane design and the ordering choice.
//!
//! ## Two-plane design (contract T6)
//!
//! - **Metadata plane — Nostos syncs it.** The [`ATTACHMENTS_TABLE`] table is a
//!   NORMAL synced table. Its `state` column carries the lifecycle below.
//!   State transitions are ordinary outbox upserts/patches — no new write
//!   path, no new wire frame, no server blob bytes.
//! - **Blob plane — the developer supplies it.** Bytes never transit the
//!   Nostos server (a moat constraint — proxying blobs would pollute fan-out
//!   throughput and make the server stateful). The SDK driver calls the app's
//!   `AttachmentStorageAdapter` (Dart/TS) when connectivity allows; on success
//!   it flips the metadata row's `state` to [`AttachmentState::Synced`] (or
//!   [`AttachmentState::Archived`] for a delete).
//!
//! ## `NOSTOS_WRITE_TABLES` — the #1 foot-gun
//!
//! The metadata table is writable through the same collapsed outbox as any
//! business table, so the server's empty-default allowlist
//! (`NOSTOS_WRITE_TABLES`, ADR-0013) MUST include `"attachments"`. A forgotten
//! entry surfaces as the transport's loud
//! _"table not writable: 'attachments' — add it to NOSTOS_WRITE_TABLES"_
//! error. Document this in the app's setup guide; the SDK READMEs do.
//!
//! ## Why the driver is per-SDK, not a Rust trait here
//!
//! An adapter trait in this crate would either pull `async` into a WASM-clean
//! crate or force a synchronous trait that misrepresents blob I/O. The contract
//! itself places `AttachmentStorageAdapter` in Dart AND TS. So the "queue lives
//! once in nostos-core" is satisfied by this state machine + the wire-string
//! contract; the driver loops that call the adapter are the ports (per-SDK).

/// The attachment metadata table name. Synced as a NORMAL table; the app
/// declares it in its schema like any other, and it MUST be in the server's
/// `NOSTOS_WRITE_TABLES` allowlist for state transitions to round-trip.
pub const ATTACHMENTS_TABLE: &str = "attachments";

/// Column names of the [`ATTACHMENTS_TABLE`] metadata table.
///
/// These are the column names the SDK drivers read/write; the app's declared
/// schema for the table MUST use exactly these names (the read-view projection
/// over `cairn_data` relies on `json_extract` hitting these keys — ADR-0028).
pub mod cols {
    /// Primary key (string). The same value is the blob's storage path/key.
    pub const ID: &str = "id";
    /// Original filename for display/download naming.
    pub const FILENAME: &str = "filename";
    /// Blob size in bytes (recorded at queue time for UX/progress).
    pub const SIZE: &str = "size";
    /// MIME media type, e.g. `image/png`. Forwarded to the adapter at upload.
    pub const MEDIA_TYPE: &str = "media_type";
    /// The lifecycle state — [`crate::attachments::AttachmentState`] wire string.
    pub const STATE: &str = "state";
    /// Client-stamped epoch (ms since unix epoch) of the last state change.
    pub const TIMESTAMP: &str = "timestamp";
}

/// Lifecycle state of an attachment's blob transfer, stored in the `state`
/// column of the synced metadata row.
///
/// Transition rules (see [`AttachmentState::on_success`] and the driver docs in
/// ADR-0034):
///
/// ```text
///   QueuedUpload   ──adapter ok──►  Synced
///   QueuedDownload ──adapter ok──►  Synced
///   QueuedDelete   ──adapter ok──►  Archived   (blob gone; metadata tombstone)
///   any Queued*    ──retries exhausted──►  Archived  (dead-letter; local error)
///   Synced | Archived  ──►  terminal (idle)
/// ```
///
/// Re-queueing an `Archived` row is an explicit app action (patch `state` back
/// to a `Queued*` value) — the driver never auto-revives a dead-letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentState {
    /// Blob bytes are local; waiting to upload to the remote bucket.
    QueuedUpload,
    /// Blob exists remotely; this client wants a local copy.
    QueuedDownload,
    /// Blob should be removed from the remote bucket (metadata retained).
    QueuedDelete,
    /// The blob has reached its target state (uploaded/downloaded/present).
    /// The active driver does nothing for a `Synced` row.
    Synced,
    /// Terminal: a successful delete (tombstone) OR a dead-lettered transfer
    /// that exhausted retries. The local error surface (`lastAttachmentError`)
    /// carries the reason; the row stays for audit until the app deletes it.
    Archived,
}

impl AttachmentState {
    /// The wire string stored in the [`cols::STATE`] column. Stable across the
    /// wire, SQLite, and both SDK drivers (the single source of truth that
    /// Dart/TS duplicate shallowly — ADR-0034).
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::QueuedUpload => "queued_upload",
            Self::QueuedDownload => "queued_download",
            Self::QueuedDelete => "queued_delete",
            Self::Synced => "synced",
            Self::Archived => "archived",
        }
    }

    /// Parse a wire string back into a state. Returns `None` for an unknown
    /// value so the driver can surface schema drift instead of panicking.
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "queued_upload" => Some(Self::QueuedUpload),
            "queued_download" => Some(Self::QueuedDownload),
            "queued_delete" => Some(Self::QueuedDelete),
            "synced" => Some(Self::Synced),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }

    /// The op a driver should attempt for this state, or `None` if the row is
    /// idle/terminal. This is the predicate the driver's tick uses to decide
    /// what to dispatch.
    #[must_use]
    pub const fn pending_op(self) -> Option<AttachmentOp> {
        match self {
            Self::QueuedUpload => Some(AttachmentOp::Upload),
            Self::QueuedDownload => Some(AttachmentOp::Download),
            Self::QueuedDelete => Some(AttachmentOp::Delete),
            Self::Synced | Self::Archived => None,
        }
    }

    /// The state that results from a SUCCESSFUL adapter call for the pending
    /// op. Terminal states ([`Self::Synced`]/[`Self::Archived`]) are returned
    /// unchanged — calling this on an idle row is harmless but the driver
    /// should not be dispatching ops for them.
    #[must_use]
    pub const fn on_success(self) -> Self {
        match self {
            Self::QueuedUpload | Self::QueuedDownload => Self::Synced,
            Self::QueuedDelete => Self::Archived,
            other => other,
        }
    }

    /// True for the `Queued*` states — rows the active driver considers
    /// actionable. Convenience over `self.pending_op().is_some()`.
    #[must_use]
    pub const fn is_queued(self) -> bool {
        matches!(
            self,
            Self::QueuedUpload | Self::QueuedDownload | Self::QueuedDelete
        )
    }
}

/// What the driver is doing with the blob for a queued row. Derived from a
/// state via [`AttachmentState::pending_op`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentOp {
    /// Push local bytes to the remote bucket (`QueuedUpload`).
    Upload,
    /// Fetch bytes from the remote bucket to local (`QueuedDownload`).
    Download,
    /// Remove the blob from the remote bucket (`QueuedDelete`).
    Delete,
}

/// Default cap on adapter retry attempts before a queued row is dead-lettered
/// to [`AttachmentState::Archived`] (ADR-0027 spirit — visible, retryable, not
/// silently dropped). The driver treats this as a floor; an app may raise it.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// Backoff schedule for a failed adapter call.
///
/// Returns `Some(delay_ms)` to retry after the backoff, or `None` once
/// `attempts >= max_attempts` (the driver then flips the row to
/// [`AttachmentState::Archived`] and surfaces the last adapter error locally).
///
/// `attempts` is the count of FAILED adapter calls so far for this row — it is
/// driver-owned LOCAL bookkeeping, NOT a synced column (retry timing/conditions
/// are per-device). Schedule: exponential, `2^attempts` seconds, capped at 60s.
///
/// # Examples
/// ```
/// use nostos_core::attachments::{retry_after_ms, DEFAULT_MAX_ATTEMPTS};
/// assert_eq!(retry_after_ms(0, DEFAULT_MAX_ATTEMPTS), Some(1_000));
/// assert_eq!(retry_after_ms(1, DEFAULT_MAX_ATTEMPTS), Some(2_000));
/// assert_eq!(retry_after_ms(2, DEFAULT_MAX_ATTEMPTS), Some(4_000));
/// // cap kicks in past 2^6 seconds:
/// assert_eq!(retry_after_ms(6, 100), Some(60_000));
/// // at the ceiling the row dead-letters:
/// assert_eq!(retry_after_ms(DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_ATTEMPTS), None);
/// ```
#[must_use]
pub fn retry_after_ms(attempts: u32, max_attempts: u32) -> Option<u64> {
    if attempts >= max_attempts {
        return None;
    }
    // Exponential backoff in whole seconds: 1, 2, 4, 8, 16, 32, then cap 60.
    // `attempts` is bounded by `max_attempts` (< u32::MAX in any sane config),
    // and we cap the shift at 6 so the value never overflows realistic use.
    let secs = 1u64 << attempts.min(6);
    Some(secs.min(60) * 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_str_roundtrip() {
        for state in [
            AttachmentState::QueuedUpload,
            AttachmentState::QueuedDownload,
            AttachmentState::QueuedDelete,
            AttachmentState::Synced,
            AttachmentState::Archived,
        ] {
            let s = state.as_wire_str();
            assert_eq!(AttachmentState::from_wire_str(s), Some(state));
        }
        assert_eq!(AttachmentState::from_wire_str("nonsense"), None);
    }

    #[test]
    fn pending_op_maps_queued_states_only() {
        assert_eq!(
            AttachmentState::QueuedUpload.pending_op(),
            Some(AttachmentOp::Upload)
        );
        assert_eq!(
            AttachmentState::QueuedDownload.pending_op(),
            Some(AttachmentOp::Download)
        );
        assert_eq!(
            AttachmentState::QueuedDelete.pending_op(),
            Some(AttachmentOp::Delete)
        );
        assert_eq!(AttachmentState::Synced.pending_op(), None);
        assert_eq!(AttachmentState::Archived.pending_op(), None);
    }

    #[test]
    fn on_success_transitions() {
        // Upload/download land at Synced.
        assert_eq!(
            AttachmentState::QueuedUpload.on_success(),
            AttachmentState::Synced
        );
        assert_eq!(
            AttachmentState::QueuedDownload.on_success(),
            AttachmentState::Synced
        );
        // A delete archives the blob but keeps the metadata as a tombstone.
        assert_eq!(
            AttachmentState::QueuedDelete.on_success(),
            AttachmentState::Archived
        );
        // Terminal states are idempotent under on_success.
        assert_eq!(
            AttachmentState::Synced.on_success(),
            AttachmentState::Synced
        );
        assert_eq!(
            AttachmentState::Archived.on_success(),
            AttachmentState::Archived
        );
    }

    #[test]
    fn is_queued_predicate() {
        assert!(AttachmentState::QueuedUpload.is_queued());
        assert!(AttachmentState::QueuedDownload.is_queued());
        assert!(AttachmentState::QueuedDelete.is_queued());
        assert!(!AttachmentState::Synced.is_queued());
        assert!(!AttachmentState::Archived.is_queued());
    }

    #[test]
    fn retry_schedule_is_exponential_then_capped_then_dead_letter() {
        let max = DEFAULT_MAX_ATTEMPTS;
        // Exponential region.
        assert_eq!(retry_after_ms(0, max), Some(1_000));
        assert_eq!(retry_after_ms(1, max), Some(2_000));
        assert_eq!(retry_after_ms(2, max), Some(4_000));
        assert_eq!(retry_after_ms(3, max), Some(8_000));
        assert_eq!(retry_after_ms(4, max), Some(16_000));
        // Cap applies once the shift would exceed 60s.
        assert_eq!(retry_after_ms(6, 100), Some(60_000));
        // At the ceiling the row dead-letters.
        assert_eq!(retry_after_ms(max, max), None);
        assert_eq!(retry_after_ms(max + 1, max), None);
        // A zero ceiling dead-letters immediately (no retries).
        assert_eq!(retry_after_ms(0, 0), None);
    }

    #[test]
    fn cols_are_stable_wire_identifiers() {
        // Regression guard: the SDK drivers + read-views key off these exact
        // strings, so a silent rename would break both Dart and TS.
        assert_eq!(cols::ID, "id");
        assert_eq!(cols::FILENAME, "filename");
        assert_eq!(cols::SIZE, "size");
        assert_eq!(cols::MEDIA_TYPE, "media_type");
        assert_eq!(cols::STATE, "state");
        assert_eq!(cols::TIMESTAMP, "timestamp");
        assert_eq!(ATTACHMENTS_TABLE, "attachments");
    }

    #[test]
    fn sdk_drivers_match_this_contract() {
        // ADR-0034 drift guard: the Dart and TS drivers duplicate this state
        // machine shallowly (per-SDK driver loops, by design). This test ties
        // their string copies to this Rust source of truth so a rename here
        // (or there) fails CI instead of silently splitting the wire contract.
        let drivers = [
            "../../sdk/nostos_flutter/lib/src/attachments.dart",
            "../../sdk/nostos_web/attachments.js",
        ];
        for rel in drivers {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let mut expected = vec![
                ATTACHMENTS_TABLE,
                cols::ID,
                cols::FILENAME,
                cols::SIZE,
                cols::MEDIA_TYPE,
                cols::STATE,
                cols::TIMESTAMP,
            ];
            expected.extend(
                [
                    AttachmentState::QueuedUpload,
                    AttachmentState::QueuedDownload,
                    AttachmentState::QueuedDelete,
                    AttachmentState::Synced,
                    AttachmentState::Archived,
                ]
                .iter()
                .map(|s| s.as_wire_str()),
            );
            for ident in expected {
                let single = format!("'{ident}'");
                let double = format!("\"{ident}\"");
                assert!(
                    src.contains(&single) || src.contains(&double),
                    "{rel} drifted from the nostos-core attachment contract: \
                     missing {ident:?} — update the driver or ADR-0034"
                );
            }
        }
    }
}
