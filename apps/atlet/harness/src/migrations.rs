//! Immutable Appwrite schema manifests shared by the migration CLI and cloud checks.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const INITIAL: &str = include_str!("../../appwrite/migrations/0001_initial.json");
const MUTATIONS: &str = include_str!("../../appwrite/migrations/0002_sync_mutations.json");
const SOURCES: &[&str] = &[INITIAL, MUTATIONS];

/// All numbered schema changes known to this build, in application order.
pub struct MigrationCatalog {
    migrations: Vec<Migration>,
}

impl MigrationCatalog {
    /// Parse and validate the checked-in manifests.
    ///
    /// # Errors
    /// Returns an error if a manifest is malformed or versions/IDs repeat.
    pub fn load() -> Result<Self> {
        let mut migrations = Vec::new();
        for source in SOURCES {
            let mut migration: Migration =
                serde_json::from_str(source).context("parse Appwrite migration")?;
            migration.checksum = hex::encode(Sha256::digest(source.as_bytes()));
            migration.validate()?;
            if migrations
                .last()
                .is_some_and(|previous: &Migration| previous.version >= migration.version)
            {
                bail!("Appwrite migration versions must strictly increase");
            }
            migrations.push(migration);
        }
        Ok(Self { migrations })
    }

    /// Migration files in ascending version order.
    #[must_use]
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }
}

/// One append-only Appwrite schema migration.
#[derive(Debug, Deserialize)]
pub struct Migration {
    version: u32,
    database: Database,
    team: Team,
    tables: Vec<Table>,
    bootstrap_rows: Vec<BootstrapRow>,
    #[serde(skip)]
    checksum: String,
}

impl Migration {
    /// Monotonic migration number.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Appwrite TablesDB database ID.
    #[must_use]
    pub fn database_id(&self) -> &str {
        &self.database.database_id
    }

    /// Database declaration used by the migration runner.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Admin team declaration used by the migration runner.
    #[must_use]
    pub fn team(&self) -> &Team {
        &self.team
    }

    /// Table declarations used by the migration runner.
    #[must_use]
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// A named table, if this migration creates it.
    #[must_use]
    pub fn table(&self, id: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.table_id == id)
    }

    /// Seed rows inserted after all tables are ready.
    #[must_use]
    pub fn bootstrap_rows(&self) -> &[BootstrapRow] {
        &self.bootstrap_rows
    }

    /// SHA-256 of the exact checked-in manifest bytes.
    #[must_use]
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    fn validate(&self) -> Result<()> {
        if self.version == 0 || self.database.database_id.is_empty() || self.tables.is_empty() {
            bail!("Appwrite migration needs a version, database and tables");
        }
        let mut table_ids = HashSet::new();
        for table in &self.tables {
            if !table_ids.insert(&table.table_id) {
                bail!("duplicate Appwrite table ID {}", table.table_id);
            }
            let mut column_keys = HashSet::new();
            for column in &table.columns {
                let key = column
                    .get("key")
                    .and_then(Value::as_str)
                    .context("Appwrite column without key")?;
                if !column_keys.insert(key) {
                    bail!("duplicate column {key} in {}", table.table_id);
                }
            }
        }
        for row in &self.bootstrap_rows {
            if !table_ids.contains(&row.table_id) {
                bail!("bootstrap row targets missing table {}", row.table_id);
            }
        }
        Ok(())
    }
}

/// TablesDB database declaration.
#[derive(Debug, Deserialize)]
pub struct Database {
    pub database_id: String,
    pub name: String,
}

/// Appwrite team declaration.
#[derive(Debug, Deserialize)]
pub struct Team {
    pub team_id: String,
    pub name: String,
}

/// TablesDB table declaration, including columns and indexes.
#[derive(Debug, Deserialize)]
pub struct Table {
    pub table_id: String,
    pub name: String,
    row_security: bool,
    #[serde(default)]
    permissions: Vec<String>,
    pub columns: Vec<Value>,
    #[serde(default)]
    pub indexes: Vec<Value>,
}

impl Table {
    /// Whether per-row permissions are enabled.
    #[must_use]
    pub fn row_security(&self) -> bool {
        self.row_security
    }

    /// Grants configured at table level (empty for server-only tables).
    #[must_use]
    pub fn permissions(&self) -> &[String] {
        &self.permissions
    }

    /// Reject a live table whose security or declared schema differs from the migration.
    ///
    /// # Errors
    /// Returns a named drift error; a runner must never silently repair an
    /// existing table because changing permissions or column types can lose data.
    pub fn check_remote(&self, remote: &Value) -> Result<()> {
        if remote["rowSecurity"].as_bool() != Some(self.row_security) {
            bail!("{}: row security drift", self.table_id);
        }
        let grants: Vec<String> = remote["$permissions"]
            .as_array()
            .context("remote table omitted permissions")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .context("non-string table permission")
            })
            .collect::<Result<_>>()?;
        if grants != self.permissions {
            bail!("{}: table permissions drift", self.table_id);
        }
        for (kind, wanted, key) in [
            ("column", &self.columns, "key"),
            ("index", &self.indexes, "key"),
        ] {
            let plural = if kind == "column" {
                "columns"
            } else {
                "indexes"
            };
            let actual = remote[plural]
                .as_array()
                .with_context(|| format!("{}: missing {plural}", self.table_id))?;
            if actual.len() != wanted.len() {
                bail!("{}: {kind} count drift", self.table_id);
            }
            for expected in wanted {
                let name = expected[key]
                    .as_str()
                    .context("manifest item without key")?;
                let found = actual
                    .iter()
                    .find(|item| item[key].as_str() == Some(name))
                    .with_context(|| format!("{}: missing {kind} {name}", self.table_id))?;
                for field in expected
                    .as_object()
                    .context("manifest schema item is not an object")?
                    .keys()
                {
                    let remote_field = if kind == "index" && field == "attributes" {
                        "columns"
                    } else {
                        field
                    };
                    let matches = if kind == "column" && field == "type" && expected[field] == "url"
                    {
                        found["type"] == "string" && found["format"] == "url"
                    } else {
                        found[remote_field] == expected[field]
                    };
                    if !matches {
                        bail!("{}: {kind} {name} field {field} drift", self.table_id);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Initial row required by a new schema.
#[derive(Debug, Deserialize)]
pub struct BootstrapRow {
    pub table_id: String,
    pub row_id: String,
    pub data: Value,
}
