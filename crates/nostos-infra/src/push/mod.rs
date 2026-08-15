//! Provider rails — FCM HTTP v1 / APNs / Web Push senders (ADR-0037 §1).
//!
//! One rail per transport vendor. Each is a plain infra struct with inherent
//! async `send` methods returning [`RailOutcome`]; there is deliberately no
//! per-rail trait — the application `PushNotifier` port is composed ON TOP of
//! these by the coalescer (plan task 2.4), keyed by the token registry's
//! `platform` column. Vendor wire formats exist nowhere else.
//!
//! ## Payload discipline (ADR-0037 §2)
//!
//! Rails take an already-built [`PushPayload`]: doorbell semantics mean the
//! silent variant carries at most `{table, lsn}` (never row data — push is a
//! wake-up trigger, not a data channel), and the visible variant arrives
//! already interpolated. Template resolution is the coalescer's job (2.4).
//!
//! ## Token hygiene
//!
//! [`RailOutcome::Unregistered`] is the prune trigger for `PgTokenStore` —
//! APNs 410, FCM `UNREGISTERED`, Web Push 404/410. The rails only report the
//! outcome; pruning is consumed by task 2.4.
//!
//! ## Config (env-only; no credentials in code)
//!
//! | rail | env |
//! |---|---|
//! | FCM | `NOSTOS_FCM_CREDENTIALS_JSON` — service-account JSON |
//! | APNs | `NOSTOS_APNS_KEY_P8` (p8 PEM or path), `NOSTOS_APNS_KEY_ID`, `NOSTOS_APNS_TEAM_ID`, `NOSTOS_APNS_BUNDLE_ID`, optional `NOSTOS_APNS_SANDBOX=1` |
//! | Web Push | `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` (base64url P-256 scalar), `NOSTOS_WEBPUSH_VAPID_SUBJECT` (`mailto:`) |

use nostos_domain::Lsn;

/// Result of one rail send. Not a `Result`: every terminal state is a value —
/// the coalescer (task 2.4) maps these to retries, `PgTokenStore::prune`, and
/// the `push_sent`/`push_failed`/`push_pruned` metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailOutcome {
    /// Accepted by the provider (2xx). Last-mile delivery stays best-effort
    /// (ADR-0037 "honest limits") — the client's LSN ack is the proof.
    Delivered,
    /// The provider says the target is gone — APNs 410, FCM `UNREGISTERED`,
    /// Web Push 404/410. Prune the token row.
    Unregistered,
    /// Provider-side transient (429/5xx/network) — retry later is worthwhile.
    TransientRetryable,
    /// Not retryable (malformed token, config/auth error, provider 4xx).
    /// Carries the diagnostic for logging/metrics.
    Fatal(String),
}

/// The message a rail sends — already built; the coalescer (2.4) constructs
/// these from the per-table template/priority config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushPayload {
    /// Doorbell (ADR-0037 §2): content-free wake. `table`/`lsn` are a resume
    /// hint for the client, at most `{table, lsn}` — row data never transits
    /// a vendor.
    Silent { table: String, lsn: Lsn },
    /// Visible notification; `title`/`body` are already interpolated
    /// (template resolution lives in task 2.4, not the rails).
    Visible { title: String, body: String },
}

impl PushPayload {
    /// Seconds this payload stays valuable. A silent ping older than a minute
    /// is worthless; a visible notification is still worth showing for an
    /// hour (ADR-0037 §4 — the rails bound staleness).
    pub(crate) fn ttl_secs(&self) -> u32 {
        match self {
            Self::Silent { .. } => SILENT_TTL_SECS,
            Self::Visible { .. } => VISIBLE_TTL_SECS,
        }
    }
}

pub(crate) const SILENT_TTL_SECS: u32 = 60;
pub(crate) const VISIBLE_TTL_SECS: u32 = 3600;

/// Rail construction/config failure. Sends never return this — they return
/// [`RailOutcome`], so a half-configured rail is impossible by construction.
#[derive(Debug, thiserror::Error)]
#[error("push rail: {0}")]
pub struct PushRailError(pub String);

/// One shared HTTP client for the rails. The 10s timeout matters: the
/// coalescer's consumer task must not stall on a hung provider connection.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// Env var that is set and non-blank, else `None`.
pub(crate) fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub mod apns;
pub mod fcm;
pub mod webpush;

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared provider mock: a local axum server that records every request
    //! and replays canned responses per path — the JWKS fixture-server idiom
    //! (`jwks.rs`), so no mock-HTTP dependency is needed. Plus the base64
    //! helpers the rail tests use to mint key material and read JWTs.
    use std::collections::{HashMap, VecDeque};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::any;
    use axum::Router;

    #[derive(Clone)]
    pub(crate) struct CannedResponse {
        pub(crate) status: u16,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) body: Vec<u8>,
    }

    impl CannedResponse {
        pub(crate) fn json(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.into().into_bytes(),
            }
        }
    }

    #[derive(Clone)]
    pub(crate) struct RecordedRequest {
        pub(crate) method: String,
        pub(crate) path: String,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) body: Vec<u8>,
    }

    impl RecordedRequest {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            let name = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.as_str())
        }

        pub(crate) fn body_str(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }

        pub(crate) fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
        }
    }

    #[derive(Clone)]
    struct RailMockState {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        routes: Arc<Mutex<HashMap<String, VecDeque<CannedResponse>>>>,
    }

    pub(crate) struct ProviderMock {
        addr: SocketAddr,
        pub(crate) requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl ProviderMock {
        /// Serve `path -> response queue` (front popped per hit; empty queue
        /// answers 200/empty, keeping multi-hit tests deterministic).
        pub(crate) async fn start(routes: HashMap<String, VecDeque<CannedResponse>>) -> Self {
            let state = RailMockState {
                requests: Arc::new(Mutex::new(Vec::new())),
                routes: Arc::new(Mutex::new(routes)),
            };
            let requests = state.requests.clone();
            let app = Router::new().fallback(any(capture)).with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve");
            });
            Self { addr, requests }
        }

        pub(crate) fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("mock").clone()
        }
    }

    async fn capture(
        State(s): State<RailMockState>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let record = RecordedRequest {
            method: method.to_string(),
            path: uri.path().to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_ascii_lowercase(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            body: body.to_vec(),
        };
        s.requests.lock().expect("mock").push(record);
        let canned = s
            .routes
            .lock()
            .expect("mock")
            .get_mut(uri.path())
            .and_then(VecDeque::pop_front);
        match canned {
            Some(r) => {
                let mut builder = axum::http::Response::builder()
                    .status(StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK));
                for (k, v) in &r.headers {
                    builder = builder.header(k.as_str(), v.as_str());
                }
                builder
                    .body(Body::from(r.body))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            None => StatusCode::OK.into_response(),
        }
    }

    // ---- base64 helpers (test-only; mirrors jwks.rs's hand-rolled encode
    // over pulling a base64 crate for a handful of call sites) ----

    /// Base64 encode; `url_safe` picks the URL alphabet, `pad` adds `=`.
    pub(crate) fn b64_encode(bytes: &[u8], url_safe: bool, pad: bool) -> String {
        const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let alphabet = if url_safe { URL } else { STD };
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(alphabet[(n >> 18 & 0x3F) as usize] as char);
            out.push(alphabet[(n >> 12 & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(alphabet[(n >> 6 & 0x3F) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(alphabet[(n & 0x3F) as usize] as char);
            }
        }
        if pad {
            match out.len() % 4 {
                2 => out.push_str("=="),
                3 => out.push('='),
                _ => {}
            }
        }
        out
    }

    /// Base64url decode (no padding accepted).
    pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u32> {
            match c {
                b'A'..=b'Z' => Some(u32::from(c - b'A')),
                b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
                b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            }
        }
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let mut acc = 0u32;
        let mut bits = 0u32;
        for &c in s.as_bytes() {
            if c == b'=' {
                break;
            }
            acc = (acc << 6) | val(c)?;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                // `acc` keeps already-emitted high bits — mask them off.
                out.push(u8::try_from(acc >> bits & 0xFF).expect("masked byte"));
            }
        }
        Some(out)
    }

    /// Read a JWT's header and payload without verifying (test fixture shape
    /// checks — signature correctness is jsonwebtoken's, exercised live).
    pub(crate) fn jwt_parts(jwt: &str) -> (serde_json::Value, serde_json::Value) {
        let segs: Vec<&str> = jwt.split('.').collect();
        assert_eq!(segs.len(), 3, "jwt has three segments");
        let parse = |s: &str| {
            serde_json::from_slice::<serde_json::Value>(&b64url_decode(s).expect("b64url"))
                .expect("jwt segment json")
        };
        (parse(segs[0]), parse(segs[1]))
    }

    /// Wrap a PKCS#8 DER key as a `BEGIN PRIVATE KEY` PEM (test key minting —
    /// avoids feature-gated `to_*_pem` on the crypto crates).
    pub(crate) fn pkcs8_pem(der: &[u8]) -> String {
        let b64 = b64_encode(der, false, true);
        let mut body = String::new();
        for chunk in b64.as_bytes().chunks(64) {
            body.push_str(std::str::from_utf8(chunk).expect("b64 is ascii"));
            body.push('\n');
        }
        format!("-----BEGIN PRIVATE KEY-----\n{body}-----END PRIVATE KEY-----\n")
    }
}
