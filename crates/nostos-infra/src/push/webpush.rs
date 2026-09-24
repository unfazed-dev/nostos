//! Web Push rail (ADR-0037 §1/§5, plan task 2.3) — direct VAPID, no FCM
//! intermediary on the web rail.
//!
//! `web-push` 0.11 provides the two genuinely hard parts — RFC 8188/8291
//! `aes128gcm` payload encryption and RFC 8292 VAPID ES256 signing — with its
//! built-in HTTP clients disabled (`default-features = false`): nostos sends
//! over the shared reqwest client using the crate's public-field message
//! API, so `IsahcWebPushClient` would only add a curl stack for one POST.
//! CVE-2025-53604 (RUSTSEC-2025-0015 — Content-Length OOM in those built-in
//! clients) is patched `>=0.10.3`; the workspace pins 0.11.
//!
//! - RFC 8030 headers: `TTL` (required — 60s silent / 1h visible),
//!   `Urgency` (`low` silent / `high` visible), `Topic` (the collapse key —
//!   at most 32 base64url characters, dropped with a warning if longer).
//! - The registry token for this rail is the browser `pushSubscription`
//!   JSON (`{endpoint, keys: {p256dh, auth}}`).
//! - The endpoint must be `https://` AND must not resolve to a
//!   loopback/private/link-local/unique-local address — tokens are
//!   client-registered and untrusted, so these two checks are the SSRF
//!   guard on the one URL nostos does not construct itself (M3).
//! - 404/410 → [`RailOutcome::Unregistered`] (prune trigger); 429/5xx →
//!   retryable.
//!
//! KNOWN BROWSER LIMITS (documented in docs/push.md § "Known Web Push
//! limitations", deliberate non-goals): a killed tab renders the visible
//! notification but a service worker cannot hold the WS sync session, so
//! data reconciles on next foreground (silent doorbells to killed tabs are
//! effectively lost); and nostos never learns of `pushsubscriptionchange`
//! rotations — the stale subscription 404/410-prunes and the APP must
//! re-register the fresh subscription JSON (registerPushToken("webpush", …)).

use serde_json::json;
use tracing::warn;
use web_push::{
    ContentEncoding, SubscriptionInfo, SubscriptionKeys, Urgency, VapidSignature,
    VapidSignatureBuilder, WebPushMessageBuilder,
};

use super::{env_nonempty, http_client, PushPayload, PushRailError, RailOutcome};

pub struct WebPushRail {
    http: reqwest::Client,
    vapid_key_b64: String,
    subject: String,
    /// Production rejects non-`https://` endpoints (SSRF guard); test-only
    /// seam for the plain-HTTP fixture server.

    #[cfg(test)]
    allow_http_endpoints: bool,
}

/// The browser `pushSubscription` JSON stored as this rail's token.
#[derive(serde::Deserialize)]
struct WebSubscription {
    endpoint: String,
    keys: WebSubscriptionKeys,
}

#[derive(serde::Deserialize)]
struct WebSubscriptionKeys {
    p256dh: String,
    auth: String,
}

impl WebPushRail {
    /// Build with a VAPID private key as base64url of the 32-byte P-256
    /// scalar (the format VAPID key generators and the other big push
    /// libraries exchange) and a `mailto:` contact subject.
    ///
    /// # Errors
    /// [`PushRailError`] if the key does not parse as a P-256 scalar.
    pub fn new(vapid_private_key_b64: &str, subject: &str) -> Result<Self, PushRailError> {
        // Fail fast: parse once against a throwaway subscription.
        let probe = SubscriptionInfo::new("https://probe.invalid", "probe", "probe");
        VapidSignatureBuilder::from_base64(vapid_private_key_b64, &probe)
            .map_err(|e| PushRailError(format!("vapid private key: {e}")))?;
        Ok(Self {
            http: http_client(),
            vapid_key_b64: vapid_private_key_b64.to_string(),
            subject: subject.to_string(),
            #[cfg(test)]
            allow_http_endpoints: false,
        })
    }

    /// Test seam over [`WebPushRail::new`]: accepts the fixture server's
    /// plain-`http://` endpoints. Production never calls this.
    #[cfg(test)]
    fn new_allowing_http(
        vapid_private_key_b64: &str,
        subject: &str,
    ) -> Result<Self, PushRailError> {
        let mut rail = Self::new(vapid_private_key_b64, subject)?;
        rail.allow_http_endpoints = true;
        Ok(rail)
    }

    /// `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` + `NOSTOS_WEBPUSH_VAPID_SUBJECT` —
    /// `Ok(None)` (rail off) when the key is unset.
    ///
    /// # Errors
    /// [`PushRailError`] when the key is set but the subject (or key) is not
    /// usable.
    pub fn from_env() -> Result<Option<Self>, PushRailError> {
        let Some(key) = env_nonempty("NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY") else {
            return Ok(None);
        };
        let subject = env_nonempty("NOSTOS_WEBPUSH_VAPID_SUBJECT").ok_or_else(|| {
            PushRailError(
                "webpush rail: NOSTOS_WEBPUSH_VAPID_SUBJECT (mailto:) is required when \
                 NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY is set"
                    .into(),
            )
        })?;
        Self::new(&key, &subject).map(Some)
    }

    /// Send one push to a `pushSubscription` JSON token.
    pub async fn send(
        &self,
        subscription_json: &str,
        collapse_key: Option<&str>,
        payload: &PushPayload,
    ) -> RailOutcome {
        let sub: WebSubscription = match serde_json::from_str(subscription_json) {
            Ok(s) => s,
            Err(e) => return RailOutcome::Fatal(format!("webpush subscription json: {e}")),
        };
        let endpoint_ok = sub.endpoint.starts_with("https://");
        // Test-only escape for the plain-HTTP fixture server (see field
        // doc). The fixture binds loopback over plain HTTP, so this skips
        // BOTH the https guard and the private-target SSRF guard.
        #[cfg(test)]
        let fixture_override = self.allow_http_endpoints;
        #[cfg(not(test))]
        let fixture_override = false;
        if !endpoint_ok && !fixture_override {
            return RailOutcome::Fatal("webpush endpoint must be https".into());
        }
        // M3 SSRF guard: the `https://` check alone still lets a client-
        // registered subscription point the server at internal services —
        // resolve the host and refuse loopback/private/link-local/
        // unique-local targets before any request leaves. Fails closed
        // (an unresolvable host is refused too).
        if !fixture_override {
            if let Err(reason) = endpoint_target_allowed(&sub.endpoint).await {
                return RailOutcome::Fatal(reason);
            }
        }
        let info = SubscriptionInfo {
            endpoint: sub.endpoint,
            keys: SubscriptionKeys {
                p256dh: b64url_no_pad(&sub.keys.p256dh),
                auth: b64url_no_pad(&sub.keys.auth),
            },
        };
        let signature = match self.vapid_signature(&info) {
            Ok(s) => s,
            Err(e) => return RailOutcome::Fatal(e.to_string()),
        };
        // The Authorization header is built from the signature's public
        // fields (RFC 8292 §2) before the builder consumes it.
        let authorization = format!(
            "vapid t={}, k={}",
            signature.auth_t,
            b64url(&signature.auth_k)
        );

        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_vapid_signature(signature);
        builder.set_ttl(payload.ttl_secs());
        builder.set_urgency(match payload {
            PushPayload::Silent { .. } => Urgency::Low,
            PushPayload::Visible { .. } => Urgency::High,
        });
        match collapse_key {
            Some(key) if valid_topic(key) => {
                builder.set_topic(key.to_string());
            }
            Some(key) => {
                warn!(key, "webpush collapse key is not a valid Topic (<=32 base64url chars); sending without");
            }
            None => {}
        }
        let plain = plain_payload(payload);
        // set_payload borrows; the buffer must outlive build() (encryption
        // happens there).
        let plain_bytes = serde_json::to_vec(&plain).unwrap_or_default();
        builder.set_payload(ContentEncoding::Aes128Gcm, &plain_bytes);
        let message = match builder.build() {
            Ok(m) => m,
            Err(e) => return RailOutcome::Fatal(format!("webpush payload build: {e}")),
        };

        let mut req = self
            .http
            .post(message.endpoint.to_string())
            .header("TTL", message.ttl.to_string())
            .header("Authorization", authorization);
        if let Some(urgency) = message.urgency {
            req = req.header("Urgency", urgency_header(urgency));
        }
        if let Some(topic) = message.topic.as_deref() {
            req = req.header("Topic", topic);
        }
        if let Some(encrypted) = message.payload.as_ref() {
            req = req
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .body(encrypted.content.clone());
        }
        match req.send().await {
            Ok(resp) => match resp.status().as_u16() {
                200 | 201 => RailOutcome::Delivered,
                404 | 410 => RailOutcome::Unregistered,
                429 | 500 | 502 | 503 | 504 => RailOutcome::TransientRetryable,
                status => RailOutcome::Fatal(format!("webpush {status}")),
            },
            Err(e) => {
                warn!(%e, "webpush send network error");
                RailOutcome::TransientRetryable
            }
        }
    }

    fn vapid_signature(&self, info: &SubscriptionInfo) -> Result<VapidSignature, PushRailError> {
        let mut builder = VapidSignatureBuilder::from_base64(&self.vapid_key_b64, info)
            .map_err(|e| PushRailError(format!("vapid key: {e}")))?;
        builder.add_claim("sub", self.subject.as_str());
        builder
            .build()
            .map_err(|e| PushRailError(format!("vapid signature: {e}")))
    }
}

/// web-push 0.11 decodes `p256dh`/`auth` as base64url without padding, while
/// browsers hand out standard base64 (`+/=`) in `pushSubscription`. This
/// translate is lossless both ways — a no-op on already-url-safe input.
fn b64url_no_pad(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            _ => c,
        })
        .filter(|c| *c != '=')
        .collect()
}

/// M3 SSRF guard: resolve `endpoint`'s host and refuse it when ANY resolved
/// address is loopback / private / link-local / unique-local (IPv4 + IPv6,
/// IPv4-mapped IPv6 included). The token is client-registered, so a
/// subscription could otherwise point the server at internal services even
/// over `https://`. Fails closed — a host that cannot be resolved is refused
/// too. ponytail: resolve-then-check, not resolve-then-pin — a rebinding DNS
/// answer could still race the connect; pin the resolved IP on reqwest if a
/// threat model ever demands it.
async fn endpoint_target_allowed(endpoint: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(endpoint).map_err(|e| format!("webpush endpoint url: {e}"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let Some(host) = url.host_str() else {
        return Err("webpush endpoint has no host".into());
    };
    // `host_str` returns IPv6 literals bracketed (`[::1]`) — strip to parse.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ips: Vec<std::net::IpAddr> = if let Ok(ip) = host.parse() {
        vec![ip]
    } else {
        match tokio::net::lookup_host((host, port)).await {
            Ok(resolved) => resolved.map(|sa| sa.ip()).collect(),
            Err(e) => {
                return Err(format!(
                    "webpush endpoint host {host:?} cannot be resolved ({e}); refusing"
                ));
            }
        }
    };
    if ips.is_empty() {
        return Err(format!(
            "webpush endpoint host {host:?} resolves to nothing; refusing"
        ));
    }
    for ip in ips {
        if is_private_target(ip) {
            return Err(format!(
                "webpush endpoint resolves to a refused address ({ip}); refusing"
            ));
        }
    }
    Ok(())
}

/// The refused ranges (M3): loopback, private, link-local, unique-local, and
/// the unspecified/broadcast extremes — all std predicates, no new deps.
fn is_private_target(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4_refused(v4),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                // IPv4-mapped (::ffff:a.b.c.d) inherits the IPv4 verdict.
                || v6.to_ipv4_mapped().is_some_and(v4_refused)
        }
    }
}

fn v4_refused(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
}

/// RFC 8030 §5.4: Topic is at most 32 base64url characters.
fn valid_topic(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 32
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn urgency_header(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::VeryLow => "very-low",
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::High => "high",
    }
}

/// Minimal base64url encode, no padding (RFC 8292 `k=` field). Hand-rolled
/// per this crate's standing preference (jwks.rs) over a base64 dependency
/// for two call sites.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[(n >> 18 & 0x3F) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// The (soon to be encrypted) JSON a service worker receives. Extracted so
/// the shape is testable: everything after this is ECE ciphertext.
fn plain_payload(payload: &PushPayload) -> serde_json::Value {
    match payload {
        PushPayload::Silent { table, lsn } => {
            json!({ "table": table, "lsn": lsn.0.to_string() })
        }
        PushPayload::Visible {
            title,
            body,
            category,
            data,
            options,
        } => {
            // `category` rides the (encrypted) payload so the client SW
            // can map it to action buttons — same field FCM/APNs carry.
            let mut plain = json!({ "title": title, "body": body });
            if let Some(c) = category {
                plain["category"] = json!(c);
            }
            // Nested, unlike APNs: a Web Push payload has no vendor envelope
            // to sit beside, and `event.data.json().data` is what a service
            // worker reads — the same nesting the FCM JS SDK hands
            // `onBackgroundMessage`.
            if !data.is_empty() {
                plain["data"] = json!(data);
            }
            // Options under the Notification API's own names (ADR-0047), so
            // a service worker's `showNotification(p.title, p)` renders them
            // with no mapping of its own.
            for (key, name) in [("image", "image"), ("collapse", "tag"), ("avatar", "icon")] {
                if let Some(value) = options.get(key) {
                    plain[name] = json!(value);
                }
            }
            if options.get("sound").is_some_and(|s| s == "none") {
                plain["silent"] = json!(true);
            }
            plain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::test_support::{b64_encode, CannedResponse, ProviderMock};
    use p256::ecdsa::SigningKey;
    use rand::{thread_rng, RngCore};
    use std::collections::{HashMap, VecDeque};

    /// Mint a VAPID server key; returns (base64url scalar, expected `k=`
    /// header value = b64url of the uncompressed public point).
    fn vapid_key() -> (String, String) {
        let key = SigningKey::random(&mut thread_rng());
        let scalar = key.as_nonzero_scalar().to_bytes().to_vec();
        let point = key.verifying_key().to_encoded_point(false);
        (
            b64_encode(&scalar, true, false),
            b64_encode(point.as_bytes(), true, false),
        )
    }

    /// Mint a browser-side subscription (real P-256 keypair — the payload
    /// encryption actually runs ECDH against `p256dh`).
    fn subscription_json(endpoint: &str) -> String {
        let key = SigningKey::random(&mut thread_rng());
        let point = key.verifying_key().to_encoded_point(false);
        let mut auth = [0u8; 16];
        thread_rng().fill_bytes(&mut auth);
        json!({
            "endpoint": endpoint,
            "keys": {
                "p256dh": b64_encode(point.as_bytes(), false, true),
                "auth": b64_encode(&auth, false, true),
            }
        })
        .to_string()
    }

    async fn rail_with(status: u16) -> (WebPushRail, String, ProviderMock) {
        let mut routes = HashMap::new();
        routes.insert(
            "/push/abc".to_string(),
            VecDeque::from(vec![CannedResponse::json(status, "")]),
        );
        let mock = ProviderMock::start(routes).await;
        let (key_b64, _) = vapid_key();
        let rail =
            WebPushRail::new_allowing_http(&key_b64, "mailto:ops@example.com").expect("rail");
        (rail, subscription_json(&mock.url("/push/abc")), mock)
    }

    fn silent() -> PushPayload {
        PushPayload::Silent {
            table: "tasks".into(),
            lsn: nostos_domain::Lsn::new(42),
        }
    }

    #[tokio::test]
    async fn webpush_silent_topic_urgency_ttl_and_encrypted_payload() {
        let (key_b64, expected_k) = vapid_key();
        let mut routes = HashMap::new();
        routes.insert(
            "/push/abc".to_string(),
            VecDeque::from(vec![CannedResponse::json(201, "")]),
        );
        let mock = ProviderMock::start(routes).await;
        let rail =
            WebPushRail::new_allowing_http(&key_b64, "mailto:ops@example.com").expect("rail");
        let sub = subscription_json(&mock.url("/push/abc"));

        let outcome = rail.send(&sub, Some("sync-tasks"), &silent()).await;
        assert_eq!(outcome, RailOutcome::Delivered);

        let requests = mock.requests();
        let req = &requests[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/push/abc");
        assert_eq!(req.header("ttl"), Some("60"));
        assert_eq!(req.header("urgency"), Some("low"));
        assert_eq!(req.header("topic"), Some("sync-tasks"));
        let auth = req.header("authorization").expect("authorization");
        let rest = auth
            .strip_prefix("vapid t=")
            .and_then(|t| t.split_once(", k="))
            .expect("vapid t=..., k=... shape");
        assert!(!rest.0.is_empty(), "vapid jwt present");
        assert_eq!(rest.1, expected_k, "vapid k= is the b64url public point");
        assert_eq!(req.header("content-encoding"), Some("aes128gcm"));
        assert_eq!(req.header("content-type"), Some("application/octet-stream"));
        assert!(
            !req.body.is_empty(),
            "doorbell payload is present, encrypted"
        );
    }

    #[tokio::test]
    async fn webpush_visible_high_urgency_long_ttl() {
        let (rail, sub, mock) = rail_with(201).await;
        let outcome = rail
            .send(
                &sub,
                Some("sync-tasks"),
                &PushPayload::Visible {
                    title: "Tasks changed".into(),
                    body: "New items to sync".into(),
                    category: None,
                    data: std::collections::BTreeMap::new(),
                    options: std::collections::BTreeMap::new(),
                },
            )
            .await;
        assert_eq!(outcome, RailOutcome::Delivered);
        let requests = mock.requests();
        let req = &requests[0];
        assert_eq!(req.header("urgency"), Some("high"));
        assert_eq!(req.header("ttl"), Some("3600"));
        assert_eq!(req.header("topic"), Some("sync-tasks"));
    }

    #[test]
    fn webpush_routing_keys_nest_under_data() {
        let plain = plain_payload(&PushPayload::Visible {
            title: "Order shipped".into(),
            body: "On its way".into(),
            category: Some("order_status".into()),
            data: [("cairn_route".to_string(), "/orders/42".to_string())]
                .into_iter()
                .collect(),
            options: std::collections::BTreeMap::new(),
        });
        assert_eq!(
            plain,
            json!({
                "title": "Order shipped", "body": "On its way",
                "category": "order_status",
                "data": { "cairn_route": "/orders/42" },
            })
        );
        // No routing keys = no `data` key at all: an SW that checks for it
        // must not find an empty object.
        assert!(plain_payload(&silent()).get("data").is_none());
    }

    #[test]
    fn webpush_options_use_the_notification_api_names() {
        let plain = plain_payload(&PushPayload::Visible {
            title: "T".into(),
            body: "B".into(),
            category: None,
            data: std::collections::BTreeMap::new(),
            options: [
                ("collapse", "order-42"),
                ("image", "https://cdn.example/i.png"),
                ("sender", "Ada"),
                ("avatar", "https://cdn.example/ada.png"),
                ("sound", "none"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        });
        assert_eq!(
            plain,
            json!({
                "title": "T", "body": "B",
                "image": "https://cdn.example/i.png",
                "tag": "order-42",
                "icon": "https://cdn.example/ada.png",
                "silent": true
            })
        );
    }

    #[tokio::test]
    async fn webpush_gone_and_not_found_map_to_prune_trigger() {
        let (rail, sub, _) = rail_with(410).await;
        assert_eq!(
            rail.send(&sub, None, &silent()).await,
            RailOutcome::Unregistered
        );
        let (rail, sub, _) = rail_with(404).await;
        assert_eq!(
            rail.send(&sub, None, &silent()).await,
            RailOutcome::Unregistered
        );
    }

    #[tokio::test]
    async fn webpush_rate_limited_is_retryable() {
        let (rail, sub, _) = rail_with(429).await;
        assert_eq!(
            rail.send(&sub, None, &silent()).await,
            RailOutcome::TransientRetryable
        );
    }

    #[tokio::test]
    async fn webpush_oversized_topic_dropped_not_fatal() {
        let (rail, sub, mock) = rail_with(201).await;
        let long_key = "k".repeat(40);
        assert_eq!(
            rail.send(&sub, Some(&long_key), &silent()).await,
            RailOutcome::Delivered
        );
        let requests = mock.requests();
        assert_eq!(
            requests[0].header("topic"),
            None,
            "a >32-char key must not be sent as Topic"
        );
        assert_eq!(requests[0].header("ttl"), Some("60"));
    }

    #[tokio::test]
    async fn webpush_non_https_endpoint_rejected_without_a_request() {
        let (key_b64, _) = vapid_key();
        let rail = WebPushRail::new(&key_b64, "mailto:ops@example.com").expect("rail");
        let mock = ProviderMock::start(HashMap::new()).await;
        // ProviderMock serves plain http:// — exactly the non-https shape the
        // guard must reject before any request leaves.
        let sub = subscription_json(&mock.url("/push/abc"));
        let outcome = rail.send(&sub, None, &silent()).await;
        assert!(matches!(outcome, RailOutcome::Fatal(_)));
        assert!(mock.requests().is_empty(), "must not hit the network");
    }

    /// M3: an `https://` endpoint on a loopback/private target must be
    /// refused WITHOUT a request — the https guard passes, the private-target
    /// guard is what stops the internal SSRF. Covers the IP-literal shape
    /// (v4 + v6) and the unique-local v6 range.
    #[tokio::test]
    async fn webpush_private_https_endpoint_refused_without_a_request() {
        let (key_b64, _) = vapid_key();
        let rail = WebPushRail::new(&key_b64, "mailto:ops@example.com").expect("rail");
        let mock = ProviderMock::start(HashMap::new()).await;
        // Same loopback authority the fixture serves on, but https://.
        let endpoint = mock.url("/push/abc").replacen("http", "https", 1);
        for endpoint in [
            endpoint,
            "https://10.1.2.3/push/abc".to_string(),
            "https://192.168.0.1/push/abc".to_string(),
            "https://[::1]/push/abc".to_string(),
            "https://[fc00::9]/push/abc".to_string(),
        ] {
            let sub = subscription_json(&endpoint);
            let outcome = rail.send(&sub, None, &silent()).await;
            assert!(
                matches!(outcome, RailOutcome::Fatal(_)),
                "{endpoint} must be refused"
            );
        }
        assert!(mock.requests().is_empty(), "must not hit the network");
    }

    /// M3: a `localhost` hostname (resolved, not an IP literal) is refused
    /// after resolution — the guard must not trust a name just because it is
    /// not dotted-quad. Fails closed either way: even on a resolver that
    /// cannot resolve `localhost`, the send is Fatal, never attempted.
    #[tokio::test]
    async fn webpush_localhost_name_endpoint_refused_after_resolution() {
        let (key_b64, _) = vapid_key();
        let rail = WebPushRail::new(&key_b64, "mailto:ops@example.com").expect("rail");
        let sub = subscription_json("https://localhost:9/push/abc");
        let outcome = rail.send(&sub, None, &silent()).await;
        assert!(matches!(outcome, RailOutcome::Fatal(_)));
    }

    #[tokio::test]
    async fn webpush_bad_subscription_json_is_fatal() {
        let (key_b64, _) = vapid_key();
        let rail = WebPushRail::new(&key_b64, "mailto:ops@example.com").expect("rail");
        let outcome = rail.send("{not json", None, &silent()).await;
        assert!(matches!(outcome, RailOutcome::Fatal(_)));
    }

    #[tokio::test]
    async fn webpush_invalid_vapid_key_rejected_at_construction() {
        let err = WebPushRail::new("!!!not-base64!!!", "mailto:x@example.com");
        assert!(err.is_err());
    }

    /// Real-rail smoke — self-skips, mirrors `NOSTOS_E2E_PG`:
    /// `NOSTOS_E2E_WEBPUSH=1` + `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` +
    /// `NOSTOS_WEBPUSH_VAPID_SUBJECT` + `NOSTOS_E2E_WEBPUSH_SUBSCRIPTION`
    /// (the pushSubscription JSON).
    #[tokio::test]
    async fn e2e_webpush_smoke() {
        if crate::env::var("NOSTOS_E2E_WEBPUSH").is_err() {
            eprintln!("skipping (set NOSTOS_E2E_WEBPUSH=1 with NOSTOS_WEBPUSH_VAPID_* and NOSTOS_E2E_WEBPUSH_SUBSCRIPTION to run)");
            return;
        }
        let rail = WebPushRail::from_env()
            .expect("config parses")
            .expect("rail configured");
        let sub = crate::env::var("NOSTOS_E2E_WEBPUSH_SUBSCRIPTION").expect("subscription");
        let outcome = rail
            .send(
                &sub,
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
