//! Rail dispatch seam (ADR-0038 §1, plan tasks 1.5 + 1.7).
//!
//! The three nostos-infra rails are plain structs with inherent async send
//! methods — nostos-infra deliberately declares no per-rail trait
//! (push/mod.rs), and their test-pointing constructors (FcmRail::
//! with_endpoints, ApnsRail::with_base, WebPushRail's http escape) are
//! private to that crate. This module is therefore the daemon's OWN minimal
//! seam: [RailDispatch] wraps exactly the three public send signatures, the
//! production [Rails] registry is built from their shared env contract via
//! [nostos_infra::RailSet::from_env], and tests substitute an in-memory
//! implementation WITHOUT touching nostos-infra (the sanctioned alternative
//! to duplicating the ProviderMock HTTP fixture). The rails are reused
//! AS-IS — no rail forks (ADR-0038 §1).

use std::sync::Arc;

use async_trait::async_trait;
use nostos_infra::push::{PushPayload, PushRailError, RailOutcome};
use nostos_infra::{ApnsRail, FcmRail, FcmTarget, RailSet, WebPushRail};

use crate::store::Platform;

/// One rail's send surface — token + supersede key + already-built payload,
/// exactly the three rails' public signatures.
#[async_trait]
pub trait RailDispatch: Send + Sync {
    /// Send one push. Every terminal state is a [RailOutcome] value.
    async fn send(
        &self,
        token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome;
}

#[async_trait]
impl RailDispatch for ApnsRail {
    async fn send(
        &self,
        token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        ApnsRail::send(self, token, collapse_key, payload).await
    }
}

#[async_trait]
impl RailDispatch for FcmRail {
    async fn send(
        &self,
        token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        // Daemon tokens are FCM registration tokens; the fid target form is
        // the embedded story's concern, not registrable here.
        FcmRail::send(
            self,
            &FcmTarget::Token(token.to_string()),
            collapse_key,
            payload,
        )
        .await
    }
}

#[async_trait]
impl RailDispatch for WebPushRail {
    async fn send(
        &self,
        token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        // The registry token for this rail is the browser pushSubscription
        // JSON — passed through verbatim.
        WebPushRail::send(self, token, collapse_key, payload).await
    }
}

/// Which rails are configured, for GET /v1/healthz (from_env per rail).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RailsHealth {
    pub apns: bool,
    pub fcm: bool,
    pub webpush: bool,
}

/// The daemon's rail registry: per-platform dispatch behind [RailDispatch],
/// plus the configured/unconfigured answer the send route needs BEFORE
/// enqueueing (503 per the contract). An unconfigured platform reached at
/// flush time maps to Fatal — the same visibility stance as nostos-infra's
/// RailSet (an operator gap must be visible on the receipt, not silent).
#[derive(Clone, Default)]
pub struct Rails {
    apns: Option<Arc<dyn RailDispatch>>,
    fcm: Option<Arc<dyn RailDispatch>>,
    webpush: Option<Arc<dyn RailDispatch>>,
}

impl Rails {
    /// Assemble from pre-built dispatchers (tests inject mocks here; the
    /// None slots are simply unconfigured rails).
    #[must_use]
    pub fn new(
        apns: Option<Arc<dyn RailDispatch>>,
        fcm: Option<Arc<dyn RailDispatch>>,
        webpush: Option<Arc<dyn RailDispatch>>,
    ) -> Self {
        Self { apns, fcm, webpush }
    }

    /// Build every rail from the shared env contract (plan task 1.7):
    /// NOSTOS_FCM_CREDENTIALS_JSON, NOSTOS_APNS_*, NOSTOS_WEBPUSH_VAPID_* —
    /// each rail's from_env(), unset = rail off. No env handling is
    /// duplicated here.
    ///
    /// # Errors
    /// [PushRailError] when a configured rail fails to construct — the
    /// caller refuses to start.
    pub fn from_env() -> Result<Self, PushRailError> {
        let set = RailSet::from_env()?;
        Ok(Self {
            apns: set.apns.map(|r| Arc::new(r) as Arc<dyn RailDispatch>),
            fcm: set.fcm.map(|r| Arc::new(r) as Arc<dyn RailDispatch>),
            webpush: set.webpush.map(|r| Arc::new(r) as Arc<dyn RailDispatch>),
        })
    }

    /// The configured check the send route performs before enqueueing.
    #[must_use]
    pub fn configured(&self, platform: Platform) -> bool {
        match platform {
            Platform::Apns => self.apns.is_some(),
            Platform::Fcm => self.fcm.is_some(),
            Platform::Webpush => self.webpush.is_some(),
        }
    }

    /// healthz rail booleans.
    #[must_use]
    pub fn health(&self) -> RailsHealth {
        RailsHealth {
            apns: self.apns.is_some(),
            fcm: self.fcm.is_some(),
            webpush: self.webpush.is_some(),
        }
    }

    /// Send through the platform's rail (flush path).
    pub async fn dispatch(
        &self,
        platform: Platform,
        token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        let rail = match platform {
            Platform::Apns => &self.apns,
            Platform::Fcm => &self.fcm,
            Platform::Webpush => &self.webpush,
        };
        match rail {
            Some(r) => r.send(token, collapse_key, payload).await,
            None => RailOutcome::Fatal(format!(
                "push rail for platform '{}' is not configured on this daemon",
                platform.as_str()
            )),
        }
    }
}

/// Default supersede key per (tenant, token): FNV-1a-64 hex (16 chars).
/// Deterministic so consecutive debounce windows supersede on-device;
/// base64url-alphabet-safe and short enough for every rail's native cap
/// (Web Push Topic <= 32 base64url chars, APNs apns-collapse-id <= 64
/// bytes, FCM collapse_key <= 6 identifiers-worth of bytes). FNV is NOT
/// cryptographic — it only needs collision resistance across one tenant's
/// tokens, and a 64-bit non-crypto hash gives that comfortably.
#[must_use]
pub fn default_collapse_key(tenant: &str, token: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in tenant.bytes().chain([0x1f]).chain(token.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::default_collapse_key;

    #[test]
    fn collapse_key_is_stable_short_and_distinct() {
        let a = default_collapse_key("tenant-a", "tok-1");
        assert_eq!(a, default_collapse_key("tenant-a", "tok-1"), "stable");
        assert_eq!(a.len(), 16, "16 hex chars — fits Web Push Topic's 32");
        assert_ne!(
            default_collapse_key("tenant-a", "tok-1"),
            default_collapse_key("tenant-b", "tok-1"),
            "tenant-scoped"
        );
        // The separator byte kills the (ab, c) == (a, bc) ambiguity.
        assert_ne!(
            default_collapse_key("ab", "c"),
            default_collapse_key("a", "bc")
        );
    }
}
