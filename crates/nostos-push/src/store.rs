//! Daemon-owned registry store (ADR-0038 §4, plan task 1.2; schema pin
//! 0.3): push tokens + the append-only receipt log.
//!
//! SQLite is the single-binary default — the nostos-cloud store.rs pattern:
//! rusqlite bundled, one connection behind an Arc<tokio::sync::Mutex>, and
//! a migrate() that is idempotent CREATE IF NOT EXISTS. The [Store] trait
//! exists so the storage engine is a seam, not a fait accompli.
//!
//! ponytail: the Postgres impl (pool-of-one PgTokenStore pattern, behind a
//! pg feature) is deferred to v1.1 per pin 0.3's time-box rule — it adds a
//! second full SQL surface + live-PG test leg without changing one byte of
//! the callers above this trait. Upgrade path: a `pg` feature + a
//! PgStore in this module; AppState keeps holding Arc<dyn Store>.
//!
//! Two columns sit beyond pin 0.3's abbreviated list, both demanded by the
//! ratified API contract: receipts.tenant_id (GET /v1/receipts is
//! tenant-scoped — without the column the oracle-safe isolation test is
//! unimplementable) and receipts.metadata (the Receipt schema echoes the
//! send's metadata; it is the push-LSN correlation channel of plan 0.4).

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

/// What an owner-scoped delete found (plan task 1.4): the oracle-safe 404
/// needs to distinguish "belongs to another tenant" from "never existed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The caller's row — gone now.
    Deleted,
    /// The token exists but belongs to another tenant — 404.
    Foreign,
    /// No such token anywhere — idempotent 204.
    Missing,
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
    /// Upsert the caller's token row. A device that changed tenants
    /// re-registers and ownership follows the latest registration — device
    /// tokens are provider-issued and globally unique, so the "overwrite"
    /// case is a migration, not an exfiltration (lookups stay tenant-scoped).
    async fn upsert_token(
        &self,
        tenant_id: &str,
        token: &str,
        platform: Platform,
        account_tag: Option<&str>,
    ) -> anyhow::Result<()>;

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
    ) -> anyhow::Result<()> {
        let now = now_rfc3339();
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO push_tokens (token, platform, tenant_id, account_tag, created_at, \
             updated_at) VALUES (?1,?2,?3,?4,?5,?5)
             ON CONFLICT(token) DO UPDATE SET
                platform=?2, tenant_id=?3, account_tag=?4, updated_at=?5",
            rusqlite::params![token, platform.as_str(), tenant_id, account_tag, now],
        )?;
        Ok(())
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
        // Not the caller's row — but is it anyone's? That distinction is the
        // oracle-safe 404: foreign = 404, never-existed = idempotent 204.
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
    use super::{DeleteOutcome, NewReceipt, Outcome, Platform, SqliteStore, Store};

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
        s.upsert_token("a", "tok-123456", Platform::Apns, None)
            .await
            .unwrap();
        s.upsert_token("a", "tok-123456", Platform::Webpush, Some("tag"))
            .await
            .unwrap();
        let rec = s
            .lookup_token("a", "tok-123456")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(rec.platform, Platform::Webpush);
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
