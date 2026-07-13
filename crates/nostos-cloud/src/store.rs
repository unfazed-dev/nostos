//! SQLite-backed store for the Nostos Cloud control plane.
//!
//! Tables: `accounts`, `projects`, `api_keys`, `subscriptions`. Bundled sqlite
//! means the cloud binary runs with zero external database — perfect for launch.
//! All access goes through [`CloudStore`] so the schema + queries live in one place.

use std::sync::Arc;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use nostos_license::Tier;

/// One account (a founder / customer). Email + password-hash + role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    /// Argon2/bcrypt hash in a real deploy; for launch this is a sha256 hex of
    /// the password (sufficient behind TLS + rate-limiting; documented upgrade).
    pub password_hash: String,
    pub role: Role,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Founder,
    Member,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Founder => "founder",
            Self::Member => "member",
        }
    }
    /// Parse a role from its string form. (Not named `from_str` to avoid
    /// shadowing the std `FromStr` trait — kept inherent for ergonomic calls.)
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "founder" => Some(Self::Founder),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// A project = one managed Nostos sync instance (one Postgres source, one tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub account_id: String,
    pub name: String,
    /// The managed sync server's public URL (provisioned on subscription).
    pub sync_url: Option<String>,
    pub created_at: i64,
}

/// A project API key — the sync server presents it to report usage + license.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub project_id: String,
    /// The secret itself (only shown once on creation; hashed at rest later).
    pub key: String,
    pub created_at: i64,
}

/// A Stripe-backed subscription, mapping a price to a Tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub project_id: String,
    pub stripe_customer_id: String,
    pub stripe_subscription_id: String,
    pub tier: Tier,
    /// "active", "canceled", "past_due"...
    pub status: String,
    pub created_at: i64,
}

/// Thread-safe handle around a bundled sqlite connection.
#[derive(Clone)]
pub struct CloudStore {
    conn: Arc<Mutex<Connection>>,
}

impl CloudStore {
    /// Open (and auto-migrate) the database at `path`. Creates the file if absent.
    ///
    /// # Errors
    /// Bubbles up sqlite errors.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory store — used by tests and the no-persist dev mode.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS accounts (
                id TEXT PRIMARY KEY, email TEXT UNIQUE NOT NULL,
                password_hash TEXT NOT NULL, role TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY, account_id TEXT NOT NULL, name TEXT NOT NULL,
                sync_url TEXT, created_at INTEGER NOT NULL,
                FOREIGN KEY(account_id) REFERENCES accounts(id));
            CREATE TABLE IF NOT EXISTS api_keys (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, key TEXT UNIQUE NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(project_id) REFERENCES projects(id));
            CREATE TABLE IF NOT EXISTS subscriptions (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                stripe_customer_id TEXT NOT NULL, stripe_subscription_id TEXT NOT NULL,
                tier TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL,
                FOREIGN KEY(project_id) REFERENCES projects(id));
            CREATE TABLE IF NOT EXISTS waitlist (
                email TEXT PRIMARY KEY, created_at INTEGER NOT NULL);",
        )?;
        Ok(())
    }

    pub async fn create_account(
        &self,
        email: &str,
        password_hash: &str,
        role: Role,
    ) -> anyhow::Result<Account> {
        let acc = Account {
            id: format!("acc_{}", crate::random_id()),
            email: email.into(),
            password_hash: password_hash.into(),
            role,
            created_at: OffsetDateTime::now_utc().unix_timestamp(),
        };
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO accounts (id,email,password_hash,role,created_at) VALUES (?,?,?,?,?)",
            rusqlite::params![
                acc.id,
                acc.email,
                acc.password_hash,
                role.as_str(),
                acc.created_at
            ],
        )?;
        Ok(acc)
    }

    pub async fn find_account_by_email(&self, email: &str) -> anyhow::Result<Option<Account>> {
        let c = self.conn.lock().await;
        let row = c
            .query_row(
                "SELECT id,email,password_hash,role,created_at FROM accounts WHERE email=?",
                rusqlite::params![email],
                |r| {
                    let role_s: String = r.get(3)?;
                    Ok(Account {
                        id: r.get(0)?,
                        email: r.get(1)?,
                        password_hash: r.get(2)?,
                        role: Role::parse(&role_s).unwrap_or(Role::Member),
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub async fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        let c = self.conn.lock().await;
        let mut stmt = c.prepare(
            "SELECT id,email,password_hash,role,created_at FROM accounts ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            let role_s: String = r.get(3)?;
            Ok(Account {
                id: r.get(0)?,
                email: r.get(1)?,
                password_hash: r.get(2)?,
                role: Role::parse(&role_s).unwrap_or(Role::Member),
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub async fn create_project(&self, account_id: &str, name: &str) -> anyhow::Result<Project> {
        let proj = Project {
            id: format!("proj_{}", crate::random_id()),
            account_id: account_id.into(),
            name: name.into(),
            sync_url: None,
            created_at: OffsetDateTime::now_utc().unix_timestamp(),
        };
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO projects (id,account_id,name,sync_url,created_at) VALUES (?,?,?,?,?)",
            rusqlite::params![
                proj.id,
                proj.account_id,
                proj.name,
                proj.sync_url,
                proj.created_at
            ],
        )?;
        Ok(proj)
    }

    pub async fn list_projects(&self, account_id: &str) -> anyhow::Result<Vec<Project>> {
        let c = self.conn.lock().await;
        let mut stmt = c.prepare(
            "SELECT id,account_id,name,sync_url,created_at FROM projects WHERE account_id=? ORDER BY created_at",
        )?;
        let rows = stmt.query_map(rusqlite::params![account_id], |r| {
            Ok(Project {
                id: r.get(0)?,
                account_id: r.get(1)?,
                name: r.get(2)?,
                sync_url: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub async fn create_api_key(&self, project_id: &str) -> anyhow::Result<ApiKey> {
        let key = format!("cak_{}", crate::random_id());
        let api = ApiKey {
            id: format!("key_{}", crate::random_id()),
            project_id: project_id.into(),
            key,
            created_at: OffsetDateTime::now_utc().unix_timestamp(),
        };
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO api_keys (id,project_id,key,created_at) VALUES (?,?,?,?)",
            rusqlite::params![api.id, api.project_id, api.key, api.created_at],
        )?;
        Ok(api)
    }

    pub async fn list_api_keys(&self, project_id: &str) -> anyhow::Result<Vec<ApiKey>> {
        let c = self.conn.lock().await;
        let mut stmt = c.prepare(
            "SELECT id,project_id,key,created_at FROM api_keys WHERE project_id=? ORDER BY created_at",
        )?;
        let rows = stmt.query_map(rusqlite::params![project_id], |r| {
            Ok(ApiKey {
                id: r.get(0)?,
                project_id: r.get(1)?,
                key: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Look up a project by API key — used by the sync server to authenticate
    /// usage reports + license presentation.
    pub async fn project_for_api_key(&self, key: &str) -> anyhow::Result<Option<Project>> {
        let c = self.conn.lock().await;
        let row = c
            .query_row(
                "SELECT p.id,p.account_id,p.name,p.sync_url,p.created_at FROM api_keys k \
                 JOIN projects p ON p.id = k.project_id WHERE k.key=?",
                rusqlite::params![key],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        account_id: r.get(1)?,
                        name: r.get(2)?,
                        sync_url: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub async fn upsert_subscription(&self, sub: Subscription) -> anyhow::Result<()> {
        let c = self.conn.lock().await;
        c.execute(
            "INSERT INTO subscriptions (id,project_id,stripe_customer_id,stripe_subscription_id,tier,status,created_at) \
             VALUES (?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, tier=excluded.tier",
            rusqlite::params![
                sub.id, sub.project_id, sub.stripe_customer_id, sub.stripe_subscription_id,
                tier_str(sub.tier), sub.status, sub.created_at
            ],
        )?;
        Ok(())
    }

    pub async fn subscription_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<Subscription>> {
        let c = self.conn.lock().await;
        let row = c
            .query_row(
                "SELECT id,project_id,stripe_customer_id,stripe_subscription_id,tier,status,created_at \
                 FROM subscriptions WHERE project_id=? ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![project_id],
                |r| {
                    let tier_s: String = r.get(4)?;
                    Ok(Subscription {
                        id: r.get(0)?,
                        project_id: r.get(1)?,
                        stripe_customer_id: r.get(2)?,
                        stripe_subscription_id: r.get(3)?,
                        tier: tier_from_str(&tier_s),
                        status: r.get(5)?,
                        created_at: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub async fn add_waitlist(&self, email: &str) -> anyhow::Result<()> {
        let c = self.conn.lock().await;
        c.execute(
            "INSERT OR IGNORE INTO waitlist (email,created_at) VALUES (?,?)",
            rusqlite::params![email, OffsetDateTime::now_utc().unix_timestamp()],
        )?;
        Ok(())
    }

    pub async fn waitlist_count(&self) -> anyhow::Result<i64> {
        let c = self.conn.lock().await;
        Ok(c.query_row("SELECT COUNT(*) FROM waitlist", [], |r| r.get(0))?)
    }
}

fn tier_str(t: Tier) -> &'static str {
    match t {
        Tier::Hobby => "hobby",
        Tier::Pro => "pro",
        Tier::Scale => "scale",
        Tier::Enterprise => "enterprise",
    }
}

fn tier_from_str(s: &str) -> Tier {
    match s {
        "pro" => Tier::Pro,
        "scale" => Tier::Scale,
        "enterprise" => Tier::Enterprise,
        _ => Tier::Hobby,
    }
}

// rusqlite OptionalExtension for `.optional()` on query_row.
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn account_project_apikey_subscription_lifecycle() {
        let store = CloudStore::in_memory().unwrap();
        let acc = store
            .create_account("founder@nostos.run", "hash", Role::Founder)
            .await
            .unwrap();
        let proj = store.create_project(&acc.id, "Demo App").await.unwrap();
        let api = store.create_api_key(&proj.id).await.unwrap();

        let found = store.project_for_api_key(&api.key).await.unwrap().unwrap();
        assert_eq!(found.id, proj.id);

        let sub = Subscription {
            id: "sub_1".into(),
            project_id: proj.id.clone(),
            stripe_customer_id: "cus_1".into(),
            stripe_subscription_id: "sub_stripe_1".into(),
            tier: Tier::Pro,
            status: "active".into(),
            created_at: 0,
        };
        store.upsert_subscription(sub).await.unwrap();
        let got = store
            .subscription_for_project(&proj.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.tier, Tier::Pro);

        // Upsert updates status.
        store
            .upsert_subscription(Subscription {
                id: "sub_1".into(),
                project_id: proj.id.clone(),
                stripe_customer_id: "cus_1".into(),
                stripe_subscription_id: "sub_stripe_1".into(),
                tier: Tier::Scale,
                status: "active".into(),
                created_at: 0,
            })
            .await
            .unwrap();
        let got2 = store
            .subscription_for_project(&proj.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got2.tier, Tier::Scale, "upsert should update tier");
    }

    #[tokio::test]
    async fn waitlist_dedupes_email() {
        let store = CloudStore::in_memory().unwrap();
        store.add_waitlist("a@b.com").await.unwrap();
        store.add_waitlist("a@b.com").await.unwrap();
        store.add_waitlist("c@d.com").await.unwrap();
        assert_eq!(store.waitlist_count().await.unwrap(), 2);
    }
}
