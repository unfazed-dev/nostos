//! Real Appwrite email/password login for cloud acceptance runners.

use std::{collections::HashMap, fs, path::Path};

use anyhow::{bail, Context, Result};
use reqwest::header::{COOKIE, SET_COOKIE};
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
    let response = client
        .post(format!("{endpoint}/account/sessions/email"))
        .header("X-Appwrite-Project", project)
        .json(&json!({"email":email,"password":password}))
        .send()
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
    let response = request.send().await?;
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
