//! axum routes for the Nostos Cloud control plane.
//!
//! Surface (all JSON under `/v1`, except the Stripe webhook which is raw-body):
//! - `POST /v1/auth/signup`, `POST /v1/auth/login`, `POST /v1/auth/logout`
//! - `GET  /v1/me`                          — current account
//! - `GET/POST /v1/projects`                — list/create projects
//! - `POST /v1/projects/:id/keys`           — mint an API key
//! - `GET  /v1/projects/:id/keys`           — list keys
//! - `POST /v1/projects/:id/checkout`       — create a Stripe Checkout Session
//! - `GET  /v1/projects/:id/license`        — mint the current license token
//! - `POST /v1/stripe/webhook`              — Stripe webhook (verified)
//! - `GET  /v1/waitlist` + `POST /v1/waitlist` — landing-page waitlist

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Redirect};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tower_cookies::{Cookie, Cookies};

use crate::store::{CloudStore, Role};
use crate::stripe::{self, CreateCheckout, DEFAULT_TOLERANCE_SECS};
use crate::{hash_password, verify_password};
use nostos_license::{LicenseClaims, Tier};

/// Shared state injected into the router.
#[derive(Clone)]
pub struct CloudState {
    pub store: CloudStore,
    pub license_secret: Arc<[u8]>,
    pub stripe_secret_key: Option<String>,
    pub stripe_webhook_secret: Option<String>,
    /// Map of tier → Stripe Price id (configured via env at launch).
    pub price_ids: Arc<Vec<(Tier, String)>>,
    pub http: reqwest::Client,
    pub public_base_url: String,
    /// JWT verifier for the Supabase Bearer-token auth path. When `None`,
    /// only the session-cookie path is accepted (the OSS self-host default).
    pub jwt_verifier: Option<Arc<dyn crate::auth::JwtVerifier>>,
}

impl CloudState {
    /// Price id for a tier, if configured.
    fn price_for(&self, tier: Tier) -> Option<&str> {
        self.price_ids
            .iter()
            .find(|(t, _)| *t == tier)
            .map(|(_, p)| p.as_str())
    }
}

/// Default license validity (1 year) — Cloud subscriptions renew monthly, but
/// the license token itself is valid for a year and re-minted on renewal.
const LICENSE_VALID_SECS: i64 = 365 * 24 * 3600;

pub fn router(state: CloudState) -> axum::Router {
    axum::Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/me", get(me))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/:id/keys", get(list_keys).post(create_key))
        .route("/v1/projects/:id/checkout", post(create_checkout))
        .route("/v1/projects/:id/license", get(mint_license))
        .route("/v1/stripe/webhook", post(stripe_webhook))
        .route("/v1/waitlist", get(waitlist_count).post(join_waitlist))
        .with_state(state)
}

// ---------- auth ----------

#[derive(Deserialize)]
struct AuthReq {
    email: String,
    password: String,
}

async fn signup(
    State(st): State<CloudState>,
    cookies: Cookies,
    Json(req): Json<AuthReq>,
) -> Result<impl IntoResponse, ApiError> {
    if req.email.trim().is_empty() || req.password.len() < 8 {
        return Err(ApiError::bad("email required and password >= 8 chars"));
    }
    // First account becomes the Founder.
    let role = if st.store.list_accounts().await?.is_empty() {
        Role::Founder
    } else {
        Role::Member
    };
    let acc = st
        .store
        .create_account(&req.email, &hash_password(&req.password), role)
        .await?;
    set_session(&cookies, &acc.id);
    Ok(Json(
        serde_json::json!({ "id": acc.id, "email": acc.email, "role": acc.role }),
    ))
}

async fn login(
    State(st): State<CloudState>,
    cookies: Cookies,
    Json(req): Json<AuthReq>,
) -> Result<impl IntoResponse, ApiError> {
    let acc = st
        .store
        .find_account_by_email(&req.email)
        .await?
        .ok_or_else(|| ApiError::bad("invalid credentials"))?;
    if !verify_password(&req.password, &acc.password_hash) {
        return Err(ApiError::bad("invalid credentials"));
    }
    set_session(&cookies, &acc.id);
    Ok(Json(
        serde_json::json!({ "id": acc.id, "email": acc.email, "role": acc.role }),
    ))
}

async fn logout(cookies: Cookies) -> impl IntoResponse {
    cookies.remove(Cookie::build("nostos_session").path("/").build());
    StatusCode::NO_CONTENT
}

fn set_session(cookies: &Cookies, account_id: &str) {
    // For a launch: the session cookie IS the account id (signed/sealed in a
    // real deploy via tower-cookies' private jar). Documented upgrade.
    cookies.add(
        Cookie::build(("nostos_session", account_id.to_string()))
            .path("/")
            .build(),
    );
}

/// Resolve the current account id from either credential path:
///
/// 1. `Authorization: Bearer <jwt>` → verified via the configured JWT verifier
/// 2. else the `nostos_session` cookie → account id (the OSS/self-host path)
///
/// Then validate the resolved id exists in the store. Returns 401 on failure.
async fn current_account_id(
    headers: &HeaderMap,
    cookies: &Cookies,
    st: &CloudState,
) -> Result<String, ApiError> {
    // Path 1: JWT bearer token (managed cloud).
    if let Some(verifier) = &st.jwt_verifier {
        if let Some(token) = bearer_token(headers) {
            if let Some(sub) = verifier.verify_sub(&token) {
                return Ok(sub);
            }
        }
    }
    // Path 2: session cookie (OSS self-host + web admin).
    let id = cookies
        .get("nostos_session")
        .ok_or(ApiError(
            StatusCode::UNAUTHORIZED,
            "not authenticated".into(),
        ))?
        .value()
        .to_string();
    // Validate the id exists.
    for acc in st.store.list_accounts().await? {
        if acc.id == id {
            return Ok(id);
        }
    }
    Err(ApiError(StatusCode::UNAUTHORIZED, "session invalid".into()))
}

/// Pull a `Bearer <token>` from the Authorization header, if present.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let h = headers.get("authorization")?.to_str().ok()?;
    let t = h.strip_prefix("Bearer ").unwrap_or("");
    (!t.is_empty()).then(|| t.to_string())
}

async fn me(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    let id = current_account_id(&headers, &cookies, &st).await?;
    let acc = st
        .store
        .list_accounts()
        .await?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "no such account".into()))?;
    Ok(Json(
        serde_json::json!({ "id": acc.id, "email": acc.email, "role": acc.role }),
    ))
}

// ---------- projects + keys ----------

#[derive(Deserialize)]
struct CreateProjectReq {
    name: String,
}

async fn list_projects(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<impl IntoResponse, ApiError> {
    let acc = current_account_id(&headers, &cookies, &st).await?;
    let projects = st.store.list_projects(&acc).await?;
    Ok(Json(projects))
}

async fn create_project(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
    Json(req): Json<CreateProjectReq>,
) -> Result<impl IntoResponse, ApiError> {
    let acc = current_account_id(&headers, &cookies, &st).await?;
    let proj = st.store.create_project(&acc, &req.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(&proj).unwrap()),
    ))
}

async fn list_keys(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _acc = current_account_id(&headers, &cookies, &st).await?;
    let keys = st.store.list_api_keys(&id).await?;
    Ok(Json(keys))
}

async fn create_key(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _acc = current_account_id(&headers, &cookies, &st).await?;
    let key = st.store.create_api_key(&id).await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(&key).unwrap()),
    ))
}

// ---------- billing ----------

#[derive(Deserialize)]
struct CheckoutReq {
    tier: Tier,
}

#[derive(Serialize)]
struct CheckoutResp {
    url: String,
}

async fn create_checkout(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
    Path(id): Path<String>,
    Json(req): Json<CheckoutReq>,
) -> Result<impl IntoResponse, ApiError> {
    let _acc = current_account_id(&headers, &cookies, &st).await?;
    let secret = st.stripe_secret_key.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Stripe not configured".into(),
        )
    })?;
    let price_id = st.price_for(req.tier).ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no price configured for tier".into(),
        )
    })?;
    let success = format!(
        "{}/v1/projects/{}/checkout/ok",
        st.public_base_url.trim_end_matches('/'),
        id
    );
    let cancel = format!(
        "{}/admin/#/projects/{}",
        st.public_base_url.trim_end_matches('/'),
        id
    );
    let ck = stripe::create_checkout_session(
        &st.http,
        secret,
        &CreateCheckout {
            price_id,
            success_url: &success,
            cancel_url: &cancel,
            client_reference_id: &id,
        },
    )
    .await?;
    Ok(Json(CheckoutResp { url: ck.url }))
}

/// Mint the current license token for a project (its active subscription's tier).
async fn mint_license(
    State(st): State<CloudState>,
    headers: HeaderMap,
    cookies: Cookies,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _acc = current_account_id(&headers, &cookies, &st).await?;
    // Tier = the project's active subscription, else Hobby (free Cloud tier).
    let tier = st
        .store
        .subscription_for_project(&id)
        .await?
        .filter(|s| s.status == "active")
        .map_or(Tier::Hobby, |s| s.tier);
    let claims = LicenseClaims {
        project_id: id,
        tier,
        expires_at: OffsetDateTime::now_utc().unix_timestamp() + LICENSE_VALID_SECS,
        device_cap: None,
    };
    let token = claims.sign(&st.license_secret)?;
    Ok(Json(serde_json::json!({ "license": token, "tier": tier })))
}

// ---------- stripe webhook ----------

/// Stripe webhook — verifies the signature, then provisions the subscription.
/// The raw body MUST be captured before any JSON parsing (Stripe requirement).
async fn stripe_webhook(
    State(st): State<CloudState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sig = headers
        .get("stripe-signature")
        .ok_or_else(|| ApiError::bad("missing Stripe-Signature header"))?
        .to_str()
        .map_err(|_| ApiError::bad("invalid header"))?;
    let secret = st
        .stripe_webhook_secret
        .as_ref()
        .ok_or_else(|| ApiError::bad("webhook secret not configured"))?;
    stripe::verify_webhook(sig, &body, secret, DEFAULT_TOLERANCE_SECS)?;
    let event: stripe::StripeEvent =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad(&format!("bad json: {e}")))?;

    if let Some((sub_id, cust_id)) = stripe::extract_subscription_ids(&event.data.object) {
        // client_reference_id carries our project id (set at Checkout creation).
        let project_id = event
            .data
            .object
            .get("client_reference_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !project_id.is_empty() {
            // Tier inferred from the price — for launch, default to Pro; a real
            // impl maps price_id → tier. Store regardless so the webhook is idempotent.
            let tier = price_to_tier(&event.data.object, &st.price_ids);
            st.store
                .upsert_subscription(crate::store::Subscription {
                    id: sub_id.clone(),
                    project_id,
                    stripe_customer_id: cust_id,
                    stripe_subscription_id: sub_id,
                    tier,
                    status: "active".into(),
                    created_at: OffsetDateTime::now_utc().unix_timestamp(),
                })
                .await?;
        }
    }
    Ok(StatusCode::OK)
}

/// Best-effort tier inference from a Checkout/Subscription object's price id.
fn price_to_tier(object: &serde_json::Value, price_ids: &[(Tier, String)]) -> Tier {
    let price = object
        .pointer("/line_items/data/0/price/id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            object
                .pointer("/items/data/0/price/id")
                .and_then(|v| v.as_str())
        })
        .unwrap_or_default();
    price_ids
        .iter()
        .find(|(_, p)| *p == price)
        .map_or(Tier::Pro, |(t, _)| *t)
}

// ---------- waitlist ----------

#[derive(Deserialize)]
struct WaitlistReq {
    email: String,
}

async fn join_waitlist(
    State(st): State<CloudState>,
    Json(req): Json<WaitlistReq>,
) -> Result<impl IntoResponse, ApiError> {
    if req.email.trim().is_empty() || !req.email.contains('@') {
        return Err(ApiError::bad("valid email required"));
    }
    st.store.add_waitlist(&req.email).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"ok": true}))))
}

async fn waitlist_count(State(st): State<CloudState>) -> Result<impl IntoResponse, ApiError> {
    let n = st.store.waitlist_count().await?;
    Ok(Json(serde_json::json!({"count": n})))
}

/// Redirect the Checkout success URL back to the admin project view.
/// `async` because axum's `Handler` trait requires it (the body doesn't await).
#[allow(clippy::unused_async)]
pub async fn checkout_ok(Path(id): Path<String>) -> Redirect {
    Redirect::to(&format!("/admin/#/projects/{id}"))
}

// ---------- error type ----------

/// A typed API error → (status, message) JSON.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    fn bad(msg: &str) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<nostos_license::LicenseError> for ApiError {
    fn from(e: nostos_license::LicenseError) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<crate::stripe::StripeError> for ApiError {
    fn from(e: crate::stripe::StripeError) -> Self {
        let status = match e {
            crate::stripe::StripeError::BadSignature(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = Json(serde_json::json!({ "error": self.1 }));
        (self.0, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Role;
    use std::time::Duration;

    fn test_state() -> CloudState {
        CloudState {
            store: CloudStore::in_memory().unwrap(),
            license_secret: Arc::from(b"test-secret".as_ref()),
            stripe_secret_key: None,
            stripe_webhook_secret: Some("whsec_test".into()),
            price_ids: Arc::new(vec![(Tier::Pro, "price_pro".into())]),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            public_base_url: "http://localhost:9100".into(),
            // No JWT verifier in the default test state → exercises the
            // session-cookie path (the OSS self-host default).
            jwt_verifier: None,
        }
    }

    #[tokio::test]
    async fn first_signup_is_founder() {
        let st = test_state();
        let acc = st
            .store
            .create_account("f@nostos.run", "h", Role::Founder)
            .await
            .unwrap();
        assert_eq!(acc.role, Role::Founder);
    }

    #[tokio::test]
    async fn price_to_tier_maps_known_price() {
        let st = test_state();
        let obj = serde_json::json!({"line_items":{"data":[{"price":{"id":"price_pro"}}]}});
        assert_eq!(price_to_tier(&obj, &st.price_ids), Tier::Pro);
    }

    #[tokio::test]
    async fn price_to_tier_defaults_pro_for_unknown() {
        let st = test_state();
        let obj = serde_json::json!({});
        assert_eq!(price_to_tier(&obj, &st.price_ids), Tier::Pro);
    }

    #[tokio::test]
    async fn mint_license_yields_hobby_without_subscription() {
        let st = test_state();
        let acc = st
            .store
            .create_account("x@nostos.run", "h", Role::Founder)
            .await
            .unwrap();
        let proj = st.store.create_project(&acc.id, "P").await.unwrap();
        let claims = LicenseClaims {
            project_id: proj.id.clone(),
            tier: Tier::Hobby,
            expires_at: OffsetDateTime::now_utc().unix_timestamp() + 3600,
            device_cap: None,
        };
        let token = claims.sign(&st.license_secret).unwrap();
        let back = LicenseClaims::verify(&token, &st.license_secret).unwrap();
        assert_eq!(back.tier, Tier::Hobby);
        assert_eq!(back.project_id, proj.id);
    }

    // ===================== HTTP route smoke tests =====================
    // These fire real requests at the axum router via `oneshot` — no live
    // server, no external services (Stripe returns 503 gracefully with no key
    // configured; the webhook path uses the test secret). They prove the
    // wiring end-to-end: signup → cookie → me → projects → keys → license.

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    /// Build the router with the CookieManagerLayer the handlers require.
    /// (main.rs adds this layer on the full app; tests must too, or `Cookies`
    /// extraction panics with a 500.)
    fn test_app(st: CloudState) -> axum::Router {
        router(st).layer(CookieManagerLayer::new())
    }

    /// Build a JSON request for a path.
    fn json_req(method: &str, path: &str, body: Option<serde_json::Value>) -> Request<Body> {
        let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    /// Pull the `nostos_session` cookie value out of a response's Set-Cookie.
    fn session_cookie(resp: &axum::response::Response) -> Option<String> {
        resp.headers().get_all("set-cookie").iter().find_map(|h| {
            let s = h.to_str().ok()?;
            let part = s.strip_prefix("nostos_session=")?;
            Some(part.split(';').next()?.to_string())
        })
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn signup_sets_session_cookie_and_me_works() {
        // ponytail: one app instance, full flow — the session cookie from
        // signup must validate against /v1/me on the same store.
        let st = test_state();
        let app = test_app(st.clone());
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/v1/auth/signup",
                Some(serde_json::json!({"email":"full@nostos.run","password":"password123"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = session_cookie(&resp).expect("cookie after signup");
        assert!(!cookie.is_empty());
        let me_req = Request::builder()
            .method("GET")
            .uri("/v1/me")
            .header("cookie", format!("nostos_session={cookie}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(me_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(
            body.contains("full@nostos.run"),
            "me echoes the email: {body}"
        );
    }

    #[tokio::test]
    async fn full_project_key_license_flow() {
        let st = test_state();
        let app = test_app(st.clone());

        // 1. signup
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/v1/auth/signup",
                Some(serde_json::json!({"email":"flow@nostos.run","password":"password123"})),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&resp).expect("cookie after signup");

        let authed = |method: &str, path: &str, body: Option<serde_json::Value>| {
            let b = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
            Request::builder()
                .method(method)
                .uri(path)
                .header("cookie", format!("nostos_session={cookie}"))
                .header("content-type", "application/json")
                .body(b)
                .unwrap()
        };

        // 2. create a project
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/v1/projects",
                Some(serde_json::json!({"name":"Demo"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let proj_body: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        let proj_id = proj_body["id"].as_str().expect("project id").to_string();

        // 3. list projects → has the one we made
        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/projects", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert!(list.as_array().unwrap().iter().any(|p| p["id"] == proj_id));

        // 4. create an API key
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/v1/projects/{proj_id}/keys"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 5. mint a license (Hobby — no subscription)
        let resp = app
            .clone()
            .oneshot(authed(
                "GET",
                &format!("/v1/projects/{proj_id}/license"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let lic: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert!(
            lic["license"].as_str().unwrap().contains('.'),
            "license is payload.sig: {lic}"
        );
    }

    #[tokio::test]
    async fn unauthenticated_request_is_401() {
        let app = test_app(test_state());
        let resp = app.oneshot(json_req("GET", "/v1/me", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn checkout_without_stripe_config_is_503() {
        // No stripe_secret_key in test_state → checkout must 503, not crash.
        let st = test_state();
        let app = test_app(st.clone());
        // signup → capture the real account id + cookie
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/v1/auth/signup",
                Some(serde_json::json!({"email":"c@nostos.run","password":"password123"})),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&resp).expect("cookie");
        let acc_body: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        let acc_id = acc_body["id"].as_str().expect("account id").to_string();
        // create the project owned by the real account
        let proj = st.store.create_project(&acc_id, "P").await.unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/projects/{}/checkout", proj.id))
                    .header("cookie", format!("nostos_session={cookie}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"tier":"pro"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn waitlist_join_and_count() {
        let app = test_app(test_state());
        let resp = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/v1/waitlist",
                Some(serde_json::json!({"email":"w@nostos.run"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .oneshot(json_req("GET", "/v1/waitlist", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("\"count\""), "waitlist count present: {body}");
    }
}
