//! Control-plane Postgres helper for `init`/`doctor`: connect (plain or TLS),
//! verify `wal_level`, create/update the publication, and read slot
//! headroom + replication lag.
//!
//! Deliberately separate from `nostos-infra`'s `PgReplicator`
//! (`crates/nostos-infra/src/replicator/pg.rs`): that adapter owns the
//! *replication* connection and creates its own slot lazily on first
//! connect (idempotent `pg_create_logical_replication_slot` under
//! `IF NOT EXISTS`-style guards). This module never creates a slot — it only
//! creates/updates the **publication** (so the server's own
//! `IF NOT EXISTS` publication check is a no-op) and reports slot
//! count/headroom so `init`/`doctor` can warn before the server ever runs.

use anyhow::{Context, Result};
use tracing::warn;

/// A control-plane connection (plain SQL, not the replication protocol).
pub struct PgControl {
    client: tokio_postgres::Client,
}

impl PgControl {
    /// Connect, choosing TLS or plain based on the URL (see [`wants_tls`]).
    pub async fn connect(url: &str) -> Result<Self> {
        let client = if wants_tls(url) {
            connect_tls(url).await?
        } else {
            connect_plain(url).await?
        };
        Ok(Self { client })
    }

    /// The underlying control-plane connection. `nostos doctor --mode direct`
    /// runs catalog queries that have nothing to do with replication, so they
    /// live in `direct::inspect` rather than growing a method here per check.
    #[must_use]
    pub fn client(&self) -> &tokio_postgres::Client {
        &self.client
    }

    /// `SHOW wal_level` — must be `logical` for Nostos to replicate at all.
    pub async fn wal_level(&self) -> Result<String> {
        let row = self
            .client
            .query_one("SHOW wal_level", &[])
            .await
            .context("querying wal_level")?;
        Ok(row.get::<_, String>(0))
    }

    /// Postgres `max_slot_wal_keep_size` as `SHOW` reports it (`-1` =
    /// unbounded, else a size like `1GB`). The only thing that bounds WAL held
    /// by an *abandoned* slot — nostos-server's client eviction can't help once
    /// the server is gone (ADR-0043).
    pub async fn max_slot_wal_keep_size(&self) -> Result<String> {
        let row = self
            .client
            .query_one("SHOW max_slot_wal_keep_size", &[])
            .await
            .context("querying max_slot_wal_keep_size")?;
        Ok(row.get::<_, String>(0))
    }

    /// Create the publication (idempotent) or, if it exists but scopes a
    /// different table set, `ALTER PUBLICATION ... SET TABLE` to match.
    /// Never touches the replication slot — that's the server's job.
    ///
    /// # Errors
    /// Fails (with the offending SQL context) if a listed table doesn't
    /// exist — `CREATE`/`ALTER PUBLICATION ... FOR TABLE` requires it to.
    pub async fn ensure_publication(
        &self,
        name: &str,
        tables: &[String],
    ) -> Result<PublicationAction> {
        anyhow::ensure!(!tables.is_empty(), "at least one table is required");
        let table_list = tables
            .iter()
            .map(|t| quote_ident(t))
            .collect::<Vec<_>>()
            .join(", ");

        match self.publication_tables(name).await? {
            None => {
                let sql = format!(
                    "CREATE PUBLICATION {} FOR TABLE {table_list}",
                    quote_ident(name)
                );
                self.client.batch_execute(&sql).await.with_context(|| {
                    format!(
                        "creating publication {name} for tables {tables:?} — does every table exist?"
                    )
                })?;
                Ok(PublicationAction::Created)
            }
            Some(mut current) => {
                current.sort();
                let mut target: Vec<String> = tables.to_vec();
                target.sort();
                if current == target {
                    return Ok(PublicationAction::Unchanged);
                }
                let sql = format!(
                    "ALTER PUBLICATION {} SET TABLE {table_list}",
                    quote_ident(name)
                );
                self.client.batch_execute(&sql).await.with_context(|| {
                    format!(
                        "updating publication {name} to tables {tables:?} — does every table exist?"
                    )
                })?;
                Ok(PublicationAction::TablesUpdated)
            }
        }
    }

    /// Read-only: `Some(tables)` if the publication exists (its current
    /// `public`-schema table scope), `None` if it doesn't exist yet. Used by
    /// both `ensure_publication` (which may then create/alter) and `doctor`
    /// (which never mutates).
    pub async fn publication_tables(&self, name: &str) -> Result<Option<Vec<String>>> {
        let exists: bool = self
            .client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = $1)",
                &[&name],
            )
            .await
            .context("checking for an existing publication")?
            .get(0);
        if !exists {
            return Ok(None);
        }
        let rows = self
            .client
            .query(
                "SELECT tablename FROM pg_publication_tables \
                 WHERE pubname = $1 AND schemaname = 'public'",
                &[&name],
            )
            .await
            .context("reading current publication tables")?;
        Ok(Some(rows.iter().map(|r| r.get::<_, String>(0)).collect()))
    }

    /// `max_replication_slots` and the count currently in use — small
    /// Supabase computes cap this at 5, shared with Realtime and any other
    /// consumer, so headroom matters before `nostos dev` ever asks the server
    /// to create its own slot.
    pub async fn slot_headroom(&self) -> Result<SlotHeadroom> {
        let max_text: String = self
            .client
            .query_one("SHOW max_replication_slots", &[])
            .await
            .context("querying max_replication_slots")?
            .get(0);
        let max: i64 = max_text
            .trim()
            .parse()
            .with_context(|| format!("parsing max_replication_slots value {max_text:?}"))?;
        let used: i64 = self
            .client
            .query_one("SELECT count(*) FROM pg_replication_slots", &[])
            .await
            .context("counting pg_replication_slots")?
            .get(0);
        Ok(SlotHeadroom { max, used })
    }

    /// Status of one named slot: whether it exists yet (the server creates
    /// it lazily on first connect, so it's normal for this to be absent
    /// before the first `nostos dev`), its confirmed-flush LSN, and its lag
    /// behind the current WAL head in bytes.
    pub async fn slot_status(&self, slot: &str) -> Result<SlotStatus> {
        let row = self
            .client
            .query_opt(
                "SELECT confirmed_flush_lsn::text, \
                        pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn)::bigint \
                 FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .context("querying replication slot status")?;
        Ok(match row {
            None => SlotStatus {
                exists: false,
                confirmed_flush_lsn: None,
                lag_bytes: None,
            },
            Some(r) => SlotStatus {
                exists: true,
                confirmed_flush_lsn: r.get(0),
                lag_bytes: r.get(1),
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationAction {
    Created,
    TablesUpdated,
    Unchanged,
}

#[derive(Debug, Clone, Copy)]
pub struct SlotHeadroom {
    pub max: i64,
    pub used: i64,
}

impl SlotHeadroom {
    #[must_use]
    pub fn headroom(&self) -> i64 {
        self.max - self.used
    }
}

#[derive(Debug, Clone)]
pub struct SlotStatus {
    pub exists: bool,
    pub confirmed_flush_lsn: Option<String>,
    pub lag_bytes: Option<i64>,
}

async fn connect_plain(url: &str) -> Result<tokio_postgres::Client> {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .with_context(|| format!("connecting to {}", redact(url)))?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!(error = %e, "postgres control connection closed with error");
        }
    });
    Ok(client)
}

/// ponytail: TLS is unverified against a real Supabase project (W0b —
/// Supabase empirical verification — is operator-blocked pending real
/// credentials). This path compiles and follows the standard
/// `tokio-postgres-rustls` recipe (webpki CA roots, no client auth), but the
/// only e2e coverage in this crate exercises `connect_plain` against local
/// Docker Postgres. Upgrade path once W0b unblocks: add a TLS-gated e2e test
/// against a real Supabase direct connection and delete this comment.
async fn connect_tls(url: &str) -> Result<tokio_postgres::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config);
    let (client, connection) = tokio_postgres::connect(url, tls)
        .await
        .with_context(|| format!("connecting (TLS) to {}", redact(url)))?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!(error = %e, "postgres control connection (TLS) closed with error");
        }
    });
    Ok(client)
}

/// Decide plain vs. TLS for the control-plane connection. An explicit
/// `sslmode` query param always wins; absent that, default to TLS unless the
/// host is an obvious local-dev target (`localhost`/`127.0.0.1`/`::1`) — the
/// same convention `psql` effectively follows, and the one that keeps
/// `docker compose up` + `nostos init` working with zero flags while still
/// doing the right thing against a managed host like Supabase.
#[must_use]
pub fn wants_tls(url: &str) -> bool {
    if let Some(mode) = query_param(url, "sslmode") {
        return mode != "disable";
    }
    !is_local_host(url)
}

fn is_local_host(url: &str) -> bool {
    matches!(
        parse_host(url).as_deref(),
        Some("localhost" | "127.0.0.1" | "::1")
    )
}

fn parse_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("postgresql://")
        .or_else(|| url.strip_prefix("postgres://"))?;
    let (authority, _path) = rest.split_once('/').unwrap_or((rest, ""));
    let hostport = authority.split_once('@').map_or(authority, |(_, h)| h);
    let hostport = hostport.split('?').next().unwrap_or(hostport);
    let host = hostport.rsplit_once(':').map_or(hostport, |(h, _)| h);
    Some(host.to_string())
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Mask the password in a libpq-style URL for safe logging/error messages.
fn redact(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let Some(at) = url.rfind('@') else {
        return url.to_string();
    };
    let scheme = &url[..scheme_end + 3];
    let userinfo = &url[scheme_end + 3..at];
    let rest = &url[at..];
    let user = userinfo.split(':').next().unwrap_or("");
    format!("{scheme}{user}:***{rest}")
}

/// Safe DDL identifier quoting (standard Postgres double-quote escaping).
/// Table names come from the CLI's own flags/config, not untrusted client
/// input, but we quote regardless — identifiers can't be bind-parameterized
/// in DDL, so this is the only injection defense available.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_defaults_on_for_remote_hosts() {
        assert!(wants_tls("postgresql://u:p@db.supabase.co:5432/postgres"));
    }

    #[test]
    fn tls_defaults_off_for_localhost() {
        assert!(!wants_tls("postgresql://cairn:cairn@localhost:5433/cairn"));
        assert!(!wants_tls("postgresql://cairn:cairn@127.0.0.1:5433/cairn"));
    }

    #[test]
    fn explicit_sslmode_disable_wins_even_for_remote_hosts() {
        assert!(!wants_tls(
            "postgresql://u:p@db.supabase.co:5432/postgres?sslmode=disable"
        ));
    }

    #[test]
    fn explicit_sslmode_require_wins_even_for_localhost() {
        assert!(wants_tls(
            "postgresql://cairn:cairn@localhost:5433/cairn?sslmode=require"
        ));
    }

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact("postgresql://cairn:s3cret@localhost:5433/cairn"),
            "postgresql://cairn:***@localhost:5433/cairn"
        );
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("tasks"), "\"tasks\"");
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn slot_headroom_math() {
        let h = SlotHeadroom { max: 5, used: 4 };
        assert_eq!(h.headroom(), 1);
    }
}
