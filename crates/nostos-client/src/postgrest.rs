//! Direct mode's native change source — `rpc/pull` over HTTPS.
//!
//! The I/O half of [`nostos_core::pull`]. Core decodes, groups by `xid` and
//! advances the horizon; this makes the POST and drives the paging loop. The
//! browser's half of the same seam is `fetch` inside `nostos-ffi-wasm`'s Worker
//! (ADR-0033), which is why none of the logic here is anything but transport.
//!
//! One HTTPS POST is the *entire* incremental dependency direct mode adds over
//! server mode — server mode already needs the WebSocket
//! (`docs/plans/direct-mode-sync-protocol.md`, "Every SDK gets this").
//!
//! ## What it does not do
//!
//! - **No doorbell.** The Realtime subscription is plan step 4; until it lands
//!   the caller decides when to call [`PostgrestSource::drain`] (on foreground,
//!   on a push wake, on a timer). Nothing here is less correct without it —
//!   every drain resumes from the stored horizon, so a missed doorbell costs
//!   staleness, never a row.
//! - **No snapshot.** A fresh device or one offline past the log's retention
//!   window needs a table snapshot through PostgREST and a horizon reset; see
//!   [`nostos_core::Horizon::fresh`].
//! - **No token refresh.** [`PostgrestSource::set_token`] is the seam; whoever
//!   owns the auth session calls it. Same division as
//!   `apps/atlet/flutter/lib/push/push_pilot.dart`, which re-registers per
//!   session rather than caching a token forever.

use std::sync::{Arc, Mutex};

use nostos_core::{ApplyEngine, Horizon, PullCursor, PullError, Storage};

/// Pages one [`PostgrestSource::drain`] call will fetch before returning with
/// `more = true`.
///
/// ponytail: a fixed cap, not a deadline. At the default 200 transactions per
/// page this is 12,800 transactions per call — enough that the common backlog
/// finishes in one drain, bounded enough that a continuously-written database
/// cannot hold the caller forever. A time budget is the upgrade if a real
/// device ever needs it.
pub const MAX_PAGES_PER_DRAIN: usize = 64;

/// What went wrong pulling from PostgREST.
#[derive(Debug, thiserror::Error)]
pub enum PostgrestError {
    /// The URL could not be assembled into a `rpc/pull` endpoint.
    #[error("invalid PostgREST base url {0:?}")]
    BadUrl(String),

    /// The request never completed (DNS, TLS, connection, timeout). Retryable.
    #[error("pull request failed: {0}")]
    Transport(String),

    /// PostgREST answered, but not with a result set. `401`/`403` is the usual
    /// one and it means the JWT, the exposed schema, or the grants — not the
    /// protocol.
    #[error("pull returned HTTP {status}: {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body, which PostgREST fills with a usable SQL error.
        body: String,
    },

    /// The response decoded but could not be applied. Rows before the failure
    /// may have committed; the horizon did not, so a retry re-reads them.
    #[error(transparent)]
    Pull(#[from] PullError),
}

/// A PostgREST endpoint the device pulls its change log from.
///
/// Cheap to clone — `reqwest::Client` is an `Arc` internally and holds the
/// connection pool, so cloning shares it rather than opening a second one.
#[derive(Debug, Clone)]
pub struct PostgrestSource {
    http: reqwest::Client,
    /// The fully-assembled `…/rest/v1/rpc/pull` URL.
    rpc_pull: String,
    /// Supabase's `apikey` header. Public by design (it is the anon key); RLS,
    /// not this, is what scopes the rows.
    apikey: String,
    /// The signed-in user's JWT. `None` = send the anon key as the bearer,
    /// which is what an unauthenticated client gets: whatever RLS grants
    /// `anon`, usually nothing.
    token: Option<String>,
}

impl PostgrestSource {
    /// Point a source at a Supabase project URL (`https://<ref>.supabase.co`)
    /// or any PostgREST base that serves `/rest/v1`.
    pub fn new(base_url: &str, apikey: impl Into<String>) -> Result<Self, PostgrestError> {
        let base = base_url.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(PostgrestError::BadUrl(base_url.to_string()));
        }
        Ok(Self {
            http: reqwest::Client::new(),
            rpc_pull: format!("{base}/rest/v1/rpc/pull"),
            apikey: apikey.into(),
            token: None,
        })
    }

    /// Swap in the signed-in user's access token. Call it on sign-in and on
    /// every refresh — a stale JWT surfaces as [`PostgrestError::Status`] 401,
    /// not as missing rows.
    pub fn set_token(&mut self, jwt: impl Into<String>) {
        self.token = Some(jwt.into());
    }

    /// Drop the user token, falling back to the anon key.
    pub fn clear_token(&mut self) {
        self.token = None;
    }

    /// The endpoint being pulled (for logs and `nostos doctor`).
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.rpc_pull
    }

    /// POST one page and return the raw response body.
    ///
    /// Kept public and separate from [`Self::drain`] so a caller that owns its
    /// own apply loop — the conformance suite, `nostos doctor` — can exercise
    /// the round trip without a storage engine.
    pub async fn fetch_page(&self, request_body: String) -> Result<String, PostgrestError> {
        let bearer = self.token.as_deref().unwrap_or(&self.apikey);
        let res = self
            .http
            .post(&self.rpc_pull)
            .header("apikey", &self.apikey)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(|e| PostgrestError::Transport(e.to_string()))?;

        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| PostgrestError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(PostgrestError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(body)
    }

    /// Pull until the log is caught up (or [`MAX_PAGES_PER_DRAIN`] is reached),
    /// applying each page atomically.
    ///
    /// The engine and cursor are shared behind mutexes because the apply is
    /// synchronous SQLite and runs on `spawn_blocking` — the same arrangement
    /// `SyncClient` uses for the server-mode apply path, and the reason the
    /// runtime stays responsive under a large backlog.
    ///
    /// **Locking order is cursor-then-engine, always.** The cursor is read
    /// alone to build the request; the apply takes both. Nothing else may take
    /// them in the other order.
    pub async fn drain<S>(
        &self,
        engine: &Arc<Mutex<ApplyEngine<S>>>,
        cursor: &Arc<Mutex<PullCursor>>,
    ) -> Result<DrainOutcome, PostgrestError>
    where
        S: Storage + Send + 'static,
    {
        let mut out = DrainOutcome::default();
        for _ in 0..MAX_PAGES_PER_DRAIN {
            let request = cursor
                .lock()
                .expect("drain: cursor mutex poisoned")
                .request_body();

            let body = self.fetch_page(request).await?;

            let engine = Arc::clone(engine);
            let cursor = Arc::clone(cursor);
            let outcome = tokio::task::spawn_blocking(move || {
                let mut engine = engine.lock().expect("drain: engine mutex poisoned");
                let mut cursor = cursor.lock().expect("drain: cursor mutex poisoned");
                cursor.apply(&mut engine, &body)
            })
            .await
            .map_err(|e| PostgrestError::Transport(format!("apply task failed: {e}")))??;

            out.pages += 1;
            out.rows_applied += outcome.rows_applied;
            if outcome.horizon.is_some() {
                out.horizon = outcome.horizon;
            }
            if !outcome.more {
                return Ok(out);
            }
        }
        // Hit the cap with the log still ahead of us. Not an error — the
        // horizon is durable, so the caller just calls again.
        out.more = true;
        Ok(out)
    }
}

/// The result of one [`PostgrestSource::drain`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Pages fetched.
    pub pages: usize,
    /// Rows committed across all of them.
    pub rows_applied: usize,
    /// The horizon the cursor ended at, or `None` if it never moved.
    pub horizon: Option<Horizon>,
    /// [`MAX_PAGES_PER_DRAIN`] was reached with more log outstanding — call
    /// `drain` again.
    pub more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_http_base_url_is_rejected() {
        assert!(matches!(
            PostgrestSource::new("ws://localhost:8080", "anon"),
            Err(PostgrestError::BadUrl(_))
        ));
    }

    #[test]
    fn the_rpc_url_is_assembled_once_and_tolerates_a_trailing_slash() {
        let a = PostgrestSource::new("https://ref.supabase.co", "anon").unwrap();
        let b = PostgrestSource::new("https://ref.supabase.co/", "anon").unwrap();
        assert_eq!(a.endpoint(), "https://ref.supabase.co/rest/v1/rpc/pull");
        assert_eq!(a.endpoint(), b.endpoint());
    }
}
