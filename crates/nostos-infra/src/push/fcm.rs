//! FCM HTTP v1 rail (ADR-0037 §1, plan task 2.1).
//!
//! - Auth: the OAuth2 service-account flow — an RS256 JWT signed with the
//!   service account's private key (`jsonwebtoken`, the JWT crate this crate
//!   already uses for JWKS verification) exchanged at
//!   `oauth2.googleapis.com/token`; the access token is cached until 60s
//!   before expiry.
//! - Single send: `POST /v1/projects/{id}/messages:send`.
//! - Fan-out: the Google batch endpoint (`POST /batch`, `multipart/mixed`
//!   with one `application/http` part per message) — up to
//!   [`FCM_BATCH_MAX`] messages per request, chunked automatically.
//! - Doorbell = data-only message (`data` map + `android {priority NORMAL,
//!   ttl 60s}`); visible = `notification {title, body}` + `HIGH` + 1h ttl
//!   (ADR-0037 §2/§4). `android.collapse_key` carries the per-(device,
//!   subscription) supersede key.
//! - Target field: `token` today; `fid` accepted for the announced
//!   `token`→`fid` deprecation (research doc §4) — [`FcmTarget`] picks the
//!   JSON field.
//! - `error.status == "UNREGISTERED"` (HTTP 404) maps to
//!   [`RailOutcome::Unregistered`] — the `PgTokenStore` prune trigger.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::warn;

use super::{
    env_nonempty, http_client, PushPayload, PushRailError, RailOutcome, SILENT_TTL_SECS,
    VISIBLE_TTL_SECS,
};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FCM_BASE: &str = "https://fcm.googleapis.com";
const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
/// Google batch endpoints accept at most 500 messages per request.
pub(crate) const FCM_BATCH_MAX: usize = 500;
/// Refresh the cached access token this long before its `expires_in`.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_mins(1);
/// Multipart boundary for the batch body. Safe by construction: the parts are
/// `serde_json` output, which never contains raw newlines.
const BATCH_BOUNDARY: &str = "nostos-fcm-batch-1a2b3c";

/// The device identifier a message targets. Serializes into the `token`
/// (current) or `fid` (announced successor, research doc §4) JSON field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FcmTarget {
    Token(String),
    Fid(String),
}

/// One message for [`FcmRail::send_batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcmMessage {
    pub target: FcmTarget,
    pub collapse_key: Option<String>,
    pub payload: PushPayload,
}

#[derive(serde::Deserialize)]
struct ServiceAccount {
    project_id: String,
    private_key: String,
    client_email: String,
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

pub struct FcmRail {
    http: reqwest::Client,
    credentials: ServiceAccount,
    encoding_key: EncodingKey,
    send_url: String,
    /// Request-path only (`/v1/projects/{id}/messages:send`) — the batch
    /// parts carry the path, not the absolute URL.
    send_path: String,
    batch_url: String,
    token_url: String,
    token_cache: Mutex<Option<(String, Instant)>>,
}

impl FcmRail {
    /// Build from a Google service-account JSON (the value of
    /// `NOSTOS_FCM_CREDENTIALS_JSON`).
    ///
    /// # Errors
    /// [`PushRailError`] if the JSON is malformed or `private_key` is not a
    /// PEM RSA key.
    pub fn new(credentials_json: &str) -> Result<Self, PushRailError> {
        Self::with_endpoints(credentials_json, FCM_BASE, TOKEN_URL.to_string())
    }

    /// `NOSTOS_FCM_CREDENTIALS_JSON` — `Ok(None)` (rail off) when unset.
    ///
    /// # Errors
    /// [`PushRailError`] when set but invalid.
    pub fn from_env() -> Result<Option<Self>, PushRailError> {
        match env_nonempty("NOSTOS_FCM_CREDENTIALS_JSON") {
            Some(json) => Self::new(&json).map(Some),
            None => Ok(None),
        }
    }

    fn with_endpoints(
        credentials_json: &str,
        fcm_base: &str,
        token_url: String,
    ) -> Result<Self, PushRailError> {
        let credentials: ServiceAccount = serde_json::from_str(credentials_json)
            .map_err(|e| PushRailError(format!("service-account json: {e}")))?;
        let encoding_key = EncodingKey::from_rsa_pem(credentials.private_key.as_bytes())
            .map_err(|e| PushRailError(format!("service-account private_key: {e}")))?;
        let send_path = format!("/v1/projects/{}/messages:send", credentials.project_id);
        Ok(Self {
            http: http_client(),
            send_url: format!("{fcm_base}{send_path}"),
            batch_url: format!("{fcm_base}/batch"),
            token_url,
            encoding_key,
            credentials,
            token_cache: Mutex::new(None),
            send_path,
        })
    }

    /// Send one message. Doorbell or visible per `payload`.
    pub async fn send(
        &self,
        target: &FcmTarget,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        let token = match self.access_token().await {
            Ok(t) => t,
            Err(e) => return RailOutcome::Fatal(e.to_string()),
        };
        let body = message_json(target, collapse_key, payload);
        match self
            .http
            .post(&self.send_url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let parsed: Value = resp.json().await.unwrap_or(Value::Null);
                outcome_for(status, &parsed)
            }
            Err(e) => {
                warn!(%e, "fcm send network error");
                RailOutcome::TransientRetryable
            }
        }
    }

    /// Send many messages over the 500-per-request batch endpoint. Returns
    /// one outcome per input, in order. Auth failures map to `Fatal` for
    /// every message; per-part statuses are mapped individually.
    pub async fn send_batch(&self, msgs: &[FcmMessage]) -> Vec<RailOutcome> {
        let mut out = Vec::with_capacity(msgs.len());
        for chunk in msgs.chunks(FCM_BATCH_MAX) {
            out.extend(self.send_batch_chunk(chunk).await);
        }
        out
    }

    async fn send_batch_chunk(&self, msgs: &[FcmMessage]) -> Vec<RailOutcome> {
        if msgs.is_empty() {
            return Vec::new();
        }
        let token = match self.access_token().await {
            Ok(t) => t,
            Err(e) => return vec![RailOutcome::Fatal(e.to_string()); msgs.len()],
        };
        let mut body = String::new();
        for (i, m) in msgs.iter().enumerate() {
            let json = serde_json::to_string(&message_json(
                &m.target,
                m.collapse_key.as_deref(),
                &m.payload,
            ))
            .unwrap_or_default();
            let _ = write!(
                body,
                "--{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: {}\r\n\r\nPOST {}\r\nContent-Type: application/json\r\n\r\n{json}\r\n",
                i + 1,
                self.send_path,
            );
        }
        let _ = write!(body, "--{BATCH_BOUNDARY}--\r\n");
        match self
            .http
            .post(&self.batch_url)
            .bearer_auth(&token)
            .header(
                "Content-Type",
                format!("multipart/mixed; boundary={BATCH_BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let text = resp.text().await.unwrap_or_default();
                if !(200..300).contains(&status) {
                    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    return vec![outcome_for(status, &parsed); msgs.len()];
                }
                match parse_batch_response(&content_type, &text) {
                    Ok(parts) if parts.len() == msgs.len() => parts
                        .into_iter()
                        .map(|(code, b)| outcome_for(code, &b))
                        .collect(),
                    Ok(parts) => {
                        // ponytail: parts are matched by position (Content-ID
                        // correlation skipped); on a count mismatch every
                        // message is Fatal rather than misattributed.
                        warn!(
                            got = parts.len(),
                            expected = msgs.len(),
                            "fcm batch part mismatch"
                        );
                        vec![
                            RailOutcome::Fatal("fcm batch response part count mismatch".into());
                            msgs.len()
                        ]
                    }
                    Err(e) => vec![RailOutcome::Fatal(e.to_string()); msgs.len()],
                }
            }
            Err(e) => {
                warn!(%e, "fcm batch network error");
                vec![RailOutcome::TransientRetryable; msgs.len()]
            }
        }
    }

    /// Cached OAuth2 access token (refreshed 60s before `expires_in`).
    /// ponytail: the lock is held across the token fetch, so concurrent
    /// senders serialize onto one fetch — a single-flight primitive would
    /// only matter at fetch rates this rail never sees.
    async fn access_token(&self) -> Result<String, PushRailError> {
        let mut cache = self.token_cache.lock().await;
        if let Some((token, expiry)) = cache.as_ref() {
            if *expiry > Instant::now() {
                return Ok(token.clone());
            }
        }
        let now = jsonwebtoken::get_current_timestamp();
        let claims = json!({
            "iss": self.credentials.client_email,
            "scope": FCM_SCOPE,
            "aud": self.token_url,
            "iat": now,
            "exp": now + 3600,
        });
        let assertion = encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .map_err(|e| PushRailError(format!("fcm assertion sign: {e}")))?;
        let resp = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| PushRailError(format!("fcm token endpoint: {e}")))?;
        let status = resp.status();
        let parsed: Result<TokenResponse, _> = resp.json().await;
        let tr = match parsed {
            Ok(tr) if status.is_success() => tr,
            _ => {
                return Err(PushRailError(format!(
                    "fcm token endpoint returned {status}"
                )))
            }
        };
        // Defensive clamp: never trust a longer expiry than Google's 1h, and
        // keep at least one second past the refresh margin.
        let ttl = tr.expires_in.unwrap_or(3600).clamp(61, 3600);
        *cache = Some((
            tr.access_token.clone(),
            Instant::now()
                + Duration::from_secs(ttl.saturating_sub(TOKEN_REFRESH_MARGIN.as_secs())),
        ));
        Ok(tr.access_token)
    }
}

/// Map an FCM REST status + error body to the rail outcome. Google's
/// canonical marker for a dead token is `UNREGISTERED`, but the SHAPES it
/// arrives in differ by endpoint vintage — the live v1 API sends
/// `{"error":{"status":"NOT_FOUND","message":"NotRegistered","details":[
/// {"@type":"…FcmError","errorCode":"UNREGISTERED"}]}}` (caught by the atlet
/// order-lifecycle smoke against real FCM; the original `status ==
/// "UNREGISTERED"` match never fired and dead tokens were never pruned).
/// Match all three shapes; quota/unavailable stay retryable.
fn outcome_for(status: u16, body: &Value) -> RailOutcome {
    if (200..300).contains(&status) {
        return RailOutcome::Delivered;
    }
    let unregistered = body.pointer("/error/status").and_then(Value::as_str)
        == Some("UNREGISTERED")
        || body
            .pointer("/error/details/0/errorCode")
            .and_then(Value::as_str)
            == Some("UNREGISTERED")
        || body.pointer("/error/message").and_then(Value::as_str) == Some("NotRegistered");
    if unregistered {
        return RailOutcome::Unregistered;
    }
    match status {
        429 | 500 | 502 | 503 | 504 => RailOutcome::TransientRetryable,
        _ => RailOutcome::Fatal(format!("fcm {status}: {body}")),
    }
}

/// Android notification channel nostos's visible pushes target. The client
/// app must create it at IMPORTANCE_HIGH or Android posts silently on the
/// DEFAULT fallback channel (no heads-up banner). See ADR-0037 §2.
const ANDROID_CHANNEL_ID: &str = "cairn";

/// Build the `messages:send` JSON. Doorbell = data-only; visible =
/// `notification`. The `android` block carries collapse/ttl/priority (the
/// registry's platform column routes only Android tokens to this rail).
fn message_json(target: &FcmTarget, collapse_key: Option<&str>, payload: &PushPayload) -> Value {
    let mut message = match target {
        FcmTarget::Token(t) => json!({ "token": t }),
        FcmTarget::Fid(f) => json!({ "fid": f }),
    };
    let (ttl, priority) = match payload {
        PushPayload::Silent { table, lsn } => {
            message["data"] = json!({ "table": table, "lsn": lsn.0.to_string() });
            (SILENT_TTL_SECS, "NORMAL")
        }
        PushPayload::Visible {
            title,
            body,
            category: Some(category),
        } => {
            // Action push (ADR-0037 §2 `action` mode), one message shaped per
            // platform by omission of the top-level `notification` block:
            //   iOS    — FCM maps the `apns` block into the APNs payload, so
            //            the system renders `aps.alert` WITH the client-
            //            registered category's buttons (killed app included).
            //   Android— no `notification` block means the system renders
            //            nothing; the HIGH-priority data message wakes the
            //            app (WhatsApp pattern), whose handler posts a local
            //            notification carrying the actions. Requires a
            //            cooperating client — plain `visible` stays the
            //            zero-client-code mode.
            message["data"] = json!({
                "title": title, "body": body, "category": category,
            });
            message["apns"] = json!({ "payload": { "aps": {
                "alert": { "title": title, "body": body },
                "sound": "default",
                "category": category,
            } } });
            (VISIBLE_TTL_SECS, "HIGH")
        }
        PushPayload::Visible { title, body, .. } => {
            message["notification"] = json!({ "title": title, "body": body });
            (VISIBLE_TTL_SECS, "HIGH")
        }
    };
    let mut android = json!({ "priority": priority, "ttl": format!("{ttl}s") });
    if let PushPayload::Visible { category: None, .. } = payload {
        // Route to the app's HIGH-importance channel so Android shows a
        // heads-up banner (data-only doorbells keep the fallback channel),
        // and ask for the platform default sound + an explicit double-buzz
        // pattern (WhatsApp-style) so the arrival is felt, not just seen.
        // On API 26+ the channel's own vibration governs; vibrate_timings
        // covers pre-O devices where the payload is authoritative.
        android["notification"] = json!({
            "channel_id": ANDROID_CHANNEL_ID,
            "default_sound": true,
            "vibrate_timings": ["0s", "0.3s", "0.2s", "0.3s"],
        });
    }
    // iOS: APNs plays the default tri-tone + haptic only when the APS
    // payload carries a sound — without it the banner lands silent and
    // still. Action pushes build their own apns block above (alert +
    // category); only plain visible ones need it here.
    if let PushPayload::Visible { category: None, .. } = payload {
        message["apns"] = json!({ "payload": { "aps": { "sound": "default" } } });
    }
    if let Some(key) = collapse_key {
        android["collapse_key"] = json!(key);
    }
    message["android"] = android;
    json!({ "message": message })
}

/// Parse a Google batch `multipart/mixed` response into per-part
/// `(status, json body)` pairs. Each part embeds an HTTP response
/// (`application/http`): status line, headers, blank line, JSON body.
fn parse_batch_response(
    content_type: &str,
    body: &str,
) -> Result<Vec<(u16, Value)>, PushRailError> {
    let boundary = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(str::to_string)
        .ok_or_else(|| PushRailError("fcm batch response has no boundary".into()))?;
    let boundary = boundary.trim_matches('"');
    let mut parts = Vec::new();
    for part in body.split(&format!("--{boundary}")) {
        let part = part.trim_matches(|c| c == '\r' || c == '\n');
        if part.is_empty() || part == "--" {
            continue;
        }
        let Some(http_pos) = part.find("HTTP/") else {
            continue;
        };
        let line_end = part[http_pos..]
            .find('\n')
            .map_or(part.len(), |i| http_pos + i);
        let code = part[http_pos..line_end]
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let json_start = part[line_end..].find("\r\n\r\n").map_or_else(
            || part[line_end..].find("\n\n").map(|i| line_end + i + 2),
            |i| Some(line_end + i + 4),
        );
        let json = json_start
            .and_then(|i| {
                serde_json::from_str::<Value>(part[i..].trim_matches(|c| c == '\r' || c == '\n'))
                    .ok()
            })
            .unwrap_or(Value::Null);
        parts.push((code, json));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::test_support::{jwt_parts, pkcs8_pem, CannedResponse, ProviderMock};
    use nostos_domain::Lsn;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;
    use std::collections::{HashMap, VecDeque};

    fn service_account_json() -> String {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
        let pem = pkcs8_pem(key.to_pkcs8_der().expect("pkcs8 der").as_bytes());
        serde_json::to_string(&json!({
            "project_id": "p1",
            "private_key": pem,
            "client_email": "svc@p1.iam.gserviceaccount.com",
        }))
        .expect("service account json")
    }

    fn queue(resps: Vec<CannedResponse>) -> VecDeque<CannedResponse> {
        resps.into_iter().collect()
    }

    /// Mock with the token endpoint primed; returns (rail, mock).
    async fn rail_with_mock(send_responses: Vec<CannedResponse>) -> (FcmRail, ProviderMock) {
        let mut routes = HashMap::new();
        routes.insert(
            "/token".to_string(),
            queue(vec![CannedResponse::json(
                200,
                r#"{"access_token":"at-1","expires_in":3600}"#,
            )]),
        );
        routes.insert(
            "/v1/projects/p1/messages:send".to_string(),
            queue(send_responses),
        );
        let mock = ProviderMock::start(routes).await;
        let base = mock.url("");
        let rail = FcmRail::with_endpoints(&service_account_json(), &base, mock.url("/token"))
            .expect("rail");
        (rail, mock)
    }

    fn silent() -> PushPayload {
        PushPayload::Silent {
            table: "tasks".into(),
            lsn: Lsn::new(12345),
        }
    }

    #[tokio::test]
    async fn fcm_silent_data_only_payload_shape() {
        let (rail, mock) =
            rail_with_mock(vec![CannedResponse::json(200, r#"{"name":"m1"}"#)]).await;
        let outcome = rail
            .send(
                &FcmTarget::Token("tok-1".into()),
                Some("sync-tasks"),
                &silent(),
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let requests = mock.requests();
        let send = &requests[1];
        assert_eq!(send.method, "POST");
        assert_eq!(send.path, "/v1/projects/p1/messages:send");
        assert_eq!(send.header("authorization"), Some("Bearer at-1"));
        assert_eq!(
            send.json(),
            json!({
                "message": {
                    "token": "tok-1",
                    "data": { "table": "tasks", "lsn": "12345" },
                    "android": { "collapse_key": "sync-tasks", "priority": "NORMAL", "ttl": "60s" }
                }
            })
        );
    }

    #[tokio::test]
    async fn fcm_visible_notification_payload_shape() {
        let (rail, mock) =
            rail_with_mock(vec![CannedResponse::json(200, r#"{"name":"m2"}"#)]).await;
        let outcome = rail
            .send(
                &FcmTarget::Token("tok-1".into()),
                Some("sync-tasks"),
                &PushPayload::Visible {
                    title: "Tasks changed".into(),
                    body: "New items to sync".into(),
                    category: None,
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let send = &mock.requests()[1];
        assert_eq!(
            send.json(),
            json!({
                "message": {
                    "token": "tok-1",
                    "notification": { "title": "Tasks changed", "body": "New items to sync" },
                    "android": { "collapse_key": "sync-tasks", "priority": "HIGH", "ttl": "3600s",
                                 "notification": { "channel_id": "cairn", "default_sound": true,
                                                   "vibrate_timings": ["0s", "0.3s", "0.2s", "0.3s"] } },
                    "apns": { "payload": { "aps": { "sound": "default" } } }
                }
            })
        );
    }

    #[tokio::test]
    async fn fcm_action_payload_shapes_both_platforms_in_one_message() {
        let (rail, mock) =
            rail_with_mock(vec![CannedResponse::json(200, r#"{"name":"m4"}"#)]).await;
        let outcome = rail
            .send(
                &FcmTarget::Token("tok-1".into()),
                Some("sync-tasks"),
                &PushPayload::Visible {
                    title: "Tasks changed".into(),
                    body: "New items to sync".into(),
                    category: Some("order_status".into()),
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let send = &mock.requests()[1];
        let message = &send.json()["message"];
        // No top-level notification: Android's system must not render (the
        // app's data handler posts the action notification locally)…
        assert!(
            message.get("notification").is_none(),
            "action pushes must be data-only for Android"
        );
        assert_eq!(
            message["data"],
            json!({ "title": "Tasks changed", "body": "New items to sync",
                    "category": "order_status" })
        );
        assert_eq!(message["android"]["priority"], json!("HIGH"));
        // …while iOS renders from the apns override, WITH the category's
        // registered action buttons.
        assert_eq!(
            message["apns"]["payload"]["aps"],
            json!({
                "alert": { "title": "Tasks changed", "body": "New items to sync" },
                "sound": "default",
                "category": "order_status"
            })
        );
    }

    #[tokio::test]
    async fn fcm_fid_target_serializes_fid_field() {
        let (rail, mock) =
            rail_with_mock(vec![CannedResponse::json(200, r#"{"name":"m3"}"#)]).await;
        let outcome = rail
            .send(&FcmTarget::Fid("fid-1".into()), None, &silent())
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let send = &mock.requests()[1];
        assert_eq!(send.json()["message"]["fid"], json!("fid-1"));
        assert!(send.json()["message"].get("token").is_none());
        // No collapse key -> the android block omits collapse_key.
        assert!(send.json()["message"]["android"]
            .get("collapse_key")
            .is_none());
    }

    #[tokio::test]
    async fn fcm_oauth_assertion_shape_and_token_caching() {
        let (rail, mock) = rail_with_mock(vec![
            CannedResponse::json(200, r#"{"name":"a"}"#),
            CannedResponse::json(200, r#"{"name":"b"}"#),
        ])
        .await;
        for _ in 0..2 {
            let outcome = rail
                .send(&FcmTarget::Token("tok-1".into()), None, &silent())
                .await;
            assert_eq!(outcome, RailOutcome::Delivered);
        }
        let requests = mock.requests();
        // Token endpoint hit exactly once for two sends.
        assert_eq!(requests.iter().filter(|r| r.path == "/token").count(), 1);
        let token_req = &requests[0];
        let form = token_req.body_str();
        assert!(form.starts_with(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion="
        ));
        let assertion = form.split("assertion=").nth(1).expect("assertion");
        let (header, claims) = jwt_parts(assertion);
        assert_eq!(header["alg"], json!("RS256"));
        assert_eq!(claims["iss"], json!("svc@p1.iam.gserviceaccount.com"));
        assert_eq!(claims["scope"], json!(FCM_SCOPE));
        assert_eq!(claims["aud"], json!(mock.url("/token")));
        // Both sends rode the same cached bearer.
        assert_eq!(requests[1].header("authorization"), Some("Bearer at-1"));
        assert_eq!(requests[2].header("authorization"), Some("Bearer at-1"));
    }

    #[tokio::test]
    async fn fcm_unregistered_maps_to_prune_trigger() {
        // The LIVE v1 shape, verbatim from a real 404 (the order-lifecycle
        // smoke caught the old `/error/status == "UNREGISTERED"` match never
        // firing on it): status is NOT_FOUND; the marker rides details[0].
        let (rail, _mock) = rail_with_mock(vec![CannedResponse::json(
            404,
            r#"{"error":{"code":404,"details":[{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":"UNREGISTERED"}],"message":"NotRegistered","status":"NOT_FOUND"}}"#,
        )])
        .await;
        let outcome = rail
            .send(&FcmTarget::Token("dead".into()), None, &silent())
            .await;
        assert_eq!(outcome, RailOutcome::Unregistered);
    }

    #[tokio::test]
    async fn fcm_transient_statuses_are_retryable() {
        let (rail, _mock) = rail_with_mock(vec![
            CannedResponse::json(503, r#"{"error":{"status":"UNAVAILABLE"}}"#),
            CannedResponse::json(429, r#"{"error":{"status":"RESOURCE_EXHAUSTED"}}"#),
        ])
        .await;
        assert_eq!(
            rail.send(&FcmTarget::Token("t".into()), None, &silent())
                .await,
            RailOutcome::TransientRetryable
        );
        assert_eq!(
            rail.send(&FcmTarget::Token("t".into()), None, &silent())
                .await,
            RailOutcome::TransientRetryable
        );
    }

    #[tokio::test]
    async fn fcm_fatal_on_bad_request() {
        let (rail, _mock) = rail_with_mock(vec![CannedResponse::json(
            400,
            r#"{"error":{"code":400,"status":"INVALID_ARGUMENT"}}"#,
        )])
        .await;
        let outcome = rail
            .send(&FcmTarget::Token("t".into()), None, &silent())
            .await;
        assert!(matches!(outcome, RailOutcome::Fatal(_)));
    }

    #[tokio::test]
    async fn fcm_batch_sends_multipart_and_maps_per_part_outcomes() {
        let mut routes = HashMap::new();
        routes.insert(
            "/token".to_string(),
            queue(vec![CannedResponse::json(
                200,
                r#"{"access_token":"bt-1","expires_in":3600}"#,
            )]),
        );
        routes.insert(
            "/batch".to_string(),
            queue(vec![CannedResponse {
                status: 200,
                headers: vec![(
                    "content-type".into(),
                    format!("multipart/mixed; boundary={BATCH_BOUNDARY}"),
                )],
                body: format!(
                    "--{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: response-1\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{{\"name\":\"ok-1\"}}\r\n\
                     --{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: response-2\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{{\"error\":{{\"status\":\"UNREGISTERED\"}}}}\r\n\
                     --{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: response-3\r\n\r\nHTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\r\n{{\"error\":{{\"status\":\"RESOURCE_EXHAUSTED\"}}}}\r\n\
                     --{BATCH_BOUNDARY}--\r\n"
                )
                .into_bytes(),
            }]),
        );
        let mock = ProviderMock::start(routes).await;
        let base = mock.url("");
        let rail = FcmRail::with_endpoints(&service_account_json(), &base, mock.url("/token"))
            .expect("rail");

        let msgs: Vec<FcmMessage> = ["a", "b", "c"]
            .into_iter()
            .map(|t| FcmMessage {
                target: FcmTarget::Token(t.into()),
                collapse_key: Some("ck".into()),
                payload: silent(),
            })
            .collect();
        let outcomes = rail.send_batch(&msgs).await;
        assert_eq!(
            outcomes,
            vec![
                RailOutcome::Delivered,
                RailOutcome::Unregistered,
                RailOutcome::TransientRetryable
            ]
        );

        let requests = mock.requests();
        let batch = &requests[1];
        assert_eq!(batch.path, "/batch");
        assert_eq!(
            batch.header("content-type"),
            Some("multipart/mixed; boundary=nostos-fcm-batch-1a2b3c")
        );
        let body = batch.body_str();
        // One embedded POST per message, each carrying the message JSON.
        assert_eq!(
            body.matches("POST /v1/projects/p1/messages:send").count(),
            3
        );
        assert!(body.contains(r#""token":"a""#));
        assert!(body.contains(r#""token":"b""#));
        assert!(body.contains(r#""token":"c""#));
        assert!(body.ends_with(&format!("--{BATCH_BOUNDARY}--\r\n")));
    }

    #[tokio::test]
    async fn fcm_batch_chunks_at_max() {
        let n = FCM_BATCH_MAX + 1;
        // First response: one part per message of the first (500) chunk.
        let mut part_bodies = String::new();
        for _ in 0..FCM_BATCH_MAX {
            let _ = write!(
                part_bodies,
                "--{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\n\r\nHTTP/1.1 200 OK\r\n\r\n{{\"name\":\"x\"}}\r\n"
            );
        }
        let _ = write!(part_bodies, "--{BATCH_BOUNDARY}--\r\n");
        let mut routes = HashMap::new();
        routes.insert(
            "/token".to_string(),
            queue(vec![CannedResponse::json(
                200,
                r#"{"access_token":"bt-1","expires_in":3600}"#,
            )]),
        );
        routes.insert(
            "/batch".to_string(),
            queue(vec![
                CannedResponse {
                    status: 200,
                    headers: vec![(
                        "content-type".into(),
                        format!("multipart/mixed; boundary={BATCH_BOUNDARY}"),
                    )],
                    body: part_bodies.into_bytes(),
                },
                CannedResponse {
                    status: 200,
                    headers: vec![(
                        "content-type".into(),
                        format!("multipart/mixed; boundary={BATCH_BOUNDARY}"),
                    )],
                    body: format!(
                        "--{BATCH_BOUNDARY}\r\nContent-Type: application/http\r\n\r\nHTTP/1.1 200 OK\r\n\r\n{{\"name\":\"x\"}}\r\n--{BATCH_BOUNDARY}--\r\n"
                    )
                    .into_bytes(),
                },
            ]),
        );
        let mock = ProviderMock::start(routes).await;
        let base = mock.url("");
        let rail = FcmRail::with_endpoints(&service_account_json(), &base, mock.url("/token"))
            .expect("rail");

        let msgs: Vec<FcmMessage> = (0..n)
            .map(|i| FcmMessage {
                target: FcmTarget::Token(format!("t{i}")),
                collapse_key: None,
                payload: silent(),
            })
            .collect();
        let outcomes = rail.send_batch(&msgs).await;
        assert_eq!(outcomes.len(), n);
        assert!(outcomes.iter().all(|o| *o == RailOutcome::Delivered));
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|r| r.path == "/batch")
                .count(),
            2,
            "501 messages must split into two batch requests"
        );
    }

    /// Real-rail smoke — self-skips, mirrors `NOSTOS_E2E_PG`:
    /// `NOSTOS_E2E_FCM=1` + `NOSTOS_FCM_CREDENTIALS_JSON` +
    /// `NOSTOS_E2E_FCM_TOKEN`.
    #[tokio::test]
    async fn e2e_fcm_smoke() {
        if std::env::var("NOSTOS_E2E_FCM").is_err() {
            eprintln!("skipping (set NOSTOS_E2E_FCM=1 with NOSTOS_FCM_CREDENTIALS_JSON and NOSTOS_E2E_FCM_TOKEN to run)");
            return;
        }
        let rail = FcmRail::from_env()
            .expect("credentials parse")
            .expect("rail configured");
        let token = std::env::var("NOSTOS_E2E_FCM_TOKEN").expect("NOSTOS_E2E_FCM_TOKEN");
        let outcome = rail
            .send(
                &FcmTarget::Token(token),
                Some("nostos-e2e"),
                &PushPayload::Silent {
                    table: "e2e".into(),
                    lsn: Lsn::new(1),
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
    }
}
