//! JWKS-backed verification for asymmetric Supabase JWTs (RS256/ES256/EdDSA).
//!
//! Supabase projects created since 2025-10-01 sign user JWTs with an
//! asymmetric key by default (RS256, optionally ES256/EdDSA) rather than the
//! legacy HS256 shared secret; the public keys are published at
//! `<project>/auth/v1/.well-known/jwks.json` and edge-cached ~10 minutes
//! (ADR-0010 addendum). This module fetches + caches that JWKS and verifies
//! tokens against it.
//!
//! Cache policy: entries are valid for `ttl` (deploy sets ≤10 min to match
//! Supabase's edge cache). On a `kid` the cache doesn't have, it refetches
//! once; a second miss (or a fetch error) fails closed rather than retrying —
//! see [`JwksVerifier::resolve_key`]. Unknown-`kid` refetches are additionally
//! rate-limited independent of `ttl` so a client sending many distinct bogus
//! `kid`s can't turn verification into a JWKS-endpoint hammer.
//!
//! Algorithm confusion is prevented at the source: the [`Algorithm`] a key
//! verifies under is fixed from the JWK's own key type when the key is
//! cached, and `jsonwebtoken`'s `Validation` is restricted to exactly that
//! algorithm — a token can't present an RS256 header and be checked against
//! an ES256 key (or vice versa), and this module never touches HS256 key
//! material at all (that stays confined to `SupabaseJwtAuth`'s legacy path).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Header, Validation};
use nostos_domain::Principal;
use tokio::sync::RwLock;
use tracing::warn;

/// Minimum spacing between unknown-`kid`-triggered refetches, independent of
/// `ttl`. Without this, N distinct bogus `kid`s in quick succession would
/// force N JWKS fetches — a cheap way to hammer the JWKS endpoint through us.
/// ponytail: fixed, not configurable — no deploy has asked for a different
/// window, and this is a DoS guard rather than a tunable knob.
const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct CachedKey {
    decoding_key: DecodingKey,
    algorithm: Algorithm,
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, CachedKey>,
    fetched_at: Option<Instant>,
    last_fetch_attempt: Option<Instant>,
    /// Whether the most recent fetch attempt errored. A failed fetch leaves
    /// `fetched_at` untouched, so the cache reads "stale" for the whole
    /// outage — without this flag every subsequent request retries the fetch.
    last_fetch_failed: bool,
}

/// Fetches, caches, and verifies against a Supabase project's JWKS.
pub(crate) struct JwksVerifier {
    jwks_url: String,
    http: reqwest::Client,
    ttl: Duration,
    /// How long a cache may keep serving after its last SUCCESSFUL fetch.
    ///
    /// Audit finding 6: a failed fetch only warns and leaves `cache.keys`
    /// intact, so rotation is observed only via a *successful* fetch — a key
    /// the IdP revoked kept verifying for the entire outage. Unknown `kid`s
    /// failed closed; rotated ones failed OPEN. Past this window the cache is
    /// refused outright, which turns an indefinite fail-open into a bounded
    /// one: a long IdP outage now costs availability, not revocation.
    max_stale: Duration,
    cache: RwLock<Cache>,
    /// Accepted `iss` values. Empty = unchecked (the default — an allowlist
    /// that defaults to non-empty would reject every existing deployment's
    /// tokens on upgrade). When set, a token from any other issuer is
    /// rejected even if its `kid` resolves.
    issuers: Vec<String>,
}

/// Default ceiling on serving a JWKS cache that has not refreshed
/// successfully. Long enough to ride a short IdP blip without an auth outage,
/// short enough that a revoked key does not stay usable for a working day.
pub(crate) const DEFAULT_JWKS_MAX_STALE: Duration = Duration::from_mins(30);

impl JwksVerifier {
    /// Restrict accepted `iss` values (audit finding 4). Empty leaves `iss`
    /// unchecked, which is the default: a non-empty default would reject
    /// every existing deployment's tokens on upgrade.
    #[must_use]
    pub(crate) fn with_issuers(mut self, issuers: Vec<String>) -> Self {
        self.issuers = issuers;
        self
    }

    /// Override the staleness ceiling (see [`Self::max_stale`]).
    #[must_use]
    pub(crate) fn with_max_stale(mut self, max_stale: Duration) -> Self {
        self.max_stale = max_stale;
        self
    }

    pub(crate) fn new(jwks_url: String, ttl: Duration) -> Self {
        Self {
            jwks_url,
            // Bound the JWKS fetch (audit 2026-08-17 M3): the refresh path
            // holds the cache WRITE lock across the fetch, so an IdP that
            // accepts TCP but stops responding would otherwise stall EVERY
            // auth verify behind the write-preferring RwLock for good.
            // 10s/5s mirrors the push rails' shared-client template.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            ttl,
            max_stale: DEFAULT_JWKS_MAX_STALE,
            cache: RwLock::new(Cache::default()),
            issuers: Vec::new(),
        }
    }

    /// Verify a token whose header algorithm is RS256/ES256/EdDSA and lift its
    /// `sub` into a [`Principal`] — mirrored into both `account_id` and
    /// `tenant_id`, identical to the HS256 path (ADR-0010 Phase 0: tenant =
    /// account; ADR-0011 defers real tenant-claim resolution). Flat scalar
    /// claims beyond `sub`/`exp` are lifted into `Principal::extra` via
    /// `crate::auth::lift_extra_claims` — the SAME size-capped,
    /// reserved-name-filtered function the HS256 path uses (ADR-0031, D1),
    /// so this path gets the security review for free rather than
    /// duplicating it. `None` on any failure: unknown `kid`, bad/expired
    /// signature, unreachable JWKS, or a claim set that fails the D1 caps.
    pub(crate) async fn verify(&self, token: &str, header: &Header) -> Option<Principal> {
        let kid = header.kid.as_deref()?;
        let key = self.resolve_key(kid).await?;
        if key.algorithm != header.alg {
            // Belt-and-suspenders: resolve_key already indexes by an
            // algorithm fixed at cache time, and `Validation::new` below
            // pins it again — this can't actually diverge, but a token can
            // never be checked against a key of a different algorithm family.
            warn!(kid, "jwt rejected: header alg does not match jwks key alg");
            return None;
        }
        let mut validation = Validation::new(key.algorithm);
        // Matches the HS256 legacy path (verify_supabase_hs256): no aud/role
        // check today, since Principal only ever lifts `sub`. exp IS checked
        // (jsonwebtoken's default `required_spec_claims` includes "exp"),
        // which is new relative to the HS256 path — see ADR-0010 addendum.
        validation.validate_aud = false;
        // Audit finding 4. `nbf` was unchecked, so a token minted for a future
        // window was already usable. `iss` was unchecked too — any issuer whose
        // key happened to resolve passed. `validate_aud` deliberately stays
        // false: Supabase tokens carry `aud:"authenticated"` and the library
        // default would reject all of them.
        validation.validate_nbf = true;
        if !self.issuers.is_empty() {
            validation.set_issuer(&self.issuers);
            // `set_issuer` alone only compares `iss` WHEN PRESENT (jsonwebtoken
            // 10.x does not add it to `required_spec_claims`), so a token that
            // simply omitted `iss` bypassed the allowlist. Caught by
            // `jwks_wrong_iss_rejected_when_allowlist_set`. An allowlist means
            // "must be one of these", which includes "must be there".
            validation.required_spec_claims.insert("iss".to_string());
        }
        let data = decode::<SupabaseClaims>(token, &key.decoding_key, &validation).ok()?;
        let sub = data.claims.sub;
        if sub.is_empty() {
            return None;
        }
        // ADR-0031, D1: same reject-the-whole-token-on-cap-violation rule as
        // the HS256 path — `?` here never downgrades to anonymous.
        let extra = crate::auth::lift_extra_claims(&data.claims.rest)?;
        Some(Principal::with_claims(sub.clone(), sub, extra))
    }

    async fn resolve_key(&self, kid: &str) -> Option<CachedKey> {
        {
            let cache = self.cache.read().await;
            if cache.fetched_at.is_some_and(|t| t.elapsed() < self.ttl) {
                if let Some(k) = cache.keys.get(kid) {
                    return Some(k.clone());
                }
            }
        }
        // Cache miss or stale: refresh under the write lock. ponytail: this
        // serializes concurrent refreshes onto one fetch-then-broadcast
        // rather than a notify/watch — fetches are rare (TTL-bounded) and
        // cheap (one small HTTP GET), and the client is timeout-bounded
        // (10s — audit M3), so the lock contention is bounded too. Upgrade
        // path: fetch outside the write lock with single-flight.
        let mut cache = self.cache.write().await;
        let is_fresh = cache.fetched_at.is_some_and(|t| t.elapsed() < self.ttl);
        let recent_attempt = cache
            .last_fetch_attempt
            .is_some_and(|t| t.elapsed() < MIN_REFETCH_INTERVAL);
        if is_fresh {
            // A concurrent writer already refreshed while we waited for the
            // lock — check again before deciding this `kid` is truly unknown.
            if let Some(k) = cache.keys.get(kid) {
                return Some(k.clone());
            }
            if recent_attempt {
                warn!(
                    kid,
                    "jwks: unknown kid, refetch rate-limited, failing closed"
                );
                return None;
            }
        } else if cache.last_fetch_failed && recent_attempt {
            // Stale cache AND the last attempt errored: back off instead of
            // retrying per request. A failing JWKS endpoint never advances
            // `fetched_at`, so without this the cache stays stale for the whole
            // outage and EVERY request drives its own fetch — under the write
            // lock, behind a 10s client timeout. That serializes all
            // authentication into a self-inflicted outage and points an
            // amplifier at the IdP; an attacker spraying random `kid`s gets it
            // for free. Same shape as GHSA-qw3h-qqm9-jrw8 (RabbitMQ) and
            // CVE-2026-48524 (PyJWT).
            //
            // Deliberately NOT gated on a healthy endpoint: there, one stale
            // fetch succeeds and re-freshens the cache, so the unknown-`kid`
            // path above bounds the attack to one fetch per `ttl`. Rate-limiting
            // successful stale refreshes too would break TTL-driven key
            // rotation, which is itself a security property.
            warn!(
                kid,
                "jwks: refetch backing off after recent failure, failing closed"
            );
            return None;
        }
        cache.last_fetch_attempt = Some(Instant::now());
        match self.fetch().await {
            Ok(keys) => {
                cache.keys = keys;
                cache.fetched_at = Some(Instant::now());
                cache.last_fetch_failed = false;
            }
            Err(err) => {
                cache.last_fetch_failed = true;
                warn!(%err, "jwks: fetch failed, failing closed");
            }
        }
        // Bounded staleness (audit finding 6). If the last SUCCESSFUL fetch is
        // older than `max_stale`, refuse to serve from this cache at all: past
        // that point we cannot claim the key is still current, and serving it
        // is a fail-open on revocation. A cache that never fetched
        // successfully (`fetched_at == None`) has nothing to vouch for its
        // keys either, so it is refused on the same rule.
        let fresh_enough = cache
            .fetched_at
            .is_some_and(|t| t.elapsed() <= self.max_stale);
        if !fresh_enough {
            warn!(
                kid,
                max_stale_secs = self.max_stale.as_secs(),
                "jwks: cache past its staleness ceiling, failing closed"
            );
            return None;
        }
        cache.keys.get(kid).cloned()
    }

    async fn fetch(&self) -> Result<HashMap<String, CachedKey>, JwksFetchError> {
        let resp = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(JwksFetchError)?;
        let set: JwkSet = resp.json().await.map_err(JwksFetchError)?;
        let mut keys = HashMap::new();
        for jwk in set.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            let Some(algorithm) = infer_algorithm(&jwk) else {
                continue; // unsupported key type/curve — skip rather than error
            };
            let Ok(decoding_key) = DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            keys.insert(
                kid,
                CachedKey {
                    decoding_key,
                    algorithm,
                },
            );
        }
        Ok(keys)
    }
}

/// Map a JWK's key type (+ curve, for EC/OKP) to the single [`Algorithm`] it
/// may verify under. Only RS256, ES256 (P-256), and EdDSA (Ed25519) are
/// supported — anything else (other curves, symmetric `oct` keys published in
/// a JWKS, RSA variants we don't pin) is skipped rather than guessed at.
fn infer_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P256 => {
            Some(Algorithm::ES256)
        }
        AlgorithmParameters::OctetKeyPair(okp) if okp.curve == EllipticCurve::Ed25519 => {
            Some(Algorithm::EdDSA)
        }
        _ => None,
    }
}

#[derive(Debug)]
struct JwksFetchError(reqwest::Error);

impl std::fmt::Display for JwksFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "jwks http error: {}", self.0)
    }
}

impl std::error::Error for JwksFetchError {}

/// The subset of JWT claims we read — the typed fields mirror the HS256
/// path's `SupabaseClaims` (kept as a separate type per module rather than
/// shared, matching this codebase's existing preference for a small
/// duplication over a cross-module dependency — ADR-0010), but `rest` is fed
/// into the SAME `crate::auth::lift_extra_claims` the HS256 path uses, so the
/// D1 filtering/size-cap logic itself is not duplicated.
#[derive(serde::Deserialize)]
struct SupabaseClaims {
    sub: String,
    /// Every other claim in the payload (ADR-0031, D1). `exp` is validated by
    /// `jsonwebtoken` itself (required by `Validation`'s defaults) and never
    /// needs to be read back out here, unlike the HS256 path's `token_exp`.
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Test-only fixtures shared with `auth.rs`'s tests: mint RSA/EC keypairs +
/// matching JWKs, mint tokens, and serve a mutable JWKS over a local axum
/// listener with a hit counter. `pub(crate)` (not private to `tests`) so the
/// combined HS256+JWKS routing tests in `auth.rs` can reuse the same fixture
/// server instead of duplicating key-minting boilerplate.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Algorithm, AlgorithmParameters, EllipticCurve, Jwk, JwkSet};
    use axum::extract::State as AxumState;
    use axum::response::Json;
    use axum::routing::get;
    use axum::Router;
    use ed25519_dalek::pkcs8::EncodePrivateKey as EncodeEdPrivateKey;
    use ed25519_dalek::SigningKey as EdSigningKey;
    use jsonwebtoken::{encode, jwk as jwk_types, EncodingKey, Header as JwtHeader};
    use p256::ecdsa::SigningKey as EcSigningKey;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Minimal base64url encode (no padding), test-only. Mirrors `auth.rs`'s
    /// existing preference for a hand-rolled encode/decode over pulling in a
    /// `base64` crate dependency for a handful of call sites.
    pub(crate) fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3F) as usize] as char);
            }
        }
        out
    }

    /// Mint an RSA keypair + its matching JWK (kid included).
    pub(crate) fn rsa_key_and_jwk(kid: &str) -> (EncodingKey, Jwk) {
        let mut rng = rand::thread_rng();
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
        let der = priv_key.to_pkcs1_der().expect("pkcs1 der");
        let encoding_key = EncodingKey::from_rsa_der(der.as_bytes());
        let n = b64url(&priv_key.n().to_bytes_be());
        let e = b64url(&priv_key.e().to_bytes_be());
        let jwk = Jwk {
            common: jwk_types::CommonParameters {
                key_id: Some(kid.to_string()),
                ..Default::default()
            },
            algorithm: AlgorithmParameters::RSA(jwk_types::RSAKeyParameters {
                key_type: jwk_types::RSAKeyType::RSA,
                n,
                e,
            }),
        };
        (encoding_key, jwk)
    }

    /// Mint a P-256 keypair + its matching JWK (kid included).
    pub(crate) fn ec_key_and_jwk(kid: &str) -> (EncodingKey, Jwk) {
        let signing_key = EcSigningKey::random(&mut rand::thread_rng());
        let der = p256::pkcs8::EncodePrivateKey::to_pkcs8_der(&signing_key).expect("pkcs8 der");
        let encoding_key = EncodingKey::from_ec_der(der.as_bytes());
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = b64url(point.x().expect("x"));
        let y = b64url(point.y().expect("y"));
        let jwk = Jwk {
            common: jwk_types::CommonParameters {
                key_id: Some(kid.to_string()),
                ..Default::default()
            },
            algorithm: AlgorithmParameters::EllipticCurve(jwk_types::EllipticCurveKeyParameters {
                key_type: jwk_types::EllipticCurveKeyType::EC,
                curve: EllipticCurve::P256,
                x,
                y,
            }),
        };
        (encoding_key, jwk)
    }

    /// Mint an Ed25519 keypair + its matching JWK (kid included).
    pub(crate) fn ed_key_and_jwk(kid: &str) -> (EncodingKey, Jwk) {
        let signing_key = EdSigningKey::generate(&mut rand::thread_rng());
        let der = signing_key.to_pkcs8_der().expect("pkcs8 der");
        let encoding_key = EncodingKey::from_ed_der(der.as_bytes());
        let x = b64url(signing_key.verifying_key().to_bytes().as_slice());
        let jwk = Jwk {
            common: jwk_types::CommonParameters {
                key_id: Some(kid.to_string()),
                ..Default::default()
            },
            algorithm: AlgorithmParameters::OctetKeyPair(jwk_types::OctetKeyPairParameters {
                key_type: jwk_types::OctetKeyPairType::OctetKeyPair,
                curve: EllipticCurve::Ed25519,
                x,
            }),
        };
        (encoding_key, jwk)
    }

    pub(crate) fn mint_token(
        encoding_key: &EncodingKey,
        alg: Algorithm,
        kid: &str,
        sub: &str,
    ) -> String {
        let mut header = JwtHeader::new(alg);
        header.kid = Some(kid.to_string());
        let now = jsonwebtoken::get_current_timestamp();
        let claims = serde_json::json!({ "sub": sub, "exp": now + 3600, "iat": now, "aud": "authenticated", "role": "authenticated" });
        encode(&header, &claims, encoding_key).expect("mint token")
    }

    /// Like [`mint_token`] but signs an arbitrary claims object — used by the
    /// D1 extra-claims tests, which need payload shapes `mint_token` can't
    /// express (nested objects, arrays, nulls, a payload-carried
    /// `tenant_id`).
    pub(crate) fn mint_token_with_claims(
        encoding_key: &EncodingKey,
        alg: Algorithm,
        kid: &str,
        claims: &serde_json::Value,
    ) -> String {
        let mut header = JwtHeader::new(alg);
        header.kid = Some(kid.to_string());
        encode(&header, claims, encoding_key).expect("mint token")
    }

    pub(crate) fn expired_token(
        encoding_key: &EncodingKey,
        alg: Algorithm,
        kid: &str,
        sub: &str,
    ) -> String {
        let mut header = JwtHeader::new(alg);
        header.kid = Some(kid.to_string());
        let now = jsonwebtoken::get_current_timestamp();
        let claims = serde_json::json!({ "sub": sub, "exp": now.saturating_sub(3600), "iat": now.saturating_sub(7200) });
        encode(&header, &claims, encoding_key).expect("mint token")
    }

    /// State for the fixture axum server — declared at module level (not
    /// inside `start`) since clippy pedantic disallows items after
    /// statements.
    #[derive(Clone)]
    struct FixtureState {
        hits: Arc<AtomicUsize>,
        set: Arc<tokio::sync::RwLock<JwkSet>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    /// A tiny axum server that serves a mutable `JwkSet` and counts hits —
    /// lets tests assert exact fetch counts (TTL expiry, single-refetch).
    pub(crate) struct FixtureJwks {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
        set: Arc<tokio::sync::RwLock<JwkSet>>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FixtureJwks {
        pub(crate) async fn start(initial: JwkSet) -> Self {
            let hits = Arc::new(AtomicUsize::new(0));
            let set = Arc::new(tokio::sync::RwLock::new(initial));
            let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let app = Router::new()
                .route(
                    "/jwks.json",
                    get(|AxumState(s): AxumState<FixtureState>| async move {
                        use axum::response::IntoResponse;
                        s.hits.fetch_add(1, Ordering::SeqCst);
                        if s.fail.load(Ordering::SeqCst) {
                            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "jwks down")
                                .into_response();
                        }
                        Json(s.set.read().await.clone()).into_response()
                    }),
                )
                .with_state(FixtureState {
                    hits: hits.clone(),
                    set: set.clone(),
                    fail: fail.clone(),
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve");
            });
            Self {
                addr,
                hits,
                set,
                fail,
            }
        }

        /// Simulate a JWKS outage: the endpoint keeps counting hits but returns
        /// a body that fails to decode, so `fetch()` errors.
        pub(crate) fn set_failing(&self, failing: bool) {
            self.fail.store(failing, Ordering::SeqCst);
        }

        pub(crate) fn url(&self) -> String {
            format!("http://{}/jwks.json", self.addr)
        }

        pub(crate) fn hit_count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        #[allow(dead_code)] // used by auth.rs's rotation-adjacent tests only
        pub(crate) async fn replace(&self, set: JwkSet) {
            *self.set.write().await = set;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn rs256_token_verified_against_jwks() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let token = mint_token(&enc, Algorithm::RS256, "k1", "user-1");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        let principal = verifier.verify(&token, &header).await.expect("verified");
        assert_eq!(principal.account_id, "user-1");
        assert_eq!(principal.tenant_id, "user-1");
        assert_eq!(fixture.hit_count(), 1);
    }

    // ---- audit finding 4: nbf + iss allowlist on the JWKS path ----
    //
    // The fix landed in ac78ae5 with no tests; these pin the behaviour.
    // `jsonwebtoken`'s default leeway is 60s, the same as
    // `crate::auth::JWT_LEEWAY_SECS`, so the HS256 tests in `auth.rs` and
    // these draw the boundary at the same place.

    fn claims_with(extra: &serde_json::Value) -> serde_json::Value {
        let now = jsonwebtoken::get_current_timestamp();
        let mut claims = serde_json::json!({ "sub": "user-1", "exp": now + 3600, "iat": now });
        if let (Some(base), Some(add)) = (claims.as_object_mut(), extra.as_object()) {
            base.extend(add.clone());
        }
        claims
    }

    #[tokio::test]
    async fn jwks_future_nbf_rejected() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let nbf = jsonwebtoken::get_current_timestamp() + 3600;
        let token = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({ "nbf": nbf })),
        );
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(
            verifier.verify(&token, &header).await.is_none(),
            "an RS256 token not yet valid (nbf an hour ahead) must be rejected"
        );
    }

    #[tokio::test]
    async fn jwks_nbf_within_leeway_accepted() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let nbf = jsonwebtoken::get_current_timestamp() + 30;
        let token = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({ "nbf": nbf })),
        );
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(
            verifier.verify(&token, &header).await.is_some(),
            "an nbf inside the 60s clock-skew leeway must not be rejected"
        );
    }

    #[tokio::test]
    async fn jwks_wrong_iss_rejected_when_allowlist_set() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10))
            .with_issuers(vec!["https://good.example/auth/v1".to_string()]);
        let wrong = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({ "iss": "https://evil.example/auth/v1" })),
        );
        let header = jsonwebtoken::decode_header(&wrong).unwrap();
        assert!(
            verifier.verify(&wrong, &header).await.is_none(),
            "an iss outside the allowlist must be rejected even with a valid signature"
        );
        let missing = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({})),
        );
        let header = jsonwebtoken::decode_header(&missing).unwrap();
        assert!(
            verifier.verify(&missing, &header).await.is_none(),
            "with an allowlist set, a token with no iss is rejected too"
        );
        let right = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({ "iss": "https://good.example/auth/v1" })),
        );
        let header = jsonwebtoken::decode_header(&right).unwrap();
        assert!(
            verifier.verify(&right, &header).await.is_some(),
            "an allowlisted iss still verifies"
        );
    }

    #[tokio::test]
    async fn jwks_any_iss_accepted_when_allowlist_unset() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let token = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &claims_with(&serde_json::json!({ "iss": "https://anyone.example/auth/v1" })),
        );
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(
            verifier.verify(&token, &header).await.is_some(),
            "no allowlist configured = iss unchecked (upgrade-safe default)"
        );
    }

    // ---- D1: extra-claims lifting applies on the JWKS path too (ADR-0031) ----
    //
    // `JwksVerifier::verify` reuses `crate::auth::lift_extra_claims` rather
    // than reimplementing the filter, so these two tests exercise the reuse
    // end-to-end rather than re-proving every property `auth.rs`'s own D1
    // suite already covers directly.

    #[tokio::test]
    async fn jwks_verified_principal_lifts_claims_and_resists_tenant_shadow() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let now = jsonwebtoken::get_current_timestamp();
        let claims = serde_json::json!({
            "sub": "user-1",
            "exp": now + 3600,
            "tenant_id": "evil", // reserved: must not shadow the derived tenant_id
            "org_id": "acme",    // scalar: lifts
            "org": {"id": "acme"}, // object: dropped, never stringified
            "roles": ["a", "b"], // array: dropped
            "note": null,        // null: dropped, never ""
        });
        let token = mint_token_with_claims(&enc, Algorithm::RS256, "k1", &claims);
        let header = jsonwebtoken::decode_header(&token).unwrap();
        let principal = verifier.verify(&token, &header).await.expect("verified");

        assert_eq!(principal.account_id, "user-1");
        assert_eq!(
            principal.tenant_id, "user-1",
            "payload tenant_id must not shadow the derived value"
        );
        assert_eq!(principal.claim("tenant_id"), Some("user-1"));
        assert_eq!(
            principal.extra.get("org_id").map(String::as_str),
            Some("acme")
        );
        assert!(!principal.extra.contains_key("tenant_id"));
        assert!(!principal.extra.contains_key("org"));
        assert!(!principal.extra.contains_key("roles"));
        assert!(!principal.extra.contains_key("note"));
    }

    #[tokio::test]
    async fn jwks_verified_token_with_oversized_claim_set_is_rejected() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let now = jsonwebtoken::get_current_timestamp();
        let mut claims = serde_json::Map::new();
        claims.insert("sub".to_string(), serde_json::json!("user-1"));
        claims.insert("exp".to_string(), serde_json::json!(now + 3600));
        for i in 0..65 {
            claims.insert(format!("c{i}"), serde_json::json!("x"));
        }
        let token = mint_token_with_claims(
            &enc,
            Algorithm::RS256,
            "k1",
            &serde_json::Value::Object(claims),
        );
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(
            verifier.verify(&token, &header).await.is_none(),
            "65 claims exceeds MAX_EXTRA_CLAIMS — the whole token must be rejected, not downgraded"
        );
    }

    #[tokio::test]
    async fn es256_token_verified_against_jwks() {
        let (enc, jwk) = ec_key_and_jwk("ec1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let token = mint_token(&enc, Algorithm::ES256, "ec1", "user-2");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        let principal = verifier.verify(&token, &header).await.expect("verified");
        assert_eq!(principal.account_id, "user-2");
    }

    #[tokio::test]
    async fn eddsa_token_verified_against_jwks() {
        let (enc, jwk) = ed_key_and_jwk("ed1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let token = mint_token(&enc, Algorithm::EdDSA, "ed1", "user-3");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        let principal = verifier.verify(&token, &header).await.expect("verified");
        assert_eq!(principal.account_id, "user-3");
    }

    #[tokio::test]
    async fn unknown_kid_refetches_once_then_fails_closed() {
        let (_enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        // First fetch happens lazily on first resolve attempt.
        let missing = verifier.resolve_key("does-not-exist").await;
        assert!(missing.is_none());
        assert_eq!(fixture.hit_count(), 1, "one refetch on unknown kid");
        // A second distinct unknown kid within the rate-limit window must NOT
        // trigger a second fetch — it fails closed immediately.
        let missing2 = verifier.resolve_key("also-missing").await;
        assert!(missing2.is_none());
        assert_eq!(fixture.hit_count(), 1, "rate-limited: no second fetch");
    }

    /// A FAILING JWKS endpoint must not turn every request into its own fetch.
    ///
    /// The sibling test above runs with a 10-minute ttl, so it only exercised
    /// the fresh-cache branch — which is how the gap survived. A failed fetch
    /// leaves `fetched_at` untouched, so the cache reads "stale" for the whole
    /// outage; without a back-off, every inbound token drives another fetch
    /// under the write lock behind a 10s client timeout, serializing all
    /// authentication into a self-inflicted outage and pointing an amplifier at
    /// the IdP. An attacker spraying random `kid`s gets it for free.
    /// GHSA-qw3h-qqm9-jrw8 (RabbitMQ), CVE-2026-48524 (PyJWT).
    #[tokio::test]
    async fn failing_jwks_backs_off_instead_of_refetching_per_request() {
        let (_enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        // ttl well under the sleeps below, so the cache is definitively stale.
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_millis(10));
        fixture.set_failing(true);

        assert!(verifier.resolve_key("nope-1").await.is_none());
        assert_eq!(fixture.hit_count(), 1, "first attempt reaches the endpoint");

        // Stale cache + previous failure: further requests must back off, even
        // with distinct kids and even after the ttl has elapsed again.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(verifier.resolve_key("nope-2").await.is_none());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(verifier.resolve_key("nope-3").await.is_none());
        assert_eq!(
            fixture.hit_count(),
            1,
            "a failing endpoint must be retried on an interval, not once per \
             request — otherwise every auth attempt stalls behind its own fetch"
        );
    }

    /// The back-off must not swallow recovery, nor block TTL-driven rotation:
    /// once the endpoint is healthy again the next attempt past the interval
    /// refetches and verification resumes.
    #[tokio::test]
    async fn jwks_recovers_after_outage_once_backoff_elapses() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_millis(10));

        fixture.set_failing(true);
        assert!(verifier.resolve_key("k1").await.is_none(), "outage: no key");
        assert_eq!(fixture.hit_count(), 1);

        fixture.set_failing(false);
        tokio::time::sleep(MIN_REFETCH_INTERVAL + Duration::from_millis(50)).await;

        let token = mint_token(&enc, Algorithm::RS256, "k1", "user-1");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        let principal = verifier
            .verify(&token, &header)
            .await
            .expect("verification must resume once the endpoint recovers");
        assert_eq!(principal.account_id, "user-1");
    }

    #[tokio::test]
    async fn ttl_expiry_triggers_refetch() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_millis(20));
        let token = mint_token(&enc, Algorithm::RS256, "k1", "user-1");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(verifier.verify(&token, &header).await.is_some());
        assert_eq!(fixture.hit_count(), 1);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(verifier.verify(&token, &header).await.is_some());
        assert_eq!(fixture.hit_count(), 2, "ttl expired -> refetched");
    }

    #[tokio::test]
    async fn key_rotation_old_kid_rejected_new_kid_accepted() {
        let (enc_old, jwk_old) = rsa_key_and_jwk("old");
        let fixture = FixtureJwks::start(JwkSet {
            keys: vec![jwk_old],
        })
        .await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_millis(10));
        let old_token = mint_token(&enc_old, Algorithm::RS256, "old", "user-1");
        let old_header = jsonwebtoken::decode_header(&old_token).unwrap();
        assert!(verifier.verify(&old_token, &old_header).await.is_some());

        // Rotate: publish a brand new key under a new kid, old kid gone.
        let (enc_new, jwk_new) = rsa_key_and_jwk("new");
        fixture
            .replace(JwkSet {
                keys: vec![jwk_new],
            })
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await; // past ttl

        let new_token = mint_token(&enc_new, Algorithm::RS256, "new", "user-1");
        let new_header = jsonwebtoken::decode_header(&new_token).unwrap();
        assert!(
            verifier.verify(&new_token, &new_header).await.is_some(),
            "new kid verifies after rotation"
        );
        // Old token/kid no longer resolves once the cache has refreshed past
        // the rotation (old kid is gone from the served set).
        assert!(verifier.verify(&old_token, &old_header).await.is_none());
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        let token = expired_token(&enc, Algorithm::RS256, "k1", "user-1");
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert!(verifier.verify(&token, &header).await.is_none());
    }

    #[tokio::test]
    async fn es256_token_rejected_against_rsa_key_same_kid() {
        // Alg-confusion probe: an EC-signed token presenting a `kid` whose
        // cached key is RSA must never verify (header.alg RS256 vs the token's
        // real ES256 signature is caught by the header/key alg mismatch — this
        // constructs the header explicitly to attempt the confusion).
        let (rsa_enc, rsa_jwk) = rsa_key_and_jwk("shared-kid");
        let fixture = FixtureJwks::start(JwkSet {
            keys: vec![rsa_jwk],
        })
        .await;
        let verifier = JwksVerifier::new(fixture.url(), Duration::from_mins(10));
        // A well-formed RS256 token still must fail if we hand-craft a header
        // claiming ES256 while the cached key for this kid is RSA.
        let token = mint_token(&rsa_enc, Algorithm::RS256, "shared-kid", "user-1");
        let mut header = jsonwebtoken::decode_header(&token).unwrap();
        header.alg = Algorithm::ES256; // attacker-style header tamper
        assert!(verifier.verify(&token, &header).await.is_none());
    }
}

/// Audit finding 6: a JWKS cache must not serve indefinitely once its issuer
/// stops answering.
///
/// The old behaviour failed CLOSED for an unknown `kid` but OPEN for a rotated
/// one: a failed fetch only warned and left `cache.keys` intact, so a key the
/// IdP had revoked kept verifying for the whole outage.
#[cfg(test)]
mod jwks_staleness_tests {
    use super::test_support::{mint_token, rsa_key_and_jwk, FixtureJwks};
    use super::{Duration, JwkSet, JwksVerifier};

    #[tokio::test]
    async fn a_cache_past_its_staleness_ceiling_stops_serving_during_an_outage() {
        let (enc, jwk) = rsa_key_and_jwk("k1");
        let fixture = FixtureJwks::start(JwkSet { keys: vec![jwk] }).await;
        let token = mint_token(&enc, jsonwebtoken::Algorithm::RS256, "k1", "u1");
        let header = jsonwebtoken::decode_header(&token).expect("header");

        // Warm cache, generous ceiling: verifies while the IdP is healthy.
        let healthy = JwksVerifier::new(fixture.url(), Duration::from_mins(5))
            .with_max_stale(Duration::from_hours(1));
        assert!(
            healthy.verify(&token, &header).await.is_some(),
            "a live JWKS verifies normally"
        );
        // Still served from cache while the IdP is down but inside the ceiling
        // — an outage must not cause an instant auth outage.
        fixture.set_failing(true);
        assert!(
            healthy.verify(&token, &header).await.is_some(),
            "inside the staleness ceiling a cached key still serves"
        );

        // Same outage, but this cache is past its ceiling. Serving here is the
        // revoked-key fail-open: we can no longer claim the key is current.
        let stale =
            JwksVerifier::new(fixture.url(), Duration::from_mins(5)).with_max_stale(Duration::ZERO);
        assert!(
            stale.verify(&token, &header).await.is_none(),
            "past the staleness ceiling the cache must fail closed, not serve"
        );
    }
}
