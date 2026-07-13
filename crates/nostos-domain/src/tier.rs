//! Subscription tier — the one entitlement type shared between the control
//! plane (nostos-cloud, which mints licenses) and the sync engine (nostos-server,
//! which enforces concurrent-device caps per session).
//!
//! Lives in the pure domain ring so neither sibling crate has to depend on the
//! other. The license *machinery* (signing, claims, HMAC) stays in `nostos-license`;
//! only the tier taxonomy + its concurrent-device ceiling live here.

/// Subscription tier. `Hobby` is the free tier; the rest are paid.
///
/// `device_cap()` returns the peak **concurrent** devices (live sync sessions),
/// not registered-device count — a sync engine's cost scales with live
/// connections, not fleet size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    #[default]
    Hobby,
    Pro,
    Scale,
    Enterprise,
}

impl Tier {
    /// Peak concurrent devices (live sync sessions) at this tier.
    /// Aligns with the landing pricing + the reactive-default strategy memo
    /// (Hobby = 100 concurrent, the free tier's deliberate ceiling).
    #[must_use]
    pub const fn device_cap(self) -> u64 {
        match self {
            Self::Hobby => 100,
            Self::Pro => 1_000,
            Self::Scale => 10_000,
            Self::Enterprise => u64::MAX,
        }
    }
}
