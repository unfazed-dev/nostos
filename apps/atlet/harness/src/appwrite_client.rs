//! Small server-side TablesDB REST client for repeatable Atlet cloud checks.

use std::time::Duration;

use anyhow::{bail, Result};
use reqwest::{Method, StatusCode};
use serde_json::Value;

/// A non-success response from Appwrite or an HTTP transport failure.
#[derive(Debug, thiserror::Error)]
pub enum AppwriteError {
    /// Appwrite returned a non-2xx status.
    #[error("Appwrite HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: StatusCode,
        /// Bounded response text, never request headers or the API key.
        body: String,
    },
    /// The request failed before a response arrived.
    #[error("Appwrite transport: {0}")]
    Transport(#[from] reqwest::Error),
}

impl AppwriteError {
    /// True when the response had this HTTP status.
    #[must_use]
    pub fn is_status(&self, status: StatusCode) -> bool {
        matches!(self, Self::Status { status: actual, .. } if *actual == status)
    }
}

/// Authenticated server API client; the key remains in memory and is never logged.
pub struct AppwriteClient {
    base_url: String,
    project_id: String,
    key: String,
    http: reqwest::Client,
}

impl AppwriteClient {
    /// Create a client for an Appwrite `/v1` endpoint and one project.
    ///
    /// # Errors
    /// Returns an error for malformed URLs or HTTP client configuration.
    pub fn new(endpoint: &str, project_id: &str, key: &str) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint)?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("Appwrite endpoint must be HTTP(S)");
        }
        if project_id.is_empty() || key.is_empty() {
            bail!("Appwrite project ID and server API key are required");
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base_url: endpoint.trim_end_matches('/').to_owned(),
            project_id: project_id.to_owned(),
            key: key.to_owned(),
            http,
        })
    }

    /// GET a JSON resource. Missing resources return `None`.
    ///
    /// # Errors
    /// Returns a transport, HTTP, or JSON parse error.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        match self.send(Method::GET, path, None).await {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.is_status(StatusCode::NOT_FOUND) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// POST a JSON body and decode the JSON response.
    ///
    /// # Errors
    /// Returns a transport, HTTP, or JSON parse error.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(Method::POST, path, Some(body))
            .await
            .map_err(Into::into)
    }

    /// PATCH a JSON body and decode the JSON response.
    ///
    /// # Errors
    /// Returns a transport, HTTP, or JSON parse error.
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(Method::PATCH, path, Some(body))
            .await
            .map_err(Into::into)
    }

    /// DELETE a resource, optionally inside a TablesDB transaction.
    ///
    /// # Errors
    /// Returns a transport, HTTP, or JSON parse error.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send(Method::DELETE, path, None)
            .await
            .map_err(Into::into)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<Value, AppwriteError> {
        let mut request = self
            .http
            .request(
                method,
                format!("{}/{}", self.base_url, path.trim_start_matches('/')),
            )
            .header("X-Appwrite-Project", &self.project_id)
            .header("X-Appwrite-Key", &self.key);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).chars().take(1024).collect();
            return Err(AppwriteError::Status { status, body });
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|error| AppwriteError::Status {
            status,
            body: format!("invalid JSON response: {error}"),
        })
    }
}
