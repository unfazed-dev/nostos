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
    /// Connect, choosing TLS or plain based on the URL (see [`ssl_mode`]).
    pub async fn connect(url: &str) -> Result<Self> {
        let client = match ssl_mode(url) {
            SslMode::Disable => connect_plain(url).await?,
            SslMode::Encrypt => connect_tls(url, false).await?,
            SslMode::Verify => connect_tls(url, true).await?,
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

/// Restate the URL in the only `sslmode` values tokio-postgres parses.
///
/// Its connection-string parser accepts `disable`/`prefer`/`require` and
/// rejects everything else outright — a libpq URL carrying `verify-full` (or
/// `allow`, or an `sslrootcert=`) never reaches this module at all, it dies as
/// "invalid value for option `sslmode`". So the mode is read here and the
/// driver is told only what it decides: whether to attempt TLS. Who the peer is
/// allowed to be is decided by the verifier in [`connect_tls`].
///
/// Note `prefer` becomes `require`: the driver would otherwise fall back to
/// plaintext on a server that refuses TLS, and failing closed is the better
/// default for a control-plane connection carrying a superuser password.
fn driver_url(url: &str, mode: SslMode) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("sslmode=") && !p.starts_with("sslrootcert="))
        .collect();
    params.push(match mode {
        SslMode::Disable => "sslmode=disable",
        SslMode::Encrypt | SslMode::Verify => "sslmode=require",
    });
    format!("{base}?{}", params.join("&"))
}

async fn connect_plain(url: &str) -> Result<tokio_postgres::Client> {
    let url = &driver_url(url, SslMode::Disable);
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

/// The trust store for a verifying connection: `sslrootcert=<pem>` when the URL
/// names one, otherwise the webpki bundle.
///
/// Managed Postgres usually issues from a private CA — Supabase's pooler cert
/// chains to "Supabase Intermediate 2021 CA", which is in no public root store
/// — so `verify-full` against one of those is only reachable with the CA the
/// provider publishes. Without this the strict modes are unusable exactly where
/// they matter most.
fn root_store(url: &str) -> Result<rustls::RootCertStore> {
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore::empty();
    let Some(path) = query_param(url, "sslrootcert") else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        return Ok(roots);
    };
    for cert in rustls::pki_types::CertificateDer::pem_file_iter(&path)
        .with_context(|| format!("reading sslrootcert {path}"))?
    {
        roots
            .add(cert.with_context(|| format!("parsing sslrootcert {path}"))?)
            .with_context(|| format!("trusting sslrootcert {path}"))?;
    }
    Ok(roots)
}

/// A verifier that checks the signature but not who signed it — libpq's
/// `require`, which encrypts and says nothing about the peer's identity.
///
/// Not a shortcut: it is the documented meaning of the mode, and the only
/// way to reach a managed Postgres whose CA the caller has not downloaded.
/// It is reached only when the URL asks for it by name.
#[derive(Debug)]
struct EncryptOnly(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// ponytail: no e2e coverage against a real managed project — the local Docker
/// Postgres this crate tests against speaks plain. Upgrade path: a TLS-gated
/// e2e against a real Supabase connection, one run per mode.
async fn connect_tls(url: &str, verify: bool) -> Result<tokio_postgres::Client> {
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?;
    let tls_config = if verify {
        builder
            .with_root_certificates(root_store(url)?)
            .with_no_client_auth()
    } else {
        warn!(
            "sslmode is `require`/`prefer`/`allow`: the connection is encrypted but the \
             server is NOT authenticated. Use sslmode=verify-full (with sslrootcert=<ca.pem> \
             for a managed provider) to check who answers."
        );
        builder
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(EncryptOnly(provider)))
            .with_no_client_auth()
    };
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config);
    let url = &driver_url(url, SslMode::Encrypt);
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

/// How much of the server's identity the control-plane connection checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// No TLS.
    Disable,
    /// Encrypted, peer unauthenticated — libpq's `allow`/`prefer`/`require`.
    Encrypt,
    /// Encrypted and the chain verified — libpq's `verify-ca`/`verify-full`.
    Verify,
}

/// Read libpq's `sslmode` out of the URL.
///
/// `require` really does mean "encrypt, don't look" in libpq — it is
/// `verify-ca`/`verify-full` that authenticate the server — and a tool that
/// takes libpq URLs has to mean the same thing by the same word, or it rejects
/// the connection string its own docs told you to paste.
///
/// The no-`sslmode` default is deliberately NOT libpq's (`prefer`): losing
/// verification is not something to do on a caller's behalf, so it stays on
/// unless the URL asks for less. Absent any `sslmode`, a local-dev host
/// (`localhost`/`127.0.0.1`/`::1`) gets plain and everything else gets
/// verification — which keeps `docker compose up` + `nostos init` working with
/// zero flags. An unrecognised value falls back to the strict path.
///
/// ponytail: `verify-ca` is treated as `verify-full`. rustls checks the name
/// along with the chain, and prising them apart means hand-rolling a verifier
/// around `WebPkiServerVerifier`; this is stricter than asked and fails closed.
/// Upgrade path if a real deployment needs the looser one: wrap that verifier
/// and swallow `InvalidCertificate(NotValidForName)`.
#[must_use]
pub fn ssl_mode(url: &str) -> SslMode {
    match query_param(url, "sslmode").as_deref() {
        Some("disable") => SslMode::Disable,
        Some("allow" | "prefer" | "require") => SslMode::Encrypt,
        None if is_local_host(url) => SslMode::Disable,
        _ => SslMode::Verify,
    }
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
        assert_eq!(
            ssl_mode("postgresql://u:p@db.supabase.co:5432/postgres"),
            SslMode::Verify
        );
    }

    #[test]
    fn tls_defaults_off_for_localhost() {
        for url in [
            "postgresql://nostos:nostos@localhost:5433/nostos",
            "postgresql://nostos:nostos@127.0.0.1:5433/nostos",
        ] {
            assert_eq!(ssl_mode(url), SslMode::Disable, "{url}");
        }
    }

    #[test]
    fn explicit_sslmode_disable_wins_even_for_remote_hosts() {
        assert_eq!(
            ssl_mode("postgresql://u:p@db.supabase.co:5432/postgres?sslmode=disable"),
            SslMode::Disable
        );
    }

    #[test]
    fn explicit_sslmode_require_wins_even_for_localhost() {
        assert_eq!(
            ssl_mode("postgresql://nostos:nostos@localhost:5433/nostos?sslmode=require"),
            SslMode::Encrypt
        );
    }

    /// The driver parses three `sslmode` values and rejects the rest, so the
    /// libpq spellings have to be translated out — including the one that made
    /// `nostos doctor --mode direct` unusable against Supabase.
    #[test]
    fn the_driver_only_ever_sees_an_sslmode_it_parses() {
        let base = "postgresql://u:p@host:5432/postgres";
        assert_eq!(
            driver_url(
                &format!("{base}?sslmode=verify-full&sslrootcert=/tmp/ca.pem&application_name=x"),
                SslMode::Verify
            ),
            format!("{base}?application_name=x&sslmode=require"),
            "verify-* and sslrootcert are ours, not the driver's"
        );
        assert_eq!(
            driver_url(base, SslMode::Disable),
            format!("{base}?sslmode=disable")
        );
        assert_eq!(
            driver_url(&format!("{base}?sslmode=prefer"), SslMode::Encrypt),
            format!("{base}?sslmode=require"),
            "prefer fails closed rather than falling back to plaintext"
        );
    }

    /// The bug this enum exists for: libpq's `require` encrypts and does NOT
    /// authenticate the server, so a managed provider whose CA is private (a
    /// Supabase pooler) answers it fine — and only `verify-*` may refuse.
    #[test]
    fn require_encrypts_without_verifying_and_verify_star_verifies() {
        let base = "postgresql://u:p@aws-1-ap-southeast-2.pooler.supabase.com:5432/postgres";
        for (mode, want) in [
            ("allow", SslMode::Encrypt),
            ("prefer", SslMode::Encrypt),
            ("require", SslMode::Encrypt),
            ("verify-ca", SslMode::Verify),
            ("verify-full", SslMode::Verify),
            // Unrecognised falls back to the strict path, never the loose one.
            ("requrie", SslMode::Verify),
        ] {
            assert_eq!(ssl_mode(&format!("{base}?sslmode={mode}")), want, "{mode}");
        }
    }

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact("postgresql://nostos:s3cret@localhost:5433/nostos"),
            "postgresql://nostos:***@localhost:5433/nostos"
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
