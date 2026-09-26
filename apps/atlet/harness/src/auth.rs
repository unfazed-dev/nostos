//! Real Appwrite email/password login for cloud acceptance runners.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use reqwest::{
    header::{HeaderMap, COOKIE, RETRY_AFTER, SET_COOKIE},
    RequestBuilder, Response, StatusCode,
};
use serde_json::{json, Value};

/// Local, ignored credential file for three explicit demo accounts.
pub struct Credentials {
    values: HashMap<String, String>,
}

impl Credentials {
    /// Read a mode-0600 `.env` file; no values are logged.
    ///
    /// # Errors
    /// Returns a file or syntax error.
    pub fn read(path: &Path) -> Result<Self> {
        let source =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let values = source
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let (key, value) = line.split_once('=').context("invalid credentials line")?;
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect::<Result<_>>()?;
        Ok(Self { values })
    }

    /// Value of one required key.
    ///
    /// # Errors
    /// Returns an error if the key is missing.
    pub fn get(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .with_context(|| format!("{key} missing"))
    }
}

/// Sign in through Appwrite Auth and mint the short-lived user JWT the Function verifies.
///
/// # Errors
/// Returns a login, session, or identity mismatch error.
pub async fn sign_in(
    client: &reqwest::Client,
    endpoint: &str,
    project: &str,
    email: &str,
    password: &str,
    expected_id: &str,
) -> Result<String> {
    let response = send_with_rate_limit(
        client
            .post(format!("{endpoint}/account/sessions/email"))
            .header("X-Appwrite-Project", project)
            .json(&json!({"email":email,"password":password})),
        "login",
    )
    .await?;
    let status = response.status();
    let cookies: Vec<String> = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .map(str::to_owned)
        .collect();
    let session: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "Appwrite login HTTP {status}: {}",
            session["message"].as_str().unwrap_or("unknown")
        );
    }
    if session["userId"] != expected_id {
        bail!("signed-in account does not match expected test user");
    }
    let mut request = client
        .post(format!("{endpoint}/account/jwts"))
        .header("X-Appwrite-Project", project)
        .json(&json!({}));
    if !cookies.is_empty() {
        request = request.header(COOKIE, cookies.join("; "));
    } else if let Some(secret) = session["secret"].as_str() {
        request = request.header("X-Appwrite-Session", secret);
    } else {
        bail!("Appwrite login returned neither cookie nor session secret");
    }
    let response = send_with_rate_limit(request, "JWT").await?;
    let status = response.status();
    let body: Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "Appwrite JWT HTTP {status}: {}",
            body["message"].as_str().unwrap_or("unknown")
        );
    }
    body["jwt"]
        .as_str()
        .map(str::to_owned)
        .context("JWT missing")
}

async fn send_with_rate_limit(request: RequestBuilder, action: &str) -> Result<Response> {
    for attempt in 0..3 {
        let retry = request
            .try_clone()
            .context("Appwrite auth request is not repeatable")?;
        let response = retry.send().await?;
        if response.status() != StatusCode::TOO_MANY_REQUESTS || attempt == 2 {
            return Ok(response);
        }
        let Some(delay) = rate_limit_delay(response.headers(), attempt) else {
            return Ok(response);
        };
        eprintln!(
            "Appwrite {action} rate limited; retrying in {}s",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
    }
    unreachable!("three attempts always return")
}

fn rate_limit_delay(headers: &HeaderMap, attempt: u32) -> Option<Duration> {
    let retry_after = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let reset_after = headers
        .get("X-RateLimit-Reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|reset| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            reset.saturating_sub(now)
        });
    let seconds = retry_after
        .or(reset_after)
        .unwrap_or(2_u64.pow(attempt + 1));
    (seconds <= 120).then(|| Duration::from_secs(seconds.saturating_add(1)))
}

#[cfg(test)]
mod tests {
    use super::rate_limit_delay;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rate_limit_headers_control_bounded_retry() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(rate_limit_delay(&headers, 0).unwrap().as_secs(), 8);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
        assert!(rate_limit_delay(&headers, 0).is_none());
        headers.remove(RETRY_AFTER);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        headers.insert(
            "X-RateLimit-Reset",
            HeaderValue::from_str(&(now + 3).to_string()).unwrap(),
        );
        assert!((3..=4).contains(&rate_limit_delay(&headers, 0).unwrap().as_secs()));
    }
}
