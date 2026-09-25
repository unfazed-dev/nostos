//! Direct mode's native change source — `rpc/nostos_pull` over HTTPS.
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
//! - **No outbox policy.** [`PostgrestSource::push_write`] sends one write and
//!   classifies the answer; attempts, backoff and the dead-letter queue already
//!   exist on the `Outbox` trait for server mode and are not duplicated here.
//! - **No token refresh.** [`PostgrestSource::set_token`] is the seam; whoever
//!   owns the auth session calls it. Same division as
//!   `apps/atlet/flutter/lib/push/push_pilot.dart`, which re-registers per
//!   session rather than caching a token forever.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostos_core::{ApplyEngine, Horizon, PendingWrite, PullCursor, PullError, Storage, WriteOp};

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
    /// The URL could not be assembled into a `rpc/nostos_pull` endpoint.
    #[error("invalid PostgREST base url {0:?}")]
    BadUrl(String),

    /// The request never completed (DNS, TLS, connection, timeout). Retryable.
    #[error("PostgREST request failed: {0}")]
    Transport(String),

    /// PostgREST answered, but not with a result set. `401`/`403` is the usual
    /// one and it means the JWT, the exposed schema, or the grants — not the
    /// protocol.
    #[error("PostgREST returned HTTP {status}: {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body, which PostgREST fills with a usable SQL error.
        body: String,
    },

    /// The device's horizon is below what the log still retains: it was
    /// offline longer than the retention window, and the rows in the gap are
    /// gone. **Not** a transport failure and not fixable by retrying — the
    /// caller must clear local state, reset the horizon to [`Horizon::fresh`]
    /// and re-snapshot.
    ///
    /// Generated SQL raises `PT410`, which PostgREST turns into HTTP 410. The
    /// alternative — returning fewer rows — is silent loss: nothing on the
    /// device can tell a pruned gap from an empty one.
    #[error("the change log has been pruned past this device's horizon: {0}")]
    Gone(String),

    /// A queued write is not sendable as it stands (an upsert with no payload,
    /// an unparseable increment). Permanent — retrying cannot fix the queue
    /// entry, so it belongs in the dead-letter queue.
    #[error("unsendable write: {0}")]
    BadWrite(String),

    /// The response decoded but could not be applied. Rows before the failure
    /// may have committed; the horizon did not, so a retry re-reads them.
    #[error(transparent)]
    Pull(#[from] PullError),
}

impl PostgrestError {
    /// Whether a retry is pointless.
    ///
    /// `4xx` is the request's fault and will fail identically forever — with
    /// **one exception that matters**: `401` is usually an expired JWT, which
    /// a token refresh fixes, so it is retryable. `429` and `5xx` are the
    /// server asking for backoff. [`Self::Gone`] is permanent in the retry
    /// sense but not in the give-up sense: the fix is a re-snapshot, not a
    /// dead letter.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::BadUrl(_) | Self::BadWrite(_) | Self::Gone(_) => true,
            Self::Status { status, .. } => {
                !(*status == 401 || *status == 429 || (500..600).contains(status))
            }
            // Transport: retry. Pull: the rows did not commit and the horizon
            // did not move, so a retry re-reads and re-applies idempotently.
            Self::Transport(_) | Self::Pull(_) => false,
        }
    }
}

/// A PostgREST endpoint the device pulls its change log from.
///
/// Cheap to clone — `reqwest::Client` is an `Arc` internally and holds the
/// connection pool, so cloning shares it rather than opening a second one.
#[derive(Debug, Clone)]
pub struct PostgrestSource {
    http: reqwest::Client,
    /// `…/rest/v1` — the base every request is built from.
    rest_base: String,
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
        // reqwest 0.12 defaults to NO timeouts (async_impl/client.rs docs):
        // a stalled connection (measured 2026-09-25, iPhone behind a VPN)
        // hung the first sync forever. Connect bounds the handshake; read
        // bounds the gap between bytes, so a long snapshot download that
        // is still flowing never trips it.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| PostgrestError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            rest_base: format!("{base}/rest/v1"),
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

    /// The `rpc/nostos_pull` endpoint (for logs and `nostos doctor`).
    ///
    /// The function lives in `public` under a `nostos_` prefix, not in the
    /// `nostos` schema: Supabase exposes `public, graphql_public` by default, so
    /// a third schema would need a `Content-Profile` header on every request
    /// AND an operator ticking it into "Exposed schemas". Prefixing instead
    /// keeps the log table off the REST API entirely — `nostos link --mode
    /// direct` generates it that way.
    #[must_use]
    pub fn pull_endpoint(&self) -> String {
        format!("{}/rpc/nostos_pull", self.rest_base)
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
            .post(self.pull_endpoint())
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
        if status.as_u16() == 410 {
            // Only the pull can be Gone: 410 is the retention guard the
            // generated `nostos_pull` raises, and nothing else in the schema
            // uses that status.
            return Err(PostgrestError::Gone(body));
        }
        if !status.is_success() {
            return Err(PostgrestError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(body)
    }

    /// Fetch a full `nostos_snapshot()` — the current rows of every synced
    /// table plus a horizon, from one statement.
    ///
    /// The recovery half of [`PostgrestError::Gone`]. That error is permanent
    /// in the retry sense: the rows the device is missing have been pruned and
    /// no amount of pulling will produce them. This is the only way forward,
    /// and [`nostos_core::PullCursor::apply_snapshot`] is what turns the body
    /// into storage writes.
    ///
    /// # Errors
    /// [`PostgrestError::Transport`] if the request fails, or
    /// [`PostgrestError::Status`] on a non-2xx. Never `Gone`: a snapshot has
    /// no horizon to be below the retention window.
    pub async fn fetch_snapshot(&self) -> Result<String, PostgrestError> {
        let bearer = self.token.as_deref().unwrap_or(&self.apikey);
        let res = self
            .http
            .post(format!("{}/rpc/nostos_snapshot", self.rest_base))
            .header("apikey", &self.apikey)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
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

/// The primary-key column direct-mode writes filter on.
///
/// ponytail: `id`, matching the outbox's own v1 convention
/// (`nostos_core::PendingWrite::pk` documents it). A per-table pk column is a
/// `nostos link` concern — it already has to read the schema to generate
/// triggers — and belongs in generated config, not in a client-side guess.
const PK_COLUMN: &str = "id";

impl PostgrestSource {
    /// Send one queued write through PostgREST, as the signed-in user.
    ///
    /// The row never travels through a Nostos server, so the *only* thing
    /// authorizing it is RLS on the target table — which is the security
    /// argument for direct mode, not a caveat: a write the policy forbids is
    /// rejected by Postgres, not by a service the developer has to trust.
    ///
    /// The echo is expected. The write fires the change-log trigger, the device
    /// pulls its own row back, and the idempotent `(table, pk)` upsert absorbs
    /// it. No suppression, no client-id round trip.
    ///
    /// | [`WriteOp`] | request |
    /// |---|---|
    /// | `Upsert` | `POST /<table>` with `Prefer: resolution=merge-duplicates` |
    /// | `Patch` | `PATCH /<table>?id=eq.<pk>` — never inserts |
    /// | `Delete` | `DELETE /<table>?id=eq.<pk>` — 0 rows matched is success |
    /// | `Increment` | `POST /rpc/nostos_increment` |
    ///
    /// **`Increment` is the one that cannot be a plain table call.** ADR-0030's
    /// guarantee is that Postgres serializes concurrent increments
    /// (`SET x = x + ?`), and a PATCH body can only carry a literal — so
    /// expressing it client-side means read-modify-write and a lost update
    /// under concurrency. It therefore needs a generated function, the second
    /// one `nostos link --mode direct` must emit after `pull`.
    pub async fn push_write(&self, write: &PendingWrite) -> Result<(), PostgrestError> {
        let bearer = self.token.as_deref().unwrap_or(&self.apikey);
        let table = &write.table;
        let pk = &write.pk;

        let mut req = match write.op {
            WriteOp::Upsert => self
                .http
                .post(format!("{}/{table}", self.rest_base))
                // Upsert = insert-or-update, which is what PostgREST's
                // merge-duplicates resolution does. The payload is a full row
                // image, so it always carries the pk the conflict resolves on.
                .header("Prefer", "resolution=merge-duplicates,return=minimal"),
            WriteOp::Patch => self
                .http
                .patch(format!("{}/{table}?{PK_COLUMN}=eq.{pk}", self.rest_base))
                .header("Prefer", "return=minimal"),
            WriteOp::Delete => self
                .http
                .delete(format!("{}/{table}?{PK_COLUMN}=eq.{pk}", self.rest_base)),
            WriteOp::Increment => self
                .http
                .post(format!("{}/rpc/nostos_increment", self.rest_base))
                .header("Prefer", "return=minimal"),
        };

        req = req
            .header("apikey", &self.apikey)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json");

        req = match write.op {
            WriteOp::Delete => req,
            WriteOp::Increment => {
                // The outbox payload is `{"field": "<col>", "delta": <i64>}`;
                // the function needs the target as well.
                let inner: serde_json::Value =
                    serde_json::from_str(write.payload_json.as_deref().unwrap_or("{}"))
                        .map_err(|e| PostgrestError::BadWrite(format!("increment payload: {e}")))?;
                req.body(
                    serde_json::json!({
                        "p_table": table,
                        "p_pk": pk,
                        "p_field": inner.get("field"),
                        "p_delta": inner.get("delta"),
                    })
                    .to_string(),
                )
            }
            WriteOp::Upsert | WriteOp::Patch => {
                let body = write.payload_json.clone().ok_or_else(|| {
                    PostgrestError::BadWrite(format!(
                        "{} on {table}/{pk} has no payload",
                        write.op.as_wire_str()
                    ))
                })?;
                req.body(body)
            }
        };

        let res = req
            .send()
            .await
            .map_err(|e| PostgrestError::Transport(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let body = res.text().await.unwrap_or_default();
        Err(PostgrestError::Status {
            status: status.as_u16(),
            body,
        })
    }

    /// Register a push token for the signed-in user (ADR-0037's
    /// `registerPushToken`, routed to PostgREST because direct mode has no
    /// server socket to carry it).
    ///
    /// The scope is **not** a parameter: the generated function takes it from
    /// the caller's own JWT, so a device cannot register against another
    /// tenant no matter what it sends. `platform` is `fcm`, `apns` or
    /// `webpush` — the same three the other SDKs use.
    ///
    /// # Errors
    /// [`PostgrestError`] on transport failure or any non-success status.
    pub async fn register_push_token(
        &self,
        platform: &str,
        token: &str,
    ) -> Result<(), PostgrestError> {
        self.rpc(
            "nostos_register_push_token",
            &serde_json::json!({ "p_platform": platform, "p_token": token }),
        )
        .await
    }

    /// Drop a push token. Idempotent — a token that is not registered is a
    /// zero-row delete, which is success.
    ///
    /// # Errors
    /// [`PostgrestError`] on transport failure or any non-success status.
    pub async fn deregister_push_token(&self, token: &str) -> Result<(), PostgrestError> {
        self.rpc(
            "nostos_deregister_push_token",
            &serde_json::json!({ "p_token": token }),
        )
        .await
    }

    /// "This device is awake." Direct mode has no server holding sockets, so
    /// presence is something the device asserts; the push trigger skips any
    /// scope with a recent heartbeat because the Realtime ring already reached
    /// it. Cheap enough to send on every foreground and every successful
    /// drain.
    ///
    /// # Errors
    /// [`PostgrestError`] on transport failure or any non-success status.
    pub async fn heartbeat(&self, device_id: &str) -> Result<(), PostgrestError> {
        self.rpc(
            "nostos_heartbeat",
            &serde_json::json!({ "p_device_id": device_id }),
        )
        .await
    }

    /// POST a `void`-returning RPC and discard the body.
    async fn rpc(&self, name: &str, body: &serde_json::Value) -> Result<(), PostgrestError> {
        let bearer = self.token.as_deref().unwrap_or(&self.apikey);
        let res = self
            .http
            .post(format!("{}/rpc/{name}", self.rest_base))
            .header("apikey", &self.apikey)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| PostgrestError::Transport(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        Err(PostgrestError::Status {
            status: status.as_u16(),
            body: res.text().await.unwrap_or_default(),
        })
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
        assert_eq!(
            a.pull_endpoint(),
            "https://ref.supabase.co/rest/v1/rpc/nostos_pull"
        );
        assert_eq!(a.pull_endpoint(), b.pull_endpoint());
    }

    #[test]
    fn permanence_matches_who_is_at_fault() {
        let permanent = |status| {
            PostgrestError::Status {
                status,
                body: String::new(),
            }
            .is_permanent()
        };
        // 401 is the exception: an expired JWT is fixed by refreshing.
        assert!(!permanent(401), "refresh the token and retry");
        assert!(permanent(403), "RLS said no; it will keep saying no");
        assert!(permanent(404), "no such table / function");
        assert!(permanent(409), "constraint violation — dead-letter it");
        assert!(!permanent(429));
        assert!(!permanent(503));
        assert!(PostgrestError::BadWrite("no payload".into()).is_permanent());
        assert!(!PostgrestError::Transport("reset".into()).is_permanent());
    }
}
