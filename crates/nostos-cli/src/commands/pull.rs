//! `nostos pull` — app-side: fetch `GET {http_base}/schema` from the running
//! nostos-server and write `.nostos/schema.json` (the ADR-0021 SchemaDescriptor
//! wire shape). See ADR-0023 D3.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::Value;

use crate::config::{ProjectConfig, DOT_NOSTOS_DIR, SCHEMA_JSON};

/// Env var `--token` falls back to. Named here so the 401 message and the
/// clap attribute can never drift apart.
pub const TOKEN_ENV: &str = "NOSTOS_TOKEN";

/// A bearer token for `GET /schema`. Newtype so redaction is structural: any
/// `{:?}` of any struct that carries one prints `[redacted]`, and there is no
/// `Display` at all — `nostos pull` never prints the value, on purpose.
///
/// Deliberately NOT a `.nostos/config.json` field: that file is committed, so a
/// secret in it would ship with the repo (v0-2-0-security-audit follow-up).
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The token with surrounding whitespace trimmed, or `None` when it is
    /// blank. `NOSTOS_TOKEN=` in a shell or CI is the common misfire and must
    /// read as "no token", not "a token the server rejected".
    #[must_use]
    fn non_blank(&self) -> Option<&str> {
        let t = self.0.trim();
        (!t.is_empty()).then_some(t)
    }
}

impl FromStr for Token {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

/// Args for `nostos pull`.
#[derive(Debug, Args)]
pub struct PullArgs {
    /// Override the nostos-server HTTP base (derived from `.nostos/config.json`
    /// `sync_url` via `ProjectConfig::http_base` when omitted).
    #[arg(long)]
    pub url: Option<String>,

    /// Bearer token sent as `Authorization: Bearer <token>` on `GET /schema`.
    /// Only needed when the server runs with `NOSTOS_PROTECT_METADATA=1`; it
    /// must then satisfy the server's `NOSTOS_SYNC_AUTH` adapter (the
    /// `NOSTOS_SYNC_BEARER_TOKEN` secret for `bearer`, a user JWT for
    /// `supabase-jwt`). Falls back to the `NOSTOS_TOKEN` env var. Never
    /// stored in `.nostos/config.json` and never printed.
    #[arg(long, env = TOKEN_ENV, hide_env_values = true)]
    pub token: Option<Token>,
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
/// unreachable, the response is non-2xx (404 = no schema source wired; 401 =
/// metadata protection on and no/rejected token; other = HTTP error), or the
/// body is not a JSON object with a non-null `tables` array.
pub async fn run(args: PullArgs, cwd: &Path) -> Result<()> {
    let http_base = match &args.url {
        Some(u) => u.clone(),
        None => ProjectConfig::load(cwd)?.http_base(),
    };
    let schema_url = schema_endpoint_url(&http_base);
    let token = args.token.as_ref().and_then(Token::non_blank);

    let client = reqwest::Client::new();
    let response = build_schema_request(&client, &schema_url, token)
        .send()
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
        // The server answers 401 both when no token was sent and when the one
        // sent failed its sync-auth adapter (nostos-server `sync_authenticated`),
        // so tell the operator which of the two happened on our side. The token
        // value itself is never echoed.
        bail!("{}", unauthorized_message(token.is_some()));
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

/// Build the `GET /schema` request, attaching `Authorization: Bearer <token>`
/// only when a token is present. `bearer_auth` marks the header sensitive so
/// reqwest's own debug output redacts it. Returns the un-sent builder so tests
/// can inspect headers without a network.
fn build_schema_request(
    client: &reqwest::Client,
    schema_url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = client.get(schema_url);
    match token {
        Some(t) => request.bearer_auth(t),
        None => request,
    }
}

/// The 401 explanation, keyed on whether we sent a token: the server returns
/// the same status for "missing" and "rejected", and the operator needs to
/// know which one they are looking at. Never includes the token value.
#[must_use]
fn unauthorized_message(token_sent: bool) -> String {
    let cause = if token_sent {
        "and rejected the bearer token from --token/NOSTOS_TOKEN. The token must \
         satisfy the server's NOSTOS_SYNC_AUTH adapter (the NOSTOS_SYNC_BEARER_TOKEN \
         secret for `bearer`, a valid user JWT for `supabase-jwt`); the value is \
         never printed, so check it at the source."
    } else {
        "which requires a bearer token and none was sent. Pass `--token <TOKEN>` \
         or set NOSTOS_TOKEN (the value must satisfy the server's NOSTOS_SYNC_AUTH \
         adapter: the NOSTOS_SYNC_BEARER_TOKEN secret for `bearer`, a user JWT for \
         `supabase-jwt`)."
    };
    format!(
        "server returned 401 for GET /schema — it is running with \
         NOSTOS_PROTECT_METADATA=1 {cause} Alternatively, unset \
         NOSTOS_PROTECT_METADATA on the server to pull the schema, or commit \
         .nostos/schema.json from a machine that pulled it while the knob was off."
    )
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

    use reqwest::header::AUTHORIZATION;

    fn built(token: Option<&str>) -> reqwest::Request {
        let client = reqwest::Client::new();
        build_schema_request(&client, "http://127.0.0.1:8800/schema", token)
            .build()
            .expect("request builds without a network")
    }

    #[test]
    fn request_without_token_has_no_authorization_header() {
        let req = built(None);
        assert_eq!(req.method(), reqwest::Method::GET);
        assert_eq!(req.url().as_str(), "http://127.0.0.1:8800/schema");
        assert!(req.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn request_with_token_sends_bearer_header_marked_sensitive() {
        let req = built(Some("s3cret"));
        let auth = req
            .headers()
            .get(AUTHORIZATION)
            .expect("authorization header present");
        assert_eq!(auth.to_str().unwrap(), "Bearer s3cret");
        assert!(
            auth.is_sensitive(),
            "bearer header must be marked sensitive"
        );
    }

    #[test]
    fn blank_token_reads_as_absent() {
        assert_eq!(Token(String::new()).non_blank(), None);
        assert_eq!(Token("   \n".to_owned()).non_blank(), None);
        assert_eq!(Token("  abc ".to_owned()).non_blank(), Some("abc"));
    }

    #[test]
    fn token_debug_is_redacted() {
        let args = PullArgs {
            url: None,
            token: Some(Token("hunter2".to_owned())),
        };
        let dbg = format!("{args:?}");
        assert!(dbg.contains("[redacted]"), "{dbg}");
        assert!(!dbg.contains("hunter2"), "token leaked via Debug: {dbg}");
    }

    #[test]
    fn unauthorized_message_without_token_points_at_the_flag_and_env() {
        let msg = unauthorized_message(false);
        assert!(msg.contains("NOSTOS_PROTECT_METADATA=1"), "{msg}");
        assert!(msg.contains("--token"), "{msg}");
        assert!(msg.contains("NOSTOS_TOKEN"), "{msg}");
        assert!(msg.contains("none was sent"), "{msg}");
        // Existing advice survives.
        assert!(msg.contains("unset"), "{msg}");
        assert!(msg.contains(".nostos/schema.json"), "{msg}");
    }

    #[test]
    fn unauthorized_message_with_token_says_it_was_rejected() {
        let msg = unauthorized_message(true);
        assert!(msg.contains("NOSTOS_PROTECT_METADATA=1"), "{msg}");
        assert!(msg.contains("rejected"), "{msg}");
        assert!(msg.contains("NOSTOS_SYNC_AUTH"), "{msg}");
        assert!(msg.contains("never printed"), "{msg}");
        assert!(msg.contains(".nostos/schema.json"), "{msg}");
    }

    #[test]
    fn clap_env_attribute_matches_token_env_const() {
        // `TOKEN_ENV` is what the 401 text names; the clap attribute is what
        // the CLI actually reads. Pin them to the same string.
        assert_eq!(TOKEN_ENV, "NOSTOS_TOKEN");
    }

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
