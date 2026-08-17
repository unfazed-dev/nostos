//! Daemon-owned registry store (ADR-0038 §4, plan task 1.2; schema pin
//! 0.3): push tokens + the append-only receipt log.
//!
//! SQLite is the single-binary default — the nostos-cloud store.rs pattern:
//! rusqlite bundled, one connection behind an Arc<tokio::sync::Mutex>, and
//! a migrate() that is idempotent CREATE IF NOT EXISTS. The [Store] trait
//! exists so the storage engine is a seam, not a fait accompli.
//!
//! The v1.1 Postgres registry ([`PgStore`], behind the `pg` feature —
//! ADR-0038 §4 addendum) is the pool-of-one PgTokenStore pattern: same
//! trait, same semantics, selected at runtime by NOSTOS_PUSHD_DATABASE_URL
//! while AppState keeps holding Arc<dyn Store>.
//!
//! Two columns sit beyond pin 0.3's abbreviated list, both demanded by the
//! ratified API contract: receipts.tenant_id (GET /v1/receipts is
//! tenant-scoped — without the column the oracle-safe isolation test is
//! unimplementable) and receipts.metadata (the Receipt schema echoes the
//! send's metadata; it is the push-LSN correlation channel of plan 0.4).
//!
//! Token ownership is exclusive per tenant (2026-08-17 security audit,
//! plan task 4.1, finding 3): registering a token held by ANOTHER tenant
//! returns [UpsertOutcome::Conflict] (the route answers 409) instead of
//! silently reassigning ownership. The migration path is DELETE-then-POST
//! — the old owner deletes, the new tenant registers.

use std::sync::Arc;

use async_trait::async_trait;
use nostos_infra::push::RailOutcome;
use rusqlite::{Connection, OptionalExtension};
use time::format_description::FormatItem;
use time::OffsetDateTime;
use tokio::sync::Mutex;

/// The platform a token addresses — the three rails of the OpenAPI contract
/// (apns-liveactivity is deliberately absent: it is an embedded-router
/// concept, not a daemon v1 platform).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Apns,
    Fcm,
    Webpush,
}

impl Platform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
            Self::Webpush => "webpush",
        }
    }

    /// Parse the registry/DB string form. Only this crate writes the column,
    /// so an unparseable value is corruption — callers treat None as fatal.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "apns" => Some(Self::Apns),
            "fcm" => Some(Self::Fcm),
            "webpush" => Some(Self::Webpush),
            _ => None,
        }
    }
}

/// One receipt outcome — the Receipt schema enum, derived from the rail
/// [RailOutcome] at flush time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Delivered,
    Unregistered,
    Transient,
    Fatal,
}

impl Outcome {
    /// Map a rail outcome to its receipt form plus the winner receipt's
    /// detail (only Fatal carries one — the rail's diagnostic).
    #[must_use]
    pub fn from_rail(rail: &RailOutcome) -> (Self, Option<String>) {
        match rail {
            RailOutcome::Delivered => (Self::Delivered, None),
            RailOutcome::Unregistered => (Self::Unregistered, None),
            RailOutcome::TransientRetryable => (Self::Transient, None),
            RailOutcome::Fatal(e) => (Self::Fatal, Some(e.clone())),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Unregistered => "unregistered",
            Self::Transient => "transient",
            Self::Fatal => "fatal",
        }
    }
}

/// What an owner-scoped delete found (plan task 1.4). Since the audit
/// closeout (plan task 4.1, finding 6) the route answers 204 for BOTH
/// not-yours cases — Foreign vs Missing stays distinguishable here for
/// callers that need it, but no longer leaks over HTTP (the split was a
/// token-existence oracle for other tenants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The caller's row — gone now.
    Deleted,
    /// The token exists but belongs to another tenant.
    Foreign,
    /// No such token anywhere — idempotent 204.
    Missing,
}

/// What a token upsert did (plan task 4.1, finding 3): ownership never
/// silently crosses tenants — a conflicting registration is the caller's
/// 409, not a reassignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The caller's row was inserted or refreshed.
    Registered,
    /// The token is already held by ANOTHER tenant — nothing was written.
    Conflict,
}

/// The looked-up registry row for a (tenant, token) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub platform: Platform,
    pub account_tag: Option<String>,
}

/// A receipt to append. seq is assigned by the store.
#[derive(Debug, Clone)]
pub struct NewReceipt {
    pub tenant_id: String,
    pub push_id: String,
    pub token: String,
    pub outcome: Outcome,
    pub detail: Option<String>,
    /// Echoed send metadata (the push-LSN correlation channel, pin 0.4) —
    /// serialized JSON.
    pub metadata: Option<serde_json::Value>,
    pub provider_ts: String,
}

/// A stored receipt, as read back through GET /v1/receipts.
#[derive(Debug, Clone)]
pub struct StoredReceipt {
    pub seq: i64,
    pub push_id: String,
    pub token: String,
    pub outcome: Outcome,
    pub detail: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub provider_ts: String,
}

/// Storage seam for the daemon registry (plan task 1.2): token CRUD with
/// tenant scoping, plus the append-only, monotonic-seq receipt log with a
/// retention sweep. Async because the SQLite impl serializes through a
/// tokio Mutex around the connection (the nostos-cloud pattern).
#[async_trait]
pub trait Store: Send + Sync {
    /// Upsert the caller's token row — for the caller's OWN rows only.
    /// A token held by another tenant is [UpsertOutcome::Conflict] (409 at
    /// the route): device tokens are provider-issued and globally unique,
    /// so a cross-tenant re-register is either an operator moving a device
    /// between tenants (the old owner DELETEs first — the documented
    /// migration path) or a tenant trying to capture a token it should not
    /// own (audit finding 3). Re-registering one's own row refreshes
    /// platform/account_tag as before.
    async fn upsert_token(
        &self,
        tenant_id: &str,
        token: &str,
        platform: Platform,
        account_tag: Option<&str>,
    ) -> anyhow::Result<UpsertOutcome>;

    /// Owner-scoped delete distinguishing the oracle-safe 404 cases.
    async fn delete_token_owner_scoped(
        &self,
        tenant_id: &str,
        token: &str,
    ) -> anyhow::Result<DeleteOutcome>;

    /// (tenant, token) -> (platform, account_tag) for the send path's
    /// rail-resolution. None = 404.
    async fn lookup_token(
        &self,
        tenant_id: &str,
        token: &str,
    ) -> anyhow::Result<Option<TokenRecord>>;

    /// Append one receipt; returns the assigned monotonic seq.
    async fn append_receipt(&self, receipt: &NewReceipt) -> anyhow::Result<i64>;

    /// Receipts with seq > since for this tenant, ascending, bounded.
    async fn list_receipts(
        &self,
        tenant_id: &str,
        since: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<StoredReceipt>>;

    /// Delete receipts older than the retention window; returns rows swept.
    async fn sweep_receipts(&self, retention_secs: u64) -> anyhow::Result<u64>;
}

/// Fixed-width RFC3339 (always 9 subsecond digits, always Z) so TEXT
/// columns and sweep cutoffs compare correctly as plain strings: the
/// well-known Rfc3339 format drops the fraction entirely on whole seconds,
/// and "…00Z" sorts AFTER "00.5Z" lexicographically — wrong order.
const TS_FORMAT: &[FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
);

/// Now, UTC, fixed-width RFC3339 (the nostos-cloud timestamp convention —
/// the time crate, mirrored here).
#[must_use]
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(TS_FORMAT)
        .expect("formatting now_utc with a fixed-shape format cannot fail")
}

/// SQLite-backed [Store] (plan pin 0.3). One connection behind a tokio
/// Mutex — the daemon is single-process and low-write; the moment a second
/// connection earns its keep is the moment this becomes the PgStore.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (and auto-migrate) the database at path; creates the file if
    /// absent.
    ///
    /// # Errors
    /// Bubbles sqlite errors (unreadable path, corrupt file).
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory store — tests and throwaway daemons.
    ///
    /// # Errors
    /// Bubbles sqlite errors.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS push_tokens (
                token TEXT PRIMARY KEY,
                platform TEXT NOT NULL CHECK(platform IN ('apns','fcm','webpush')),
                tenant_id TEXT NOT NULL,
                account_tag TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS idx_push_tokens_tenant_account
                ON push_tokens(tenant_id, account_tag);
            CREATE TABLE IF NOT EXISTS receipts (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                tenant_id TEXT NOT NULL,
                push_id TEXT NOT NULL,
                token TEXT NOT NULL,
                outcome TEXT NOT NULL,
                detail TEXT,
                metadata TEXT,
                provider_ts TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS idx_receipts_tenant_seq
                ON receipts(tenant_id, seq);",
        )?;
        Ok(())
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn upsert_token(
        &self,
        tenant_id: &str,
        token: &str,
        platform: Platform,
        account_tag: Option<&str>,
    ) -> anyhow::Result<UpsertOutcome> {
        let now = now_rfc3339();
        let c = self.conn.lock().await;
        // Ownership check first (audit finding 3): the old ON CONFLICT DO
        // UPDATE silently reassigned tenant_id — a cross-tenant capture the
        // route must refuse. Check-then-write is atomic here because the
        // single connection sits behind the held mutex.
        let owner: Option<String> = c
            .query_row(
                "SELECT tenant_id FROM push_tokens WHERE token=?1",
                rusqlite::params![token],
                |r| r.get(0),
            )
            .optional()?;
        if owner.is_some_and(|owner| owner != tenant_id) {
            return Ok(UpsertOutcome::Conflict);
        }
        c.execute(
            "INSERT INTO push_tokens (token, platform, tenant_id, account_tag, created_at, \
             updated_at) VALUES (?1,?2,?3,?4,?5,?5)
             ON CONFLICT(token) DO UPDATE SET
                platform=?2, account_tag=?4, updated_at=?5",
            rusqlite::params![token, platform.as_str(), tenant_id, account_tag, now],
        )?;
        Ok(UpsertOutcome::Registered)
    }

    async fn delete_token_owner_scoped(
        &self,
        tenant_id: &str,
        token: &str,
    ) -> anyhow::Result<DeleteOutcome> {
        let c = self.conn.lock().await;
        let n = c.execute(
            "DELETE FROM push_tokens WHERE token=?1 AND tenant_id=?2",
            rusqlite::params![token, tenant_id],
        )?;
        if n == 1 {
            return Ok(DeleteOutcome::Deleted);
        }
        // Not the caller's row — Foreign vs Missing is kept for callers
        // that need it; the ROUTE answers 204 either way (audit finding 6:
        // the split was a token-existence oracle).
        let owner: Option<String> = c
            .query_row(
                "SELECT tenant_id FROM push_tokens WHERE token=?1",
                rusqlite::params![token],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match owner {
            Some(_) => DeleteOutcome::Foreign,
            None => DeleteOutcome::Missing,
        })
    }

    async fn lookup_token(
        &self,
        tenant_id: &str,
        token: &str,
    ) -> anyhow::Result<Option<TokenRecord>> {
        let c = self.conn.lock().await;
        let row = c
            .query_row(
                "SELECT platform, account_tag FROM push_tokens
                 WHERE token=?1 AND tenant_id=?2",
                rusqlite::params![token, tenant_id],
                |r| {
                    let platform: String = r.get(0)?;
                    let account_tag: Option<String> = r.get(1)?;
                    Ok((platform, account_tag))
                },
            )
            .optional()?;
        Ok(row.map(|(platform, account_tag)| TokenRecord {
            platform: Platform::parse(&platform)
                .unwrap_or_else(|| panic!("corrupt platform column: {platform}")),
            account_tag,
        }))
    }

    async fn append_receipt(&self, receipt: &NewReceipt) -> anyhow::Result<i64> {
        let metadata = receipt.metadata.as_ref().map(serde_json::Value::to_string);
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO receipts (tenant_id, push_id, token, outcome, detail, metadata, \
             provider_ts) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                receipt.tenant_id,
                receipt.push_id,
                receipt.token,
                receipt.outcome.as_str(),
                receipt.detail,
                metadata,
                receipt.provider_ts
            ],
        )?;
        // Safe under the held lock: no other INSERT can interleave before
        // the rowid is read.
        Ok(c.last_insert_rowid())
    }

    async fn list_receipts(
        &self,
        tenant_id: &str,
        since: i64,
        limit: u32,
    ) -> anyhow::Result<Vec<StoredReceipt>> {
        let c = self.conn.lock().await;
        let mut stmt = c.prepare(
            "SELECT seq, push_id, token, outcome, detail, metadata, provider_ts
             FROM receipts WHERE tenant_id=?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![tenant_id, since, i64::from(limit)], |r| {
            let outcome: String = r.get(3)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                outcome,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, push_id, token, outcome, detail, metadata, provider_ts) = row?;
            out.push(StoredReceipt {
                seq,
                push_id,
                token,
                // Unparseable outcome = corruption; fatal is the honest floor.
                outcome: crate::store::Outcome::parse(&outcome).unwrap_or(Outcome::Fatal),
                detail,
                metadata: metadata.and_then(|m| serde_json::from_str(&m).ok()),
                provider_ts,
            });
        }
        Ok(out)
    }

    async fn sweep_receipts(&self, retention_secs: u64) -> anyhow::Result<u64> {
        let cutoff = (OffsetDateTime::now_utc()
            - time::Duration::seconds(i64::try_from(retention_secs).unwrap_or(i64::MAX)))
        .format(TS_FORMAT)
        .expect("fixed-shape format");
        let c = self.conn.lock().await;
        let n = c.execute(
            "DELETE FROM receipts WHERE provider_ts < ?1",
            rusqlite::params![cutoff],
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

// ===========================================================================
// PgStore — the v1.1 Postgres registry (feature "pg", ADR-0038 §4).
// ===========================================================================

/// The v1.1 Postgres registry (ADR-0038 §4 addendum): same trait, same
/// semantics as [`SqliteStore`], selected at runtime by
/// `NOSTOS_PUSHD_DATABASE_URL`. Only present under the `pg` feature.
#[cfg(feature = "pg")]
pub use self::pg::PgStore;

#[cfg(feature = "pg")]
mod pg {
    use super::{
        now_rfc3339, DeleteOutcome, NewReceipt, Outcome, Platform, Store, StoredReceipt,
        TokenRecord, UpsertOutcome, TS_FORMAT,
    };
    use anyhow::Context as _;
    use async_trait::async_trait;
    use std::sync::Arc;
    use time::OffsetDateTime;
    use tokio::sync::Mutex;
    use tokio_postgres::NoTls;

    /// Boot DDL mirroring the SQLite schema (pin 0.3) with PG types.
    /// Timestamps stay TEXT in the same fixed-width RFC3339 form
    /// ([`TS_FORMAT`] / [`now_rfc3339`]) so sweep cutoffs compare
    /// identically as plain strings, and `metadata` stays serialized-JSON
    /// TEXT for the same read-back parse — zero semantic drift from the
    /// SQLite twin. `seq` is a PG identity column (the SQLite
    /// AUTOINCREMENT rowid role): monotonic, assigned server-side.
    const DDL: &str = "CREATE TABLE IF NOT EXISTS push_tokens ( \
            token TEXT PRIMARY KEY, \
            platform TEXT NOT NULL CHECK(platform IN ('apns','fcm','webpush')), \
            tenant_id TEXT NOT NULL, \
            account_tag TEXT, \
            created_at TEXT NOT NULL, \
            updated_at TEXT NOT NULL); \
        CREATE INDEX IF NOT EXISTS idx_push_tokens_tenant_account \
            ON push_tokens(tenant_id, account_tag); \
        CREATE TABLE IF NOT EXISTS receipts ( \
            seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
            tenant_id TEXT NOT NULL, \
            push_id TEXT NOT NULL, \
            token TEXT NOT NULL, \
            outcome TEXT NOT NULL, \
            detail TEXT, \
            metadata TEXT, \
            provider_ts TEXT NOT NULL); \
        CREATE INDEX IF NOT EXISTS idx_receipts_tenant_seq \
            ON receipts(tenant_id, seq);";

    /// Advisory-lock key for boot DDL ("nostos" ASCII — arbitrary but
    /// fixed). Concurrent `CREATE TABLE IF NOT EXISTS` for one name races
    /// on the `pg_type` unique index (two pushd replicas booting at once,
    /// or parallel e2e processes); the xact-scoped lock makes the DDL wait
    /// its turn instead of failing.
    const DDL_LOCK_KEY: i64 = 0x0063_6169_726E;

    /// Bounded registry handshake (review 2026-08-17 #2) — the pool-of-one
    /// serializes every caller behind this wait.
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// Postgres-backed [`Store`] (ADR-0038 §4, v1.1) — the pool-of-one
    /// `PgTokenStore` construction pattern: one lazily-opened
    /// `tokio_postgres::Client` behind a tokio `Mutex`, transparently
    /// reopened after a connection death (any statement error drops the
    /// client; the next call reconnects).
    ///
    /// ponytail: single connection, and the guard is held across each
    /// statement so store access serializes exactly like the SQLite twin
    /// (which holds its mutex across its queries). Pool when a real load
    /// shows contention — the daemon is low-write by design.
    #[derive(Clone)]
    pub struct PgStore {
        pg_url: String,
        /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection can
        /// be replaced: take the lock, execute, and on a fatal error drop
        /// the inner `Client` (the next call reconnects).
        client: Arc<Mutex<Option<tokio_postgres::Client>>>,
    }

    impl PgStore {
        /// Open (connect + run the idempotent DDL) the registry at a
        /// libpq-style URL. Boot-time only. The URL is never logged — it
        /// can carry credentials.
        ///
        /// # Errors
        /// Bubbles connect/SQL errors (unreachable host, bad credentials).
        pub async fn open(pg_url: &str) -> anyhow::Result<Self> {
            let store = Self {
                pg_url: pg_url.to_string(),
                client: Arc::new(Mutex::new(None)),
            };
            store.migrate().await?;
            Ok(store)
        }

        /// Idempotent CREATE IF NOT EXISTS boot migration (the
        /// nostos-cloud `migrate()` role), serialized across concurrent
        /// boots by [`DDL_LOCK_KEY`].
        async fn migrate(&self) -> anyhow::Result<()> {
            self.with_client(|mut c| async move {
                let tx = c
                    .transaction()
                    .await
                    .context("opening the DDL transaction")?;
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&DDL_LOCK_KEY])
                    .await
                    .context("taking the boot-DDL advisory lock")?;
                tx.batch_execute(DDL)
                    .await
                    .context("running the pushd registry DDL")?;
                tx.commit().await.context("committing the DDL")?;
                Ok((c, ()))
            })
            .await
        }

        /// Run `f` with the pool-of-one client (lazily connected). The
        /// guard is held across the statement — the SQLite twin holds its
        /// mutex across its queries, so serialization semantics are
        /// identical and multi-statement methods (the owner-scoped
        /// delete's follow-up lookup) stay race-free. On error the client
        /// is dropped inside `f`'s future: the slot stays empty and the
        /// next call reconnects (the `PgTokenStore` fatal-error story).
        /// Deliberately NO auto-retry: a timed-out `append_receipt` may
        /// have committed, and replaying it would double-append a receipt.
        async fn with_client<F, Fut, T>(&self, f: F) -> anyhow::Result<T>
        where
            F: FnOnce(tokio_postgres::Client) -> Fut,
            Fut: std::future::Future<Output = anyhow::Result<(tokio_postgres::Client, T)>>,
        {
            let mut guard = self.client.lock().await;
            let client = match guard.take() {
                Some(c) => c,
                None => connect(&self.pg_url).await?,
            };
            let (client, out) = f(client).await?;
            *guard = Some(client);
            Ok(out)
        }
    }

    /// Connect one client and drive its socket on a detached task (the
    /// `PgTokenStore` pattern: tokio-postgres drives the connection;
    /// dropping the `Client` closes it). The handshake is bounded (review
    /// 2026-08-17 #2): a blackholed PG fails fast instead of holding the
    /// pool-of-one mutex for the OS TCP timeout, and a session
    /// `statement_timeout` bounds every statement for the same reason
    /// (every caller serializes behind the one connection).
    async fn connect(pg_url: &str) -> anyhow::Result<tokio_postgres::Client> {
        let (client, conn) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_postgres::connect(pg_url, NoTls),
        )
        .await
        .context("timed out connecting the pushd registry to Postgres (NOSTOS_PUSHD_DATABASE_URL)")?
        .context("connecting the pushd registry to Postgres (NOSTOS_PUSHD_DATABASE_URL)")?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
            .batch_execute("SET statement_timeout = '30s'")
            .await
            .context("setting the registry session statement_timeout")?;
        Ok(client)
    }

    #[async_trait]
    impl Store for PgStore {
        async fn upsert_token(
            &self,
            tenant_id: &str,
            token: &str,
            platform: Platform,
            account_tag: Option<&str>,
        ) -> anyhow::Result<UpsertOutcome> {
            let now = now_rfc3339();
            let platform = platform.as_str();
            self.with_client(|c| async move {
                // ONE atomic statement (audit finding 3): the conflict
                // update is gated on the existing row's owner, so a
                // cross-tenant re-register updates ZERO rows — the
                // zero-row result IS the Conflict signal. Unlike the
                // SQLite twin there is no check-then-act to keep atomic:
                // race-safe by construction, even across processes.
                let params: [&(dyn tokio_postgres::types::ToSql + Sync); 5] =
                    [&token, &platform, &tenant_id, &account_tag, &now];
                let n = c
                    .execute(
                        "INSERT INTO push_tokens \
                         (token, platform, tenant_id, account_tag, created_at, updated_at) \
                         VALUES ($1, $2, $3, $4, $5, $5) \
                         ON CONFLICT (token) DO UPDATE SET \
                             platform = $2, account_tag = $4, updated_at = $5 \
                         WHERE push_tokens.tenant_id = $3",
                        &params,
                    )
                    .await?;
                Ok((
                    c,
                    if n == 0 {
                        UpsertOutcome::Conflict
                    } else {
                        UpsertOutcome::Registered
                    },
                ))
            })
            .await
        }

        async fn delete_token_owner_scoped(
            &self,
            tenant_id: &str,
            token: &str,
        ) -> anyhow::Result<DeleteOutcome> {
            self.with_client(|c| async move {
                let n = c
                    .execute(
                        "DELETE FROM push_tokens WHERE token = $1 AND tenant_id = $2",
                        &[&token, &tenant_id],
                    )
                    .await?;
                if n == 1 {
                    return Ok((c, DeleteOutcome::Deleted));
                }
                // Not the caller's row — Foreign vs Missing is kept for
                // callers that need it; the ROUTE answers 204 either way
                // (audit finding 6, same as the SQLite twin).
                let owner = c
                    .query_opt(
                        "SELECT tenant_id FROM push_tokens WHERE token = $1",
                        &[&token],
                    )
                    .await?;
                Ok((
                    c,
                    match owner {
                        Some(_) => DeleteOutcome::Foreign,
                        None => DeleteOutcome::Missing,
                    },
                ))
            })
            .await
        }

        async fn lookup_token(
            &self,
            tenant_id: &str,
            token: &str,
        ) -> anyhow::Result<Option<TokenRecord>> {
            self.with_client(|c| async move {
                let row = c
                    .query_opt(
                        "SELECT platform, account_tag FROM push_tokens \
                         WHERE token = $1 AND tenant_id = $2",
                        &[&token, &tenant_id],
                    )
                    .await?;
                Ok((
                    c,
                    row.map(|r| {
                        let platform: String = r.get(0);
                        TokenRecord {
                            // Unparseable platform = corruption; the SQLite
                            // twin panics here too (only this crate writes
                            // the column).
                            platform: Platform::parse(&platform)
                                .unwrap_or_else(|| panic!("corrupt platform column: {platform}")),
                            account_tag: r.get(1),
                        }
                    }),
                ))
            })
            .await
        }

        async fn append_receipt(&self, receipt: &NewReceipt) -> anyhow::Result<i64> {
            let metadata = receipt.metadata.as_ref().map(serde_json::Value::to_string);
            let tenant_id = receipt.tenant_id.as_str();
            let push_id = receipt.push_id.as_str();
            let token = receipt.token.as_str();
            let outcome = receipt.outcome.as_str();
            let detail = receipt.detail.as_deref();
            let provider_ts = receipt.provider_ts.as_str();
            self.with_client(|c| async move {
                // RETURNING seq — the identity column hands back the
                // assigned monotonic seq in the same round trip (stronger
                // than the SQLite twin's last_insert_rowid, and race-free
                // by construction).
                let params: [&(dyn tokio_postgres::types::ToSql + Sync); 7] = [
                    &tenant_id,
                    &push_id,
                    &token,
                    &outcome,
                    &detail,
                    &metadata,
                    &provider_ts,
                ];
                let row = c
                    .query_one(
                        "INSERT INTO receipts \
                         (tenant_id, push_id, token, outcome, detail, metadata, provider_ts) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7) \
                         RETURNING seq",
                        &params,
                    )
                    .await?;
                let seq: i64 = row.get(0);
                Ok((c, seq))
            })
            .await
        }

        async fn list_receipts(
            &self,
            tenant_id: &str,
            since: i64,
            limit: u32,
        ) -> anyhow::Result<Vec<StoredReceipt>> {
            let max = i64::from(limit);
            self.with_client(|c| async move {
                let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
                    [&tenant_id, &since, &max];
                let rows = c
                    .query(
                        "SELECT seq, push_id, token, outcome, detail, metadata, provider_ts \
                         FROM receipts WHERE tenant_id = $1 AND seq > $2 \
                         ORDER BY seq ASC LIMIT $3",
                        &params,
                    )
                    .await?;
                let mut out = Vec::with_capacity(rows.len());
                for r in rows {
                    let outcome: String = r.get(3);
                    let metadata: Option<String> = r.get(5);
                    out.push(StoredReceipt {
                        seq: r.get(0),
                        push_id: r.get(1),
                        token: r.get(2),
                        // Unparseable outcome = corruption; fatal is the
                        // honest floor (same as the SQLite twin).
                        outcome: Outcome::parse(&outcome).unwrap_or(Outcome::Fatal),
                        detail: r.get(4),
                        metadata: metadata.and_then(|m| serde_json::from_str(&m).ok()),
                        provider_ts: r.get(6),
                    });
                }
                Ok((c, out))
            })
            .await
        }

        async fn sweep_receipts(&self, retention_secs: u64) -> anyhow::Result<u64> {
            // Same string-comparison sweep as the SQLite twin: provider_ts
            // is fixed-width RFC3339 TEXT, so a fixed-width cutoff
            // compares correctly as a plain string.
            let cutoff = (OffsetDateTime::now_utc()
                - time::Duration::seconds(i64::try_from(retention_secs).unwrap_or(i64::MAX)))
            .format(TS_FORMAT)
            .expect("fixed-shape format");
            self.with_client(|c| async move {
                let n = c
                    .execute("DELETE FROM receipts WHERE provider_ts < $1", &[&cutoff])
                    .await?;
                Ok((c, n))
            })
            .await
        }
    }
}

impl Outcome {
    /// Parse the DB string form (only this crate writes it).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "delivered" => Some(Self::Delivered),
            "unregistered" => Some(Self::Unregistered),
            "transient" => Some(Self::Transient),
            "fatal" => Some(Self::Fatal),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeleteOutcome, NewReceipt, Outcome, Platform, SqliteStore, Store, UpsertOutcome};

    fn receipt(push_id: &str, tenant: &str, provider_ts: &str) -> NewReceipt {
        NewReceipt {
            tenant_id: tenant.to_string(),
            push_id: push_id.to_string(),
            token: "tok".to_string(),
            outcome: Outcome::Delivered,
            detail: None,
            metadata: None,
            provider_ts: provider_ts.to_string(),
        }
    }

    #[tokio::test]
    async fn token_roundtrip_and_owner_scoped_delete() {
        let s = SqliteStore::in_memory().expect("store");
        s.upsert_token("a", "device-token-1", Platform::Fcm, Some("acct"))
            .await
            .expect("upsert");
        let rec = s.lookup_token("a", "device-token-1").await.expect("lookup");
        assert_eq!(
            rec,
            Some(super::TokenRecord {
                platform: Platform::Fcm,
                account_tag: Some("acct".to_string())
            })
        );
        // Tenant-scoped lookup: B sees nothing of A's row.
        assert_eq!(s.lookup_token("b", "device-token-1").await.unwrap(), None);
        // Foreign delete is reported for the 404; own delete succeeds; the
        // second own delete is Missing (idempotent 204).
        assert_eq!(
            s.delete_token_owner_scoped("b", "device-token-1")
                .await
                .unwrap(),
            DeleteOutcome::Foreign
        );
        assert_eq!(
            s.delete_token_owner_scoped("a", "device-token-1")
                .await
                .unwrap(),
            DeleteOutcome::Deleted
        );
        assert_eq!(
            s.delete_token_owner_scoped("a", "device-token-1")
                .await
                .unwrap(),
            DeleteOutcome::Missing
        );
        assert_eq!(s.lookup_token("a", "device-token-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn upsert_re_registration_refreshes_platform() {
        let s = SqliteStore::in_memory().expect("store");
        assert_eq!(
            s.upsert_token("a", "tok-123456", Platform::Apns, None)
                .await
                .unwrap(),
            UpsertOutcome::Registered
        );
        assert_eq!(
            s.upsert_token("a", "tok-123456", Platform::Webpush, Some("tag"))
                .await
                .unwrap(),
            UpsertOutcome::Registered
        );
        let rec = s
            .lookup_token("a", "tok-123456")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(rec.platform, Platform::Webpush);
    }

    /// Audit finding 3: cross-tenant upsert is a Conflict (409 at the
    /// route), ownership never silently reassigns, and the documented
    /// migration path — old owner deletes, new tenant registers — works.
    #[tokio::test]
    async fn cross_tenant_upsert_conflicts_until_owner_deletes() {
        let s = SqliteStore::in_memory().expect("store");
        s.upsert_token("a", "tok-123456", Platform::Apns, None)
            .await
            .unwrap();
        assert_eq!(
            s.upsert_token("b", "tok-123456", Platform::Fcm, None)
                .await
                .unwrap(),
            UpsertOutcome::Conflict
        );
        // Ownership and platform survived the refused attempt.
        let rec = s.lookup_token("a", "tok-123456").await.unwrap();
        assert_eq!(rec.expect("row").platform, Platform::Apns);
        assert_eq!(s.lookup_token("b", "tok-123456").await.unwrap(), None);
        // Migration path: a deletes, then b registers successfully.
        assert_eq!(
            s.delete_token_owner_scoped("a", "tok-123456")
                .await
                .unwrap(),
            DeleteOutcome::Deleted
        );
        assert_eq!(
            s.upsert_token("b", "tok-123456", Platform::Fcm, None)
                .await
                .unwrap(),
            UpsertOutcome::Registered
        );
        let rec = s.lookup_token("b", "tok-123456").await.unwrap();
        assert_eq!(rec.expect("row").platform, Platform::Fcm);
    }

    #[tokio::test]
    async fn receipts_append_ascending_and_tenant_isolated() {
        let s = SqliteStore::in_memory().expect("store");
        let s1 = s
            .append_receipt(&receipt("p1", "a", "2026-01-01T00:00:00.000000000Z"))
            .await
            .unwrap();
        let s2 = s
            .append_receipt(&receipt("p2", "b", "2026-01-01T00:00:01.000000000Z"))
            .await
            .unwrap();
        let s3 = s
            .append_receipt(&receipt("p3", "a", "2026-01-01T00:00:02.000000000Z"))
            .await
            .unwrap();
        assert!(s2 > s1 && s3 > s2, "monotonic seq");
        let a = s.list_receipts("a", 0, 100).await.unwrap();
        assert_eq!(a.len(), 2, "tenant b's receipt is invisible to a");
        assert!(a[0].seq < a[1].seq, "ascending");
        // Cursor: everything after the first of a's receipts.
        let tail = s.list_receipts("a", a[0].seq, 100).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].push_id, "p3");
    }

    #[tokio::test]
    async fn sweep_deletes_only_old_receipts() {
        let s = SqliteStore::in_memory().expect("store");
        s.append_receipt(&receipt("old", "a", "2020-01-01T00:00:00.000000000Z"))
            .await
            .unwrap();
        s.append_receipt(&receipt("new", "a", &super::now_rfc3339()))
            .await
            .unwrap();
        let swept = s.sweep_receipts(60).await.unwrap();
        assert_eq!(swept, 1);
        let left = s.list_receipts("a", 0, 100).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].push_id, "new");
    }
}
