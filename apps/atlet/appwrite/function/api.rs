use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use ureq::{Agent, AgentBuilder};

pub struct Api {
    endpoint: String,
    project: String,
    key: String,
    http: Agent,
}

impl Api {
    pub fn new(endpoint: String, project: String, key: String) -> Result<Self> {
        if !endpoint.starts_with("https://") || project.is_empty() || key.is_empty() {
            return Err(anyhow!("invalid Function Appwrite configuration"));
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            project,
            key,
            http: AgentBuilder::new().timeout(Duration::from_secs(15)).build(),
        })
    }

    pub fn get(&self, path: &str) -> Result<Option<Value>> {
        self.request("GET", path, None, None, &[])
    }

    pub fn get_with_jwt(&self, path: &str, jwt: &str) -> Result<Option<Value>> {
        self.request("GET", path, None, Some(jwt), &[])
    }

    pub fn list(&self, path: &str, queries: &[String]) -> Result<Value> {
        let pairs: Vec<(&str, &str)> = queries
            .iter()
            .map(|query| ("queries[]", query.as_str()))
            .chain(std::iter::once(("ttl", "0")))
            .collect();
        self.request("GET", path, None, None, &pairs)?
            .context("list route returned 404")
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.request("POST", path, Some(body), None, &[])?
            .context("post route returned 404")
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.request("PATCH", path, Some(body), None, &[])?
            .context("patch route returned 404")
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.request("PUT", path, Some(body), None, &[])?
            .context("put route returned 404")
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        self.request("DELETE", path, None, None, &[])?
            .context("delete route returned 404")?;
        Ok(())
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        jwt: Option<&str>,
        queries: &[(&str, &str)],
    ) -> Result<Option<Value>> {
        let mut request = self
            .http
            .request(
                method,
                &format!("{}/{}", self.endpoint, path.trim_start_matches('/')),
            )
            .set("X-Appwrite-Project", &self.project);
        if let Some(jwt) = jwt {
            request = request.set("X-Appwrite-JWT", jwt);
        } else {
            request = request.set("X-Appwrite-Key", &self.key);
        }
        if !queries.is_empty() {
            for (key, value) in queries {
                request = request.query(key, value);
            }
        }
        let response = if let Some(body) = body {
            request.send_json(body.clone())
        } else {
            request.call()
        };
        match response {
            Ok(response) => {
                let text = response.into_string()?;
                if text.is_empty() {
                    Ok(Some(Value::Null))
                } else {
                    Ok(Some(serde_json::from_str(&text)?))
                }
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(ureq::Error::Status(status, response)) => {
                let message = response.into_string().unwrap_or_default();
                Err(anyhow!(
                    "Appwrite HTTP {status}: {}",
                    message.chars().take(300).collect::<String>()
                ))
            }
            Err(error) => Err(error.into()),
        }
    }
}
