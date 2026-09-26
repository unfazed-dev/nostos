//! Apply or verify the numbered Atlet TablesDB schema without silent drift repair.

use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::json;

use crate::{
    appwrite_client::AppwriteClient,
    migrations::{Migration, MigrationCatalog},
};

/// Result of checking or applying all known migrations.
#[derive(Debug, Default)]
pub struct MigrationStatus {
    /// Versions already recorded in the cloud ledger.
    pub applied: Vec<u32>,
    /// Versions still absent from the cloud ledger.
    pub missing: Vec<u32>,
}

/// Compare every migration with the remote project, optionally creating missing resources.
///
/// Existing resources are checked field by field. A mismatch is an error, never
/// an invitation to overwrite a live schema. The ledger row is written last.
///
/// # Errors
/// Returns an API error or a schema/checksum drift error.
pub async fn run(
    client: &AppwriteClient,
    catalog: &MigrationCatalog,
    apply: bool,
) -> Result<MigrationStatus> {
    let mut status = MigrationStatus::default();
    for migration in catalog.migrations() {
        let ledger_path = format!(
            "tablesdb/{}/tables/schema_migrations/rows/{:04}",
            migration.database_id(),
            migration.version()
        );
        let ledger = client.get(&ledger_path).await?;
        if let Some(row) = &ledger {
            let data = row.get("data").unwrap_or(row);
            if data["checksum"].as_str() != Some(migration.checksum()) {
                bail!(
                    "migration {:04} checksum differs from the cloud ledger; never edit applied migrations",
                    migration.version()
                );
            }
        } else if !apply {
            status.missing.push(migration.version());
            continue;
        }

        ensure_schema(client, migration, apply).await?;
        if ledger.is_none() {
            client
                .post(
                    &format!(
                        "tablesdb/{}/tables/schema_migrations/rows",
                        migration.database_id()
                    ),
                    &json!({
                        "rowId": format!("{:04}", migration.version()),
                        "data": {
                            "version": migration.version(),
                            "checksum": migration.checksum(),
                            "applied_at": now_iso8601(),
                        },
                        "permissions": [],
                    }),
                )
                .await
                .with_context(|| format!("record Appwrite migration {:04}", migration.version()))?;
        }
        status.applied.push(migration.version());
    }
    Ok(status)
}

async fn ensure_schema(client: &AppwriteClient, migration: &Migration, apply: bool) -> Result<()> {
    let database = migration.database();
    let database_path = format!("tablesdb/{}", database.database_id);
    match client.get(&database_path).await? {
        Some(remote) if remote["name"] == database.name => {}
        Some(_) => bail!("database {} name drift", database.database_id),
        None if apply => {
            client
                .post(
                    "tablesdb",
                    &json!({"databaseId": database.database_id, "name": database.name}),
                )
                .await?;
        }
        None => bail!("database {} is missing", database.database_id),
    }

    let team = migration.team();
    let team_path = format!("teams/{}", team.team_id);
    match client.get(&team_path).await? {
        Some(remote) if remote["name"] == team.name => {}
        Some(_) => bail!("team {} name drift", team.team_id),
        None if apply => {
            client
                .post("teams", &json!({"teamId": team.team_id, "name": team.name}))
                .await?;
        }
        None => bail!("team {} is missing", team.team_id),
    }

    for table in migration.tables() {
        let path = format!(
            "tablesdb/{}/tables/{}",
            database.database_id, table.table_id
        );
        let remote = match client.get(&path).await? {
            Some(remote) => remote,
            None if apply => client
                .post(
                    &format!("tablesdb/{}/tables", database.database_id),
                    &json!({
                        "tableId": table.table_id,
                        "name": table.name,
                        "permissions": table.permissions(),
                        "rowSecurity": table.row_security(),
                        "columns": table.columns,
                        "indexes": table.indexes,
                    }),
                )
                .await
                .with_context(|| format!("create Appwrite table {}", table.table_id))?,
            None => bail!("table {} is missing", table.table_id),
        };
        table.check_remote(&remote)?;
    }

    for row in migration.bootstrap_rows() {
        let path = format!(
            "tablesdb/{}/tables/{}/rows/{}",
            database.database_id, row.table_id, row.row_id
        );
        if client.get(&path).await?.is_none() {
            if !apply {
                bail!("bootstrap row {}/{} is missing", row.table_id, row.row_id);
            }
            client
                .post(
                    &format!(
                        "tablesdb/{}/tables/{}/rows",
                        database.database_id, row.table_id
                    ),
                    &json!({"rowId": row.row_id, "data": row.data, "permissions": []}),
                )
                .await
                .with_context(|| format!("create bootstrap row {}/{}", row.table_id, row.row_id))?;
        }
    }
    Ok(())
}

fn now_iso8601() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
