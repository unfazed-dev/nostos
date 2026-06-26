//! Postgres Log Sequence Number — the fundamental unit of replication progress.
//!
//! An LSN is a 64-bit offset into the WAL. In Nostos it doubles as our
//! checkpoint/cursor: a client records the highest LSN it has applied, and on
//! reconnect asks the server to resume from there. This is what makes sync
//! incremental and resumable (vs PowerSync's full-reprocessing model — see
//! ADR-0003).

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A Postgres WAL Log Sequence Number.
///
/// Wraps a `u64` so we can attach invariants (monotonic advance) and parse
/// Postgres's `X/Y` hex textual form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The zero LSN — used as "no progress yet."
    pub const ZERO: Lsn = Lsn(0);

    /// The maximum representable LSN.
    pub const MAX: Lsn = Lsn(u64::MAX);

    #[inline]
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    #[inline]
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Advance to `next` if it is strictly greater. Returns whether we moved.
    ///
    /// LSNs are monotonic — a replication stream never goes backward. This
    /// invariant is the foundation of "resume from checkpoint."
    #[inline]
    pub fn advance(&mut self, next: Lsn) -> bool {
        if next.0 > self.0 {
            self.0 = next.0;
            true
        } else {
            false
        }
    }

    /// Distance between two LSNs in bytes of WAL (Postgres semantics).
    #[inline]
    #[must_use]
    pub const fn diff(self, earlier: Lsn) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Parse the Postgres textual LSN form `HI/LO` (two 32-bit hex values).
impl FromStr for Lsn {
    type Err = LsnParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hi, lo) = s.split_once('/').ok_or(LsnParseError::MissingSeparator)?;
        let hi = u32::from_str_radix(hi.trim(), 16).map_err(LsnParseError::InvalidHex)?;
        let lo = u32::from_str_radix(lo.trim(), 16).map_err(LsnParseError::InvalidHex)?;
        Ok(Lsn((u64::from(hi) << 32) | u64::from(lo)))
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Postgres textual form: HI/LO in hex.
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xFFFF_FFFF)
    }
}

impl Default for Lsn {
    fn default() -> Self {
        Self::ZERO
    }
}

#[derive(Debug, Error)]
pub enum LsnParseError {
    #[error("LSN must be of the form HI/LO")]
    MissingSeparator,
    #[error("invalid hex component: {0}")]
    InvalidHex(ParseIntError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_textual_form_roundtrip() {
        for raw in [0u64, 1, 0xFFFF_FFFF, 0x1_0000_0000, u64::MAX] {
            let lsn = Lsn(raw);
            let s = lsn.to_string();
            let back: Lsn = s.parse().unwrap();
            assert_eq!(back, lsn, "roundtrip failed for raw={raw:#x}");
        }
    }

    #[test]
    fn advance_is_monotonic() {
        let mut l = Lsn::new(10);
        assert!(l.advance(Lsn::new(20)));
        assert_eq!(l.raw(), 20);
        // Equal or smaller must NOT move.
        assert!(!l.advance(Lsn::new(20)));
        assert!(!l.advance(Lsn::new(5)));
        assert_eq!(l.raw(), 20);
    }

    #[test]
    fn diff_is_saturating() {
        assert_eq!(Lsn::new(30).diff(Lsn::new(10)), 20);
        assert_eq!(Lsn::new(5).diff(Lsn::new(10)), 0); // never negative
    }

    #[test]
    fn ordering_works() {
        assert!(Lsn::new(1) < Lsn::new(2));
        assert_eq!(Lsn::new(5), Lsn::new(5));
    }

    #[test]
    fn rejects_malformed() {
        assert!("nope".parse::<Lsn>().is_err());
        assert!("AB".parse::<Lsn>().is_err()); // missing slash
        assert!("/1".parse::<Lsn>().is_err()); // empty hi
        "0/0".parse::<Lsn>().unwrap(); // valid zero
    }
}
