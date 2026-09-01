//! `nostos pull` — app-side: fetch `GET {http_base}/schema` from the running
//! nostos-server and write `.nostos/schema.json` (the ADR-0021 SchemaDescriptor
//! wire shape). See ADR-0023 D3.

use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::Value;

use crate::config::{ProjectConfig, DOT_NOSTOS_DIR, SCHEMA_JSON};

/// Args for `nostos pull`.
#[derive(Debug, Args)]
pub struct PullArgs {
    /// Override the nostos-server HTTP base (derived from `.nostos/config.json`
    /// `sync_url` via `ProjectConfig::http_base` when omitted).
    #[arg(long)]
    pub url: Option<String>,
}

/// Fetch `GET {http_base}/schema` from the running nostos-server and write the
/// returned `SchemaDescriptor` verbatim to `.nostos/schema.json` (ADR-0023 D3).
///
/// Thin, transparent proxy: the JSON body passes through unchanged. This
/// command intentionally does NOT couple to `nostos_application`'s
/// `SchemaDescriptor` type — the CLI treats the schema as opaque JSON so the
/// client tooling never depends on the server's internal type surface.
///
/// # Errors
/// [`anyhow::Error`] if the project config is missing, the server is
/// unreachable, the response is non-2xx (404 = no schema source wired; other
/// = HTTP error), or the body is not a JSON object with a non-null `tables`
/// array.
pub async fn run(args: PullArgs, cwd: &Path) -> Result<()> {
    let http_base = match &args.url {
        Some(u) => u.clone(),
        None => ProjectConfig::load(cwd)?.http_base(),
    };
    let schema_url = schema_endpoint_url(&http_base);

    let response = reqwest::get(schema_url.as_str())
        .await
        .with_context(|| format!("connecting to {schema_url}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "server returned 404 for GET /schema — no schema source wired. \
             Run the server with NOSTOS_REPLICATOR=pg (`nostos dev` against a \
             wal_level=logical Postgres). The fake replicator serves no schema."
        );
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // `nostos pull` sends no bearer token — `ProjectConfig` has no field to
        // put one in. Adding a credential store to the CLI is a feature, not a
        // security fix, so it is deliberately NOT bundled here; name the knob
        // instead of failing with a bare 401 the operator has to guess at.
        bail!(
            "server returned 401 for GET /schema — it is running with \
             NOSTOS_PROTECT_METADATA=1, which requires a bearer token that \
             `nostos pull` cannot yet send. Unset NOSTOS_PROTECT_METADATA on the \
             server to pull the schema, or commit .nostos/schema.json from a \
             machine that pulled it while the knob was off."
        );
    }
    if !status.is_success() {
        bail!("GET /schema returned HTTP {status}");
    }

    let value: Value = response
        .json()
        .await
        .with_context(|| format!("parsing JSON body from {schema_url}"))?;

    let table_count = if let Some(Value::Array(arr)) = value.get("tables") {
        arr.len()
    } else {
        let excerpt = excerpt(&value);
        bail!(
            "unexpected schema response from {schema_url} — body is not a \
             schema object with a non-null `tables` array. Body excerpt: {excerpt}"
        );
    };

    let pretty = serde_json::to_string_pretty(&value).context("pretty-printing schema JSON")?;
    let nostos_dir = cwd.join(DOT_NOSTOS_DIR);
    std::fs::create_dir_all(&nostos_dir)
        .with_context(|| format!("creating {}", nostos_dir.display()))?;
    let out_path = nostos_dir.join(SCHEMA_JSON);
    std::fs::write(&out_path, format!("{pretty}\n"))
        .with_context(|| format!("writing {}", out_path.display()))?;

    let publication = value
        .get("publication")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    println!("✓ wrote .nostos/schema.json ({table_count} tables, publication \"{publication}\")");
    if table_count == 0 {
        // nostos reads your schema via logical replication — it does not create
        // the tables upstream. An empty publication means the schema step was
        // skipped. Point at the runnable artifact instead of guessing.
        println!(
            "⚠ 0 tables published — nostos reads your schema, it does not create it. \
             Create the tables upstream (paste supabase/schema.sql into the Supabase \
             SQL editor, or apply your own migration to the `cairn_pub` publication), \
             then re-run `nostos pull`."
        );
    }

    Ok(())
}

/// Build the `GET /schema` URL from an HTTP base, collapsing any trailing
/// slashes so we never produce a double-slash. Pure and allocation-only, so
/// it can be unit-tested in isolation without touching the network.
#[must_use]
fn schema_endpoint_url(http_base: &str) -> String {
    let trimmed = http_base.trim_end_matches('/');
    format!("{trimmed}/schema")
}

/// Short single-line excerpt of a JSON value for error messages, truncated at
/// a char boundary so multi-byte UTF-8 is never split.
#[must_use]
fn excerpt(value: &Value) -> String {
    const MAX: usize = 120;
    let raw = value.to_string();
    if raw.chars().count() <= MAX {
        return raw;
    }
    let mut head: String = raw.chars().take(MAX).collect();
    head.push('…');
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_endpoint_url_appends_schema() {
        assert_eq!(
            schema_endpoint_url("http://127.0.0.1:8800"),
            "http://127.0.0.1:8800/schema"
        );
        assert_eq!(
            schema_endpoint_url("https://nostos.example.com"),
            "https://nostos.example.com/schema"
        );
    }

    #[test]
    fn schema_endpoint_url_strips_trailing_slashes() {
        assert_eq!(
            schema_endpoint_url("https://nostos.example.com/"),
            "https://nostos.example.com/schema"
        );
        assert_eq!(
            schema_endpoint_url("http://127.0.0.1:8800///"),
            "http://127.0.0.1:8800/schema"
        );
    }
}
