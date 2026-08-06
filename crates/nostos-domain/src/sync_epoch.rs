//! Composed sync epoch (ADR-0031 + ADR-0025).
//!
//! `Subscribe`'s epoch field must invalidate a client's resume whenever
//! *either* of two independent things changes: the replication slot's
//! lineage (ADR-0025 slice 4b's `slot_epoch`, bumped on every slot
//! (re)creation) or the active `nostos_rules.toml` scope (ADR-0031's
//! `rules_checksum`, D2). A slot recreate without a rules edit must still
//! force a resnapshot, and a rules edit without a slot recreate must too —
//! neither input alone is a safe stand-in for "has anything changed that
//! would strand this client's resume."

use crate::fnv::fnv1a_64;

/// Fold `slot_epoch` and `rules_checksum` into the single `u64` the server
/// advertises at subscribe. A change in either input changes the result.
#[must_use]
pub fn compose_sync_epoch(slot_epoch: u64, rules_checksum: u64) -> u64 {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&slot_epoch.to_le_bytes());
    bytes[8..].copy_from_slice(&rules_checksum.to_le_bytes());
    fnv1a_64(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_inputs_distinct_epochs() {
        let base = compose_sync_epoch(1, 100);

        // Vary slot_epoch alone.
        assert_ne!(base, compose_sync_epoch(2, 100));
        // Vary rules_checksum alone.
        assert_ne!(base, compose_sync_epoch(1, 200));
        // Vary both.
        assert_ne!(base, compose_sync_epoch(2, 200));
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(compose_sync_epoch(42, 999), compose_sync_epoch(42, 999));
        assert_eq!(compose_sync_epoch(0, 0), compose_sync_epoch(0, 0));
    }

    #[test]
    fn zero_checksum_is_not_identity() {
        // An old client that persisted a raw slot_epoch (pre-D2, no rules
        // checksum folded in) must not accidentally collide with a new
        // composed epoch that happens to carry a zero rules_checksum.
        assert_ne!(compose_sync_epoch(5, 0), 5);
        assert_ne!(compose_sync_epoch(0, 0), 0);
    }
}
