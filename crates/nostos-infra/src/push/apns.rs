//! APNs rail (ADR-0037 §1, plan task 2.2) — token-based auth (`.p8` key).
//!
//! Deliberately NOT the `a2` crate: an APNs send is one HTTPS POST with five
//! headers, and `jsonwebtoken` (ES256, already a direct dep) plus the shared
//! `reqwest` client cover it. `a2` would add a second HTTP stack (hyper +
//! its own client) for no capability gain, and its client is far harder to
//! point at a test fixture than a plain reqwest URL (kimitail: no new dep
//! for what a few lines do). The workspace `reqwest` carries the `http2`
//! feature so ALPN negotiates the h2 connection APNs requires.
//!
//! - silent: `apns-push-type: background` + `apns-priority: 5` (the pairing
//!   Apple mandates for background pushes) + `apns-expiration: 0`
//!   (deliver-now-or-discard — a stale ping is worthless, ADR-0037 §4) and
//!   payload `{"aps":{"content-available":1}, "table", "lsn"}`.
//! - visible: `alert` push-type + priority 10 + expiration `now + 1h` and
//!   payload `{"aps":{"alert":{"title","body"}}}` (already interpolated —
//!   template resolution is task 2.4).
//! - `apns-collapse-id` (≤64 bytes, truncated on char boundary) carries the
//!   per-(device, subscription) supersede key; `apns-topic` = bundle id.
//! - Provider JWT (`ES256`, `kid` = key id, `iss` = team id, no `exp` —
//!   Apple caps validity at 1h from `iat`) cached 50 minutes.
//! - 410 (reason `Unregistered`) → [`RailOutcome::Unregistered`] — the
//!   `PgTokenStore` prune trigger.

use std::time::{Duration, Instant};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tracing::warn;

use super::{env_nonempty, http_client, PushPayload, PushRailError, RailOutcome, VISIBLE_TTL_SECS};

const APNS_BASE: &str = "https://api.push.apple.com";
const APNS_SANDBOX_BASE: &str = "https://api.sandbox.push.apple.com";
/// Apple: provider tokens are valid 1h and should be refreshed no more often
/// than every 20 minutes; 50min sits between.
const JWT_TTL: Duration = Duration::from_mins(50);

pub struct ApnsRail {
    http: reqwest::Client,
    base: String,
    bundle_id: String,
    key_id: String,
    team_id: String,
    encoding_key: EncodingKey,
    /// `(jwt, valid_until)` — building the JWT is pure CPU, so a std Mutex.
    jwt_cache: std::sync::Mutex<Option<(String, Instant)>>,
}

impl ApnsRail {
    /// Build with the production endpoint.
    ///
    /// # Errors
    /// [`PushRailError`] if `p8_pem` is not a PEM EC (P-256) key.
    pub fn new(
        p8_pem: &str,
        key_id: &str,
        team_id: &str,
        bundle_id: &str,
        sandbox: bool,
    ) -> Result<Self, PushRailError> {
        Self::with_base(
            p8_pem,
            key_id,
            team_id,
            bundle_id,
            if sandbox {
                APNS_SANDBOX_BASE
            } else {
                APNS_BASE
            }
            .to_string(),
        )
    }

    /// `NOSTOS_APNS_*` — `Ok(None)` (rail off) when none are set. `p8` accepts
    /// the PEM inline (secret-manager style) or a filesystem path.
    ///
    /// # Errors
    /// [`PushRailError`] when partially set, unreadable, or invalid.
    pub fn from_env() -> Result<Option<Self>, PushRailError> {
        let p8 = env_nonempty("NOSTOS_APNS_KEY_P8");
        let key_id = env_nonempty("NOSTOS_APNS_KEY_ID");
        let team_id = env_nonempty("NOSTOS_APNS_TEAM_ID");
        let bundle_id = env_nonempty("NOSTOS_APNS_BUNDLE_ID");
        if p8.is_none() && key_id.is_none() && team_id.is_none() && bundle_id.is_none() {
            return Ok(None);
        }
        let (Some(p8), Some(key_id), Some(team_id), Some(bundle_id)) =
            (p8, key_id, team_id, bundle_id)
        else {
            return Err(PushRailError(
                "apns rail: NOSTOS_APNS_KEY_P8, NOSTOS_APNS_KEY_ID, NOSTOS_APNS_TEAM_ID and \
                 NOSTOS_APNS_BUNDLE_ID must be set together"
                    .into(),
            ));
        };
        let pem = if p8.contains("-----BEGIN") {
            p8
        } else {
            std::fs::read_to_string(&p8)
                .map_err(|e| PushRailError(format!("NOSTOS_APNS_KEY_P8: cannot read {p8}: {e}")))?
        };
        let sandbox = std::env::var("NOSTOS_APNS_SANDBOX").is_ok_and(|v| v == "1");
        Self::new(&pem, &key_id, &team_id, &bundle_id, sandbox).map(Some)
    }

    fn with_base(
        p8_pem: &str,
        key_id: &str,
        team_id: &str,
        bundle_id: &str,
        base: String,
    ) -> Result<Self, PushRailError> {
        let encoding_key = EncodingKey::from_ec_pem(p8_pem.as_bytes())
            .map_err(|e| PushRailError(format!("apns p8 key: {e}")))?;
        Ok(Self {
            http: http_client(),
            base,
            bundle_id: bundle_id.to_string(),
            key_id: key_id.to_string(),
            team_id: team_id.to_string(),
            encoding_key,
            jwt_cache: std::sync::Mutex::new(None),
        })
    }

    /// Send one push to a 64-hex-class device token.
    pub async fn send(
        &self,
        device_token: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        if device_token.is_empty() || !device_token.bytes().all(|b| b.is_ascii_hexdigit()) {
            // Length only — never echo token material into logs.
            return RailOutcome::Fatal(format!(
                "apns device token is not hex (len {})",
                device_token.len()
            ));
        }
        let jwt = match self.provider_token() {
            Ok(t) => t,
            Err(e) => return RailOutcome::Fatal(e.to_string()),
        };
        let (push_type, priority, expiration, body) = match payload {
            PushPayload::Silent { table, lsn } => (
                "background",
                "5",
                0u64,
                json!({
                    "aps": { "content-available": 1 },
                    "table": table,
                    "lsn": lsn.0.to_string()
                }),
            ),
            PushPayload::Visible { title, body } => (
                "alert",
                "10",
                jsonwebtoken::get_current_timestamp() + u64::from(VISIBLE_TTL_SECS),
                json!({ "aps": { "alert": { "title": title, "body": body } } }),
            ),
        };
        let url = format!("{}/3/device/{device_token}", self.base);
        let mut req = self
            .http
            .post(&url)
            .bearer_auth(&jwt)
            .header("apns-push-type", push_type)
            .header("apns-priority", priority)
            .header("apns-expiration", expiration.to_string())
            .header("apns-topic", &self.bundle_id);
        if let Some(key) = collapse_key {
            // ≤64 bytes per Apple; chars() can only shrink below that.
            let cid: String = key.chars().take(64).collect();
            req = req.header("apns-collapse-id", cid);
        }
        match req.json(&body).send().await {
            Ok(resp) => outcome_for(resp).await,
            Err(e) => {
                warn!(%e, "apns send network error");
                RailOutcome::TransientRetryable
            }
        }
    }

    /// Cached provider JWT. Apple rejects tokens >1h old and rate-limits
    /// refreshes, so one token lives 50 minutes here.
    fn provider_token(&self) -> Result<String, PushRailError> {
        let mut cache = self.jwt_cache.lock().expect("apns jwt cache");
        if let Some((jwt, valid_until)) = cache.as_ref() {
            if *valid_until > Instant::now() {
                return Ok(jwt.clone());
            }
        }
        let claims = json!({ "iss": self.team_id, "iat": jsonwebtoken::get_current_timestamp() });
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let jwt = encode(&header, &claims, &self.encoding_key)
            .map_err(|e| PushRailError(format!("apns jwt sign: {e}")))?;
        *cache = Some((jwt.clone(), Instant::now() + JWT_TTL));
        Ok(jwt)
    }
}

/// APNs status → outcome. 410 is always `Unregistered` (reason
/// `BadCollapseId`-style 400s and auth 403s are Fatal with the reason body
/// included — diagnosing APNs config without the reason string is hell).
async fn outcome_for(resp: reqwest::Response) -> RailOutcome {
    let status = resp.status();
    if status.is_success() {
        return RailOutcome::Delivered;
    }
    let text = resp.text().await.unwrap_or_default();
    let reason = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["reason"].as_str().map(str::to_string))
        .unwrap_or_default();
    match status.as_u16() {
        410 => RailOutcome::Unregistered,
        429 | 500 | 502 | 503 | 504 => RailOutcome::TransientRetryable,
        _ => RailOutcome::Fatal(format!("apns {status} {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::test_support::{jwt_parts, pkcs8_pem, CannedResponse, ProviderMock};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey;
    use rand::thread_rng;
    use std::collections::{HashMap, VecDeque};

    fn p8_pem() -> String {
        let key = SigningKey::random(&mut thread_rng());
        pkcs8_pem(key.to_pkcs8_der().expect("pkcs8").as_bytes())
    }

    const TOKEN: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    async fn rail_with(responses: Vec<CannedResponse>) -> (ApnsRail, ProviderMock) {
        let mut routes = HashMap::new();
        routes.insert(
            format!("/3/device/{TOKEN}"),
            responses.into_iter().collect::<VecDeque<CannedResponse>>(),
        );
        let mock = ProviderMock::start(routes).await;
        let rail = ApnsRail::with_base(
            &p8_pem(),
            "KID123",
            "TEAM456",
            "run.nostos.app",
            mock.url(""),
        )
        .expect("rail");
        (rail, mock)
    }

    fn silent() -> PushPayload {
        PushPayload::Silent {
            table: "tasks".into(),
            lsn: nostos_domain::Lsn::new(777),
        }
    }

    #[tokio::test]
    async fn apns_silent_background_headers_and_payload() {
        let (rail, mock) = rail_with(vec![CannedResponse::json(200, "")]).await;
        let outcome = rail.send(TOKEN, Some("sync/tasks"), &silent()).await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let requests = mock.requests();
        let req = &requests[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, format!("/3/device/{TOKEN}"));
        assert_eq!(req.header("apns-push-type"), Some("background"));
        assert_eq!(req.header("apns-priority"), Some("5"));
        assert_eq!(req.header("apns-expiration"), Some("0"));
        assert_eq!(req.header("apns-collapse-id"), Some("sync/tasks"));
        assert_eq!(req.header("apns-topic"), Some("run.nostos.app"));
        assert!(req
            .header("authorization")
            .is_some_and(|a| a.starts_with("Bearer ")));
        assert_eq!(
            req.json(),
            json!({
                "aps": { "content-available": 1 },
                "table": "tasks",
                "lsn": "777"
            })
        );
    }

    #[tokio::test]
    async fn apns_visible_alert_headers_and_payload() {
        let (rail, mock) = rail_with(vec![CannedResponse::json(200, "")]).await;
        let before = jsonwebtoken::get_current_timestamp();
        let outcome = rail
            .send(
                TOKEN,
                None,
                &PushPayload::Visible {
                    title: "Tasks changed".into(),
                    body: "New items to sync".into(),
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let requests = mock.requests();
        let req = &requests[0];
        assert_eq!(req.header("apns-push-type"), Some("alert"));
        assert_eq!(req.header("apns-priority"), Some("10"));
        let expiration: u64 = req
            .header("apns-expiration")
            .expect("expiration")
            .parse()
            .expect("expiration is unix ts");
        assert!(
            (before + 3500..=before + 3700).contains(&expiration),
            "visible expiration should be ~now+1h, got {expiration}"
        );
        assert_eq!(req.header("apns-collapse-id"), None);
        assert_eq!(
            req.json(),
            json!({ "aps": { "alert": { "title": "Tasks changed", "body": "New items to sync" } } })
        );
    }

    #[tokio::test]
    async fn apns_provider_jwt_kid_team_and_cache() {
        let (rail, mock) = rail_with(vec![
            CannedResponse::json(200, ""),
            CannedResponse::json(200, ""),
        ])
        .await;
        for _ in 0..2 {
            assert_eq!(
                rail.send(TOKEN, None, &silent()).await,
                RailOutcome::Delivered
            );
        }
        let requests = mock.requests();
        let first = requests[0].header("authorization").expect("bearer");
        let second = requests[1].header("authorization").expect("bearer");
        assert_eq!(first, second, "provider jwt must be cached, not re-minted");
        let jwt = first.strip_prefix("Bearer ").expect("bearer prefix");
        let (header, claims) = jwt_parts(jwt);
        assert_eq!(header["alg"], json!("ES256"));
        assert_eq!(header["kid"], json!("KID123"));
        assert_eq!(claims["iss"], json!("TEAM456"));
    }

    #[tokio::test]
    async fn apns_gone_410_maps_to_prune_trigger() {
        let (rail, _requests) = rail_with(vec![CannedResponse::json(
            410,
            r#"{"reason":"Unregistered"}"#,
        )])
        .await;
        assert_eq!(
            rail.send(TOKEN, None, &silent()).await,
            RailOutcome::Unregistered
        );
    }

    #[tokio::test]
    async fn apns_transient_and_fatal_statuses() {
        let (rail, _requests) = rail_with(vec![
            CannedResponse::json(503, r#"{"reason":"ServiceUnavailable"}"#),
            CannedResponse::json(429, r#"{"reason":"TooManyRequests"}"#),
            CannedResponse::json(403, r#"{"reason":"InvalidProviderToken"}"#),
        ])
        .await;
        assert_eq!(
            rail.send(TOKEN, None, &silent()).await,
            RailOutcome::TransientRetryable
        );
        assert_eq!(
            rail.send(TOKEN, None, &silent()).await,
            RailOutcome::TransientRetryable
        );
        let fatal = rail.send(TOKEN, None, &silent()).await;
        assert!(matches!(fatal, RailOutcome::Fatal(ref m) if m.contains("InvalidProviderToken")));
    }

    #[tokio::test]
    async fn apns_non_hex_token_is_fatal_without_a_request() {
        let (rail, mock) = rail_with(vec![CannedResponse::json(200, "")]).await;
        let outcome = rail.send("not-hex!", None, &silent()).await;
        assert!(matches!(outcome, RailOutcome::Fatal(_)));
        assert!(
            mock.requests().is_empty(),
            "invalid token must not hit the network"
        );
    }

    #[tokio::test]
    async fn apns_collapse_id_truncated_to_64_bytes() {
        let (rail, mock) = rail_with(vec![CannedResponse::json(200, "")]).await;
        let long_key: String = "x".repeat(80);
        assert_eq!(
            rail.send(TOKEN, Some(&long_key), &silent()).await,
            RailOutcome::Delivered
        );
        let requests = mock.requests();
        assert_eq!(
            requests[0].header("apns-collapse-id").map_or(0, str::len),
            64,
            "collapse id must be truncated to Apple's 64-byte cap"
        );
    }

    /// Real-rail smoke — self-skips, mirrors `NOSTOS_E2E_PG`:
    /// `NOSTOS_E2E_APNS=1` + the four `NOSTOS_APNS_*` vars +
    /// `NOSTOS_E2E_APNS_TOKEN` (64-hex device token).
    #[tokio::test]
    async fn e2e_apns_smoke() {
        if std::env::var("NOSTOS_E2E_APNS").is_err() {
            eprintln!(
                "skipping (set NOSTOS_E2E_APNS=1 with NOSTOS_APNS_* and NOSTOS_E2E_APNS_TOKEN to run)"
            );
            return;
        }
        let rail = ApnsRail::from_env()
            .expect("config parses")
            .expect("rail configured");
        let token = std::env::var("NOSTOS_E2E_APNS_TOKEN").expect("NOSTOS_E2E_APNS_TOKEN");
        let outcome = rail
            .send(
                &token,
                Some("nostos-e2e"),
                &PushPayload::Silent {
                    table: "e2e".into(),
                    lsn: nostos_domain::Lsn::new(1),
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
    }
}
