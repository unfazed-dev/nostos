//! Conflict-free merge types — the CRDT tier (ADR-0030).
//!
//! nostos is server-authoritative and converges server-delivered frames by
//! per-row LSN-gated last-writer-wins (ADR-0025 slice 4a). That LSN-LWW is
//! enough for rows a single principal edits, and for server-serialized writes.
//! It is NOT enough for one shape: a row holding a **multi-element set** that a
//! client edits **optimistically while offline**, where a remote edit to the
//! same row is in flight. There the pending-replay's full-row upsert would
//! clobber the remote element (the classic lost-update on a collection). An
//! add-wins OR-set CRDT merges element-wise instead, so an offline add of `x`
//! and a remote add of `y` converge to `{x,y}` rather than one losing the
//! other.
//!
//! This module is the pure algebra: a hybrid logical clock ([`Hlc`]) and the
//! add-wins OR-set merge over opaque payload bytes. It is deliberately
//! clock-free — [`Hlc::mint`] takes `now_wall_ms` as a parameter so the domain
//! stays pure and testable; the caller (client for optimistic edits, server at
//! write-back commit — ADR-0030 Decision 4, relaxed to client+server minting
//! since HLC needs no clock sync) supplies wall time and threads the previous
//! HLC as its monotone state.
//!
//! `unsafe` is forbidden crate-wide; this module adds none.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// A hybrid logical clock timestamp — `wall_ms` (wall clock, milliseconds) plus
/// a logical `ctr` to break same-millisecond ties. Totally ordered lexicographically
/// (`wall_ms`, then `ctr`), so any two HLCs compare — which is what makes the
/// OR-set merge a deterministic per-element max rather than a partial-order
/// conflict.
///
/// O(1) — 12 bytes — vs a version vector's O(clients)/frame. The cost of total
/// order is that "concurrent" operations (same wall, adjacent ctr) resolve by
/// mint order, i.e. last-mint-wins among ties; for nostos that is acceptable
/// (the server serializes real conflicts; the HLC only arbitrates the
/// offline-optimistic merge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hlc {
    /// Wall-clock milliseconds at mint time.
    pub wall_ms: u64,
    /// Logical counter — disambiguates events within one wall millisecond.
    pub ctr: u32,
}

impl Hlc {
    /// The zero HLC — "never minted." Any real HLC is strictly greater (the
    /// OR-set uses it as the absent-remove sentinel's lower bound).
    pub const ZERO: Hlc = Hlc { wall_ms: 0, ctr: 0 };

    /// Mint the next HLC after `prev`, given the current wall clock. This is
    /// the standard HLC "local/send" rule (Kulkarni 2014): the new wall is the
    /// max of the real clock and the previous wall; the counter advances only
    /// when wall time did not (else it resets). The result is strictly greater
    /// than `prev`, preserving monotonicity for the caller's HLC state.
    ///
    /// Counter overflow (≈4×10⁹ events in one millisecond) bumps `wall_ms` and
    /// resets `ctr` — the HLC overflow rule, keeping the result monotone without
    /// panicking. Real workloads never hit it.
    #[must_use]
    pub fn mint(prev: Option<Hlc>, now_wall_ms: u64) -> Hlc {
        let Some(p) = prev else {
            return Hlc {
                wall_ms: now_wall_ms,
                ctr: 0,
            };
        };
        let wall_ms = now_wall_ms.max(p.wall_ms);
        if wall_ms == p.wall_ms {
            match p.ctr.checked_add(1) {
                Some(ctr) => Hlc { wall_ms, ctr },
                // Counter saturated this ms — advance the wall to stay monotone.
                None => Hlc {
                    wall_ms: wall_ms.checked_add(1).expect("Hlc wall_ms overflow"),
                    ctr: 0,
                },
            }
        } else {
            Hlc { wall_ms, ctr: 0 }
        }
    }

    /// The lexicographically larger of two HLCs — the per-element "winner" in
    /// the OR-set merge. Manual compare (mirrors `Lsn::min`): `<u64 as Ord>::cmp`
    /// is not const-stable, so a const `max` can't call it.
    #[must_use]
    pub const fn max(self, other: Hlc) -> Hlc {
        if self.wall_ms > other.wall_ms || (self.wall_ms == other.wall_ms && self.ctr >= other.ctr)
        {
            self
        } else {
            other
        }
    }
}

impl PartialOrd for Hlc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hlc {
    fn cmp(&self, other: &Self) -> Ordering {
        self.wall_ms
            .cmp(&other.wall_ms)
            .then(self.ctr.cmp(&other.ctr))
    }
}

impl Default for Hlc {
    fn default() -> Self {
        Self::ZERO
    }
}

/// One element of an add-wins OR-set. `h` is the add-HLC (when `v` was added,
/// or the latest re-add); `d` is the remove-HLC tombstone (`None` = never
/// removed). The element is present iff `h > d`.
///
/// `v` is a `String` — element values in the pomodoro fixture (presence =
/// user-ids, community tags) are bare strings. ponytail: generalize to a JSON
/// value if a structured element ever appears; none does today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrSetElement {
    /// The element value (a tag, a user-id, …).
    pub v: String,
    /// Add-HLC — the timestamp of the latest add of `v`.
    pub h: Hlc,
    /// Remove-HLC tombstone — `None` means `v` has not been removed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub d: Option<Hlc>,
}

impl OrSetElement {
    /// Whether this element is present: an add with no tombstone, or whose
    /// add-HLC beats its remove-HLC (add-wins — a re-add after a remove mints a
    /// newer `h` and re-activates the element).
    #[must_use]
    pub fn is_present(&self) -> bool {
        match self.d {
            None => true,
            Some(dval) => self.h > dval,
        }
    }
}

/// The on-the-wire OR-set payload shape: `{"elements":[{"v":…,"h":…,"d":…}]}`.
/// Serialized as the row's opaque payload bytes — so `WireFrame` is unchanged
/// (the HLCs live inside the blob, ADR-0030's moat-safe claim).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrSetPayload {
    /// The element set. Order is not semantically meaningful; merge serializes
    /// sorted by `v` for deterministic bytes.
    #[serde(default)]
    pub elements: Vec<OrSetElement>,
}

/// Why an OR-set payload operation failed. The merge is best-effort at the
/// storage seam: a malformed payload falls back to LWW (the caller decides),
/// so this is a signal, not a panic.
#[derive(Debug, thiserror::Error)]
pub enum OrSetError {
    /// The payload was not valid JSON.
    #[error("or-set payload is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// The payload was valid JSON but not an object with an `elements` array.
    #[error("or-set payload has the wrong shape (expected {{\"elements\":[…]}})")]
    Malformed,
}

/// Parse an OR-set payload into its elements. Empty/`null` bytes parse to an
/// empty set (a row with no elements yet) — only non-object JSON is malformed.
fn parse_elements(bytes: &[u8]) -> Result<Vec<OrSetElement>, OrSetError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Object(_) => {
            let payload: OrSetPayload = serde_json::from_value(value)?;
            Ok(payload.elements)
        }
        _ => Err(OrSetError::Malformed),
    }
}

/// Serialize elements as the canonical OR-set payload, sorted by `v` so two
/// merges of the same set produce byte-identical output (idempotence).
fn serialize_elements(elements: &[OrSetElement]) -> Vec<u8> {
    let mut sorted: Vec<OrSetElement> = elements.to_vec();
    sorted.sort_by(|a, b| a.v.cmp(&b.v));
    // `to_vec` on a Vec<OrSetElement> is infallible (no map types, no cycles).
    serde_json::to_vec(&OrSetPayload { elements: sorted })
        .expect("serializing OrSetPayload is infallible")
}

/// Merge two OR-set payloads into one, add-wins.
///
/// For each distinct value `v` across both payloads, the merged element keeps
/// the maximum add-HLC and the maximum remove-HLC. This is the classic add-wins
/// OR-set merge (state-based, convergent): idempotent, commutative, associative.
/// An offline add of `x` (HLC `hx`) and a remote add of `y` (HLC `hy`) merge to
/// a row present-set of `{x,y}`; a remove of `x` only takes effect if its
/// tombstone-HLC beats the latest add-HLC of `x`.
///
/// Returns the merged payload bytes. On a malformed payload the caller should
/// fall back to LWW (treat the row as opaque) — the merge is opt-in per
/// OR-set-tagged table.
///
/// # Errors
/// [`OrSetError::InvalidJson`] if either payload is not valid JSON, or
/// [`OrSetError::Malformed`] if either is valid JSON but not an object.
pub fn merge_or_set_payloads(a: &[u8], b: &[u8]) -> Result<Vec<u8>, OrSetError> {
    let a_elems = parse_elements(a)?;
    let b_elems = parse_elements(b)?;

    // Key by value: keep the max add-HLC and max remove-HLC per element.
    let mut merged: std::collections::BTreeMap<String, (Hlc, Option<Hlc>)> =
        std::collections::BTreeMap::new();
    for el in a_elems.iter().chain(b_elems.iter()) {
        match merged.get_mut(&el.v) {
            Some((add, rmv)) => {
                *add = (*add).max(el.h);
                *rmv = max_remove(*rmv, el.d);
            }
            None => {
                merged.insert(el.v.clone(), (el.h, el.d));
            }
        }
    }

    let elements: Vec<OrSetElement> = merged
        .into_iter()
        .map(|(v, (h, d))| OrSetElement { v, h, d })
        .collect();
    Ok(serialize_elements(&elements))
}

/// Merge two OR-set payloads, falling back to `incoming` (a plain LWW clobber)
/// if either side is malformed. This is the infallible seam the storage apply
/// loop calls: a row whose payload isn't a valid OR-set image degrades to LWW
/// rather than erroring the whole apply batch. The merge itself is
/// [`merge_or_set_payloads`]; only the error handling is added here.
#[must_use]
pub fn merge_or_set_or_lww(existing: &[u8], incoming: &[u8]) -> Vec<u8> {
    merge_or_set_payloads(existing, incoming).unwrap_or_else(|_| incoming.to_vec())
}

/// The present (non-tombstoned) element values in an OR-set payload — what a
/// view renders. `add-hlc > remove-hlc` per element; an absent tombstone means
/// present.
///
/// # Errors
/// [`OrSetError`] if the payload is malformed.
pub fn present_elements(bytes: &[u8]) -> Result<Vec<String>, OrSetError> {
    Ok(parse_elements(bytes)?
        .into_iter()
        .filter(OrSetElement::is_present)
        .map(|e| e.v)
        .collect())
}

/// Max of two optional remove-HLCs: `None` if both are `None`, else the larger.
fn max_remove(a: Option<Hlc>, b: Option<Hlc>) -> Option<Hlc> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.max(y)),
    }
}

// ─────────────────────── PN-Counter (ADR-0030 Counter B) ───────────────────────
//
// A state-based Positive-Negative Counter CRDT — the counter mirror of the
// OR-set above. Each replica `r` maintains a pair `(p, n)` (positive and
// negative counts); the counter's value is Σp − Σn across all replicas. Merge
// is per-replica elementwise max: commutative, associative, idempotent.
//
// Unlike the OR-set (append-only — each element is independent, so the client
// enqueues a single-element payload without reading state), a counter's
// increments are CUMULATIVE per replica. Enqueuing `{r:R, p:3}` then `{r:R,
// p:2}` gives `max(3,2)=3` on merge — the second increment is lost. So the
// client MUST read-modify-write: read the current payload, add the delta to
// THIS replica's entry, and enqueue the full result. The per-replica max merge
// still converges across replicas (a concurrent increment from replica S
// touches a different key and survives). This is the crux of a state-based
// PN-counter — not a flaw, the defining property (advisor-confirmed).

/// One replica's contribution to a PN-counter: `p` (increments) and `n`
/// (decrements), both monotonically non-decreasing. The counter value sums
/// `p − n` across all replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnEntry {
    /// The replica/client id (stable per client — see `SyncClientConfig::client_id`).
    pub r: String,
    /// Positive count (total increments by this replica).
    pub p: u64,
    /// Negative count (total decrements by this replica).
    pub n: u64,
}

/// The on-the-wire PN-counter payload shape: `{"entries":[{"r":…,"p":…,"n":…}]}`.
/// Serialized as the row's opaque payload bytes (same seam as `OrSetPayload`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PnCounterPayload {
    /// Per-replica counts. Order is not semantically meaningful; merge
    /// serializes sorted by `r` for deterministic bytes.
    #[serde(default)]
    pub entries: Vec<PnEntry>,
}

/// Why a counter payload operation failed. Same best-effort contract as
/// [`OrSetError`]: malformed payloads fall back to LWW.
#[derive(Debug, thiserror::Error)]
pub enum CounterError {
    /// The payload was not valid JSON.
    #[error("counter payload is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// The payload was valid JSON but not an object with an `entries` array.
    #[error("counter payload has the wrong shape (expected {{\"entries\":[…]}})")]
    Malformed,
}

/// Parse a counter payload into its entries. Empty/`null` bytes parse to an
/// empty set (a counter with no increments yet).
fn parse_counter_entries(bytes: &[u8]) -> Result<Vec<PnEntry>, CounterError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Object(_) => {
            let payload: PnCounterPayload = serde_json::from_value(value)?;
            Ok(payload.entries)
        }
        _ => Err(CounterError::Malformed),
    }
}

/// Serialize entries as the canonical counter payload, sorted by `r` so two
/// merges of the same state produce byte-identical output (idempotence).
fn serialize_counter_entries(entries: &[PnEntry]) -> Vec<u8> {
    let mut sorted: Vec<PnEntry> = entries.to_vec();
    sorted.sort_by(|a, b| a.r.cmp(&b.r));
    serde_json::to_vec(&PnCounterPayload { entries: sorted })
        .expect("serializing PnCounterPayload is infallible")
}

/// Merge two counter payloads into one, per-replica elementwise max.
///
/// For each replica `r` across both payloads, the merged entry keeps the
/// maximum `p` and maximum `n`. This is the classic PN-counter merge
/// (state-based, convergent): idempotent, commutative, associative.
///
/// Returns the merged payload bytes. On a malformed payload the caller should
/// fall back to LWW — the merge is opt-in per counter-tagged table.
///
/// # Errors
/// [`CounterError::InvalidJson`] if either payload is not valid JSON, or
/// [`CounterError::Malformed`] if either is valid JSON but not an object.
pub fn merge_counter_payloads(a: &[u8], b: &[u8]) -> Result<Vec<u8>, CounterError> {
    let a_entries = parse_counter_entries(a)?;
    let b_entries = parse_counter_entries(b)?;

    // Key by replica: keep the max p and max n per replica.
    let mut merged: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    for e in a_entries.iter().chain(b_entries.iter()) {
        match merged.get_mut(&e.r) {
            Some((p, n)) => {
                *p = (*p).max(e.p);
                *n = (*n).max(e.n);
            }
            None => {
                merged.insert(e.r.clone(), (e.p, e.n));
            }
        }
    }

    let entries: Vec<PnEntry> = merged
        .into_iter()
        .map(|(r, (p, n))| PnEntry { r, p, n })
        .collect();
    Ok(serialize_counter_entries(&entries))
}

/// Merge two counter payloads, falling back to `incoming` (LWW clobber) if
/// either side is malformed. The infallible seam the storage apply loop calls.
#[must_use]
pub fn merge_counter_or_lww(existing: &[u8], incoming: &[u8]) -> Vec<u8> {
    merge_counter_payloads(existing, incoming).unwrap_or_else(|_| incoming.to_vec())
}

/// The current counter value: Σp − Σn across all replicas.
///
/// # Errors
/// [`CounterError`] if the payload is malformed.
pub fn counter_value(bytes: &[u8]) -> Result<i64, CounterError> {
    let entries = parse_counter_entries(bytes)?;
    let p_sum: u64 = entries.iter().map(|e| e.p).sum();
    let n_sum: u64 = entries.iter().map(|e| e.n).sum();
    // Both sums fit in i64 for any realistic counter (2⁶³ increments per replica
    // is unreachable). Saturating on the absurd case.
    let p = i64::try_from(p_sum).unwrap_or(i64::MAX);
    let n = i64::try_from(n_sum).unwrap_or(i64::MAX);
    Ok(p.saturating_sub(n))
}

/// Read-modify-write at the payload level: add `delta` to replica `replica`'s
/// entry in the counter. A positive delta bumps `p`; a negative delta bumps
/// `n` by `|delta|`. A zero delta is a no-op (returns the parsed payload
/// unchanged). Malformed bytes are treated as an empty counter (the replica
/// starts fresh — losing a corrupt payload is better than erroring the write).
///
/// This is the client-side primitive `counter_increment` / `counter_decrement`
/// call before enqueuing the result as a merge-upsert. The per-replica max
/// merge on apply converges across replicas; the read-modify-write ensures
/// this replica's cumulative count survives repeated increments.
#[must_use]
pub fn counter_apply_delta(bytes: &[u8], replica: &str, delta: i64) -> Vec<u8> {
    let mut entries = parse_counter_entries(bytes).unwrap_or_default();
    // `position` (not `find`) so the mutable borrow ends before the else-push —
    // the index is a Copy, not a borrow of `entries`.
    if let Some(idx) = entries.iter().position(|e| e.r == replica) {
        if delta >= 0 {
            entries[idx].p = entries[idx]
                .p
                .saturating_add(u64::try_from(delta).unwrap_or(u64::MAX));
        } else {
            entries[idx].n = entries[idx].n.saturating_add(delta.unsigned_abs());
        }
    } else {
        let (p, n) = if delta >= 0 {
            (u64::try_from(delta).unwrap_or(u64::MAX), 0)
        } else {
            (0, delta.unsigned_abs())
        };
        entries.push(PnEntry {
            r: replica.to_string(),
            p,
            n,
        });
    }
    serialize_counter_entries(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(wall: u64, ctr: u32) -> Hlc {
        Hlc { wall_ms: wall, ctr }
    }

    fn payload(elems: &[OrSetElement]) -> Vec<u8> {
        serialize_elements(elems)
    }

    fn el(v: &str, h: Hlc, d: Option<Hlc>) -> OrSetElement {
        OrSetElement { v: v.into(), h, d }
    }

    // --- Hlc ---

    #[test]
    fn mint_is_strictly_monotone() {
        // `mint(Some(prev), _)` always strictly exceeds `prev` (ctr bumps when
        // wall is unchanged, wall advances otherwise). Seed `last` with the
        // first mint, then every subsequent mint must be strictly greater.
        let mut prev: Option<Hlc> = None;
        let mut last = Hlc::mint(prev, 1);
        prev = Some(last);
        for now in [1u64, 1, 5, 5, 5, 10, 9, 10, 3] {
            let next = Hlc::mint(prev, now);
            assert!(next > last, "{next:?} not > {last:?} (now={now})");
            last = next;
            prev = Some(next);
        }
    }

    #[test]
    fn mint_advances_ctr_within_a_ms_and_resets_on_new_wall() {
        let a = Hlc::mint(None, 100);
        assert_eq!(a, h(100, 0));
        let b = Hlc::mint(Some(a), 100); // same ms → ctr bumps
        assert_eq!(b, h(100, 1));
        let c = Hlc::mint(Some(b), 200); // new ms → ctr resets
        assert_eq!(c, h(200, 0));
    }

    #[test]
    fn mint_uses_max_of_clock_and_prev_wall() {
        // Clock went backward vs prev wall → prev wall wins, ctr bumps.
        let a = Hlc::mint(None, 500);
        let b = Hlc::mint(Some(a), 100); // now(100) < prev wall(500)
        assert_eq!(b, h(500, 1));
    }

    #[test]
    fn hlc_total_order() {
        assert!(h(1, 0) < h(1, 1));
        assert!(h(1, 9) < h(2, 0));
        assert_eq!(h(3, 3), h(3, 3));
        // max
        assert_eq!(h(1, 5).max(h(2, 0)), h(2, 0));
        assert_eq!(h(2, 0).max(h(1, 5)), h(2, 0));
        assert_eq!(h(1, 3).max(h(1, 3)), h(1, 3));
    }

    // --- OR-set merge ---

    #[test]
    fn merge_union_of_disjoint_adds() {
        // Offline add of x + remote add of y → both present.
        let a = payload(&[el("x", h(10, 0), None)]);
        let b = payload(&[el("y", h(10, 1), None)]);
        let m = merge_or_set_payloads(&a, &b).unwrap();
        let mut present = present_elements(&m).unwrap();
        present.sort();
        assert_eq!(present, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn merge_is_commutative_and_idempotent() {
        let a = payload(&[el("x", h(10, 0), None), el("y", h(10, 1), None)]);
        let b = payload(&[el("y", h(20, 0), Some(h(15, 0))), el("z", h(12, 0), None)]);
        let ab = merge_or_set_payloads(&a, &b).unwrap();
        let ba = merge_or_set_payloads(&b, &a).unwrap();
        assert_eq!(ab, ba, "commutative");
        let abb = merge_or_set_payloads(&ab, &b).unwrap();
        assert_eq!(ab, abb, "idempotent");
    }

    #[test]
    fn add_wins_over_concurrent_remove() {
        // x added at h(10,0), removed (tombstone) at h(10,1) → remove is later → absent.
        let a = payload(&[el("x", h(10, 0), None)]);
        let b = payload(&[el("x", h(10, 0), Some(h(10, 1)))]);
        let m = merge_or_set_payloads(&a, &b).unwrap();
        assert!(
            present_elements(&m).unwrap().is_empty(),
            "later remove wins"
        );

        // Now re-add at a newer hlc → add-wins re-activates.
        let readd = payload(&[el("x", h(20, 0), None)]);
        let m2 = merge_or_set_payloads(&m, &readd).unwrap();
        assert_eq!(
            present_elements(&m2).unwrap(),
            vec!["x".to_string()],
            "re-add wins"
        );
    }

    #[test]
    fn merge_keeps_max_add_and_max_remove_per_element() {
        // Two adds of x at different HLCs → max add kept; a remove at a smaller
        // HLC than the add does NOT tombstone it.
        let a = payload(&[el("x", h(30, 0), None)]); // late add
        let b = payload(&[el("x", h(10, 0), Some(h(20, 0)))]); // early add + mid remove
        let m = merge_or_set_payloads(&a, &b).unwrap();
        // max add = h(30,0); remove = h(20,0); 30>20 → present.
        assert_eq!(present_elements(&m).unwrap(), vec!["x".to_string()]);
    }

    #[test]
    fn merge_empty_payloads() {
        let empty: &[u8] = b"";
        let a = payload(&[el("x", h(1, 0), None)]);
        assert_eq!(
            present_elements(&merge_or_set_payloads(&a, empty).unwrap()).unwrap(),
            vec!["x".to_string()]
        );
        assert!(
            present_elements(&merge_or_set_payloads(empty, empty).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merge_null_payload_is_empty_set() {
        let a = payload(&[el("x", h(1, 0), None)]);
        let m = merge_or_set_payloads(&a, b"null").unwrap();
        assert_eq!(present_elements(&m).unwrap(), vec!["x".to_string()]);
    }

    #[test]
    fn merge_rejects_non_object_payload() {
        // A bare array or scalar is malformed (it's not an OR-set row image).
        assert!(merge_or_set_payloads(b"[1,2,3]", b"{}").is_err());
        assert!(merge_or_set_payloads(b"\"oops\"", b"{}").is_err());
        assert!(merge_or_set_payloads(b"not-json", b"{}").is_err());
    }

    #[test]
    fn present_filters_tombstoned_only() {
        let p = payload(&[
            el("present_add", h(10, 0), None),
            el("removed", h(10, 0), Some(h(20, 0))), // remove > add → absent
            el("readded", h(30, 0), Some(h(20, 0))), // add > remove → present
        ]);
        let mut present = present_elements(&p).unwrap();
        present.sort();
        assert_eq!(
            present,
            vec!["present_add".to_string(), "readded".to_string()]
        );
    }

    #[test]
    fn is_present_add_wins() {
        assert!(el("x", h(10, 0), None).is_present());
        assert!(el("x", h(30, 0), Some(h(20, 0))).is_present()); // add beats remove
        assert!(!el("x", h(10, 0), Some(h(20, 0))).is_present()); // remove beats add
    }

    // --- PN-Counter ---

    fn ce(r: &str, p: u64, n: u64) -> PnEntry {
        PnEntry {
            r: r.to_string(),
            p,
            n,
        }
    }

    fn cpayload(entries: &[PnEntry]) -> Vec<u8> {
        serialize_counter_entries(entries)
    }

    #[test]
    fn counter_value_sums_p_minus_n() {
        let p = cpayload(&[ce("a", 5, 1), ce("b", 3, 0)]);
        assert_eq!(counter_value(&p).unwrap(), 7); // (5+3) - (1+0)
    }

    #[test]
    fn counter_value_can_be_negative() {
        let p = cpayload(&[ce("a", 1, 5)]);
        assert_eq!(counter_value(&p).unwrap(), -4);
    }

    #[test]
    fn counter_merge_is_per_replica_max() {
        // Two replicas increment concurrently; merge keeps the max per replica.
        let a = cpayload(&[ce("A", 3, 0), ce("B", 2, 0)]);
        let b = cpayload(&[ce("A", 3, 0), ce("C", 5, 0)]);
        let m = merge_counter_payloads(&a, &b).unwrap();
        assert_eq!(counter_value(&m).unwrap(), 10); // 3+2+5
                                                    // A replica's count never decreases on merge.
        let entries = parse_counter_entries(&m).unwrap();
        let a_entry = entries.iter().find(|e| e.r == "A").unwrap();
        assert_eq!((a_entry.p, a_entry.n), (3, 0));
    }

    #[test]
    fn counter_merge_is_commutative_associative_idempotent() {
        let a = cpayload(&[ce("A", 3, 1), ce("B", 2, 0)]);
        let b = cpayload(&[ce("B", 5, 2), ce("C", 1, 0)]);
        let c = cpayload(&[ce("A", 4, 0)]);

        // Commutative: merge(a,b) == merge(b,a)
        let ab = merge_counter_payloads(&a, &b).unwrap();
        let ba = merge_counter_payloads(&b, &a).unwrap();
        assert_eq!(ab, ba, "commutative");

        // Associative: merge(merge(a,b),c) == merge(a,merge(b,c))
        let ab_c = merge_counter_payloads(&ab, &c).unwrap();
        let a_bc = merge_counter_payloads(&a, &merge_counter_payloads(&b, &c).unwrap()).unwrap();
        assert_eq!(ab_c, a_bc, "associative");

        // Idempotent: merge(a,a) == a
        let aa = merge_counter_payloads(&a, &a).unwrap();
        assert_eq!(aa, a, "idempotent");
    }

    #[test]
    fn counter_apply_delta_positive_bumps_p() {
        let p = cpayload(&[ce("A", 3, 0)]);
        let p2 = counter_apply_delta(&p, "A", 2);
        assert_eq!(counter_value(&p2).unwrap(), 5);
        let entries = parse_counter_entries(&p2).unwrap();
        let a = entries.iter().find(|e| e.r == "A").unwrap();
        assert_eq!((a.p, a.n), (5, 0));
    }

    #[test]
    fn counter_apply_delta_negative_bumps_n() {
        let p = cpayload(&[ce("A", 5, 0)]);
        let p2 = counter_apply_delta(&p, "A", -3);
        assert_eq!(counter_value(&p2).unwrap(), 2); // 5 - 3
        let entries = parse_counter_entries(&p2).unwrap();
        let a = entries.iter().find(|e| e.r == "A").unwrap();
        assert_eq!((a.p, a.n), (5, 3));
    }

    #[test]
    fn counter_apply_delta_creates_new_replica() {
        let p = cpayload(&[ce("A", 1, 0)]);
        let p2 = counter_apply_delta(&p, "B", 4);
        assert_eq!(counter_value(&p2).unwrap(), 5); // 1 + 4
        assert!(parse_counter_entries(&p2)
            .unwrap()
            .iter()
            .any(|e| e.r == "B" && e.p == 4));
    }

    #[test]
    fn counter_apply_delta_on_empty_starts_fresh() {
        let p2 = counter_apply_delta(b"", "A", 7);
        assert_eq!(counter_value(&p2).unwrap(), 7);
    }

    #[test]
    fn counter_concurrent_increments_from_two_replicas_merge_correctly() {
        // The core convergence test: two replicas independently increment +
        // decrement, then merge. The result is the correct total regardless of
        // arrival order (commutative).
        // Replica A: +3, +2, -1  →  p_A=5, n_A=1
        let mut a_payload = b"".to_vec();
        a_payload = counter_apply_delta(&a_payload, "A", 3);
        a_payload = counter_apply_delta(&a_payload, "A", 2);
        a_payload = counter_apply_delta(&a_payload, "A", -1);

        // Replica B: +4, -2  →  p_B=4, n_B=2
        let mut b_payload = b"".to_vec();
        b_payload = counter_apply_delta(&b_payload, "B", 4);
        b_payload = counter_apply_delta(&b_payload, "B", -2);

        // Expected total: (5+4) - (1+2) = 6
        let merged = merge_counter_payloads(&a_payload, &b_payload).unwrap();
        assert_eq!(
            counter_value(&merged).unwrap(),
            6,
            "merged counter value is the correct total"
        );
        // Commutative regardless of arrival order.
        let merged_rev = merge_counter_payloads(&b_payload, &a_payload).unwrap();
        assert_eq!(counter_value(&merged_rev).unwrap(), 6);
        assert_eq!(merged, merged_rev, "byte-identical regardless of order");
    }

    #[test]
    fn counter_merge_empty_and_null_payloads() {
        let a = cpayload(&[ce("A", 3, 0)]);
        assert_eq!(
            counter_value(&merge_counter_payloads(&a, b"").unwrap()).unwrap(),
            3
        );
        assert_eq!(
            counter_value(&merge_counter_payloads(&a, b"null").unwrap()).unwrap(),
            3
        );
        assert_eq!(
            counter_value(&merge_counter_payloads(b"", b"").unwrap()).unwrap(),
            0
        );
    }

    #[test]
    fn counter_merge_rejects_non_object_payload() {
        assert!(merge_counter_payloads(b"[1,2,3]", b"{}").is_err());
        assert!(merge_counter_payloads(b"42", b"{}").is_err());
        assert!(merge_counter_payloads(b"not-json", b"{}").is_err());
    }
}
