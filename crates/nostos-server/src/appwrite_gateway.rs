//! Appwrite server transport over the same Function journal as direct mode.
//! The Function remains the authority for ACL, mutation IDs and sequencing
//! (ADR-0052). This gateway holds no API key or local copy of private rows.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context as _};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::{cors::build_cors_layer, shutdown::shutdown_signal, telemetry::redacted_request_span};

const BODY_LIMIT: usize = 1024 * 1024;

struct Gateway {
    http: reqwest::Client,
    execution_url: String,
    project_id: String,
}

impl Gateway {
    fn from_env() -> anyhow::Result<Self> {
        let endpoint = nostos_infra::env::var("NOSTOS_APPWRITE_ENDPOINT")
            .context("NOSTOS_APPWRITE_ENDPOINT is required")?;
        let project_id = nostos_infra::env::var("NOSTOS_APPWRITE_PROJECT_ID")
            .context("NOSTOS_APPWRITE_PROJECT_ID is required")?;
        let function_id = nostos_infra::env::var("NOSTOS_APPWRITE_FUNCTION_ID")
            .context("NOSTOS_APPWRITE_FUNCTION_ID is required")?;
        let url = reqwest::Url::parse(&endpoint).context("invalid Appwrite endpoint")?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            bail!("Appwrite endpoint must use HTTPS outside loopback");
        }
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("Appwrite endpoint must not contain credentials, query or fragment");
        }
        if !valid_id(&project_id) || !valid_id(&function_id) {
            bail!("invalid Appwrite project or Function ID");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(35))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            execution_url: format!(
                "{}/functions/{function_id}/executions",
                endpoint.trim_end_matches('/')
            ),
            project_id,
        })
    }

    async fn forward(
        &self,
        path: &'static str,
        headers: &HeaderMap,
        body: Value,
    ) -> impl IntoResponse {
        let Some(jwt) = headers
            .get(AUTHORIZATION)
            .and_then(|raw| raw.to_str().ok())
            .and_then(|raw| raw.strip_prefix("Bearer "))
            .filter(|jwt| {
                !jwt.is_empty()
                    && jwt.len() <= 8192
                    && !jwt.bytes().any(|b| b.is_ascii_whitespace())
            })
        else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"message":"bearer token required"})),
            );
        };
        if !body.is_object() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message":"JSON object required"})),
            );
        }
        let response = self
            .http
            .post(&self.execution_url)
            .header("X-Appwrite-Project", &self.project_id)
            .header("X-Appwrite-JWT", jwt)
            .json(&json!({"method":"POST","path":path,"body":body.to_string()}))
            .send()
            .await;
        let Ok(response) = response else {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"message":"Appwrite unavailable"})),
            );
        };
        let status = response.status();
        let value: Value = match response.json().await {
            Ok(value) => value,
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"message":"invalid Appwrite response"})),
                );
            }
        };
        if !status.is_success() {
            let message = value["message"]
                .as_str()
                .unwrap_or("Appwrite rejected request");
            return (
                status,
                Json(json!({"message":message.chars().take(300).collect::<String>()})),
            );
        }
        let (Some(code), Some(body)) = (
            value["responseStatusCode"].as_u64(),
            value["responseBody"].as_str(),
        ) else {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"message":"invalid Function envelope"})),
            );
        };
        // Only these two fields are needed by Nostos clients. Appwrite's full
        // execution record can include request headers; never echo a JWT.
        (
            StatusCode::OK,
            Json(json!({"responseStatusCode":code,"responseBody":body})),
        )
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 36
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

async fn pull(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    gateway.forward("/sync/pull", &headers, body).await
}

async fn push(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    gateway.forward("/sync/push", &headers, body).await
}

pub(crate) async fn serve(bind: &str, cors_origins: &str) -> anyhow::Result<()> {
    let gateway = Arc::new(Gateway::from_env()?);
    let app = Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","backend":"appwrite"})) }),
        )
        .route("/appwrite/sync/pull", post(pull))
        .route("/appwrite/sync/push", post(push))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(build_cors_layer(cors_origins)?)
        .layer(TraceLayer::new_for_http().make_span_with(redacted_request_span))
        .with_state(gateway);
    let addr: SocketAddr = bind.parse().context("invalid NOSTOS_BIND")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    info!(%addr, "Nostos Appwrite gateway listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Appwrite gateway error")
}
