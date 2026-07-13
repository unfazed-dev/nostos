//! Typed-schema endpoint adapter (WS1 — Flutter PowerSync-style redesign,
//! Option-C): read the publication's tables + columns + types from the
//! Postgres catalog and hand them to nostos-server's `GET /schema` as a
//! [`SchemaDescriptor`], so the Flutter SDK can auto-build its typed tables
//! instead of hand-writing a `Schema` (the headline DX win over PowerSync).
//!
//! ## Why this exists
//!
//! nostos-server already bootstraps the publication's relation/column metadata
//! from the catalog at startup (`PgReplicator::bootstrap_relations_from_catalog`
//! → `catalog_relations`), but that metadata is private to the replicator. This
//! adapter exposes the SAME catalog query's result as a schema port so the
//! client can discover it independently of the replication stream.
//!
//! ## Trust boundary
//!
//! Unlike [`PgSnapshotter`](crate::snapshot_source::PgSnapshotter), there is NO
//! client-controlled input on this path: the publication name is server config
//! (`PgReplicatorConfig::publication`), not a frame field. So there is no
//! identifier to validate and no SQL-injection surface — the one catalog query
//! binds the publication name as a parameter (`$1`).
//!
//! ## Affinity (ADR-0019)
//!
//! Each column's SQLite affinity comes from
//! [`crate::replicator::typed::oid_to_sqlite_affinity`], which mirrors the JSON
//! token shape `append_typed_value` emits — so a value the client receives over
//! the wire stores in its typed column without coercion.
//!
//! `unsafe` is forbidden crate-wide. This module performs a single read-only
//! catalog query per `fetch`.

#![cfg(feature = "pg")]

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;

use nostos_application::ports::{
    SchemaColumn, SchemaDescriptor, SchemaError, SchemaSource, SchemaTable,
};

use crate::replicator::pg::catalog_relations;
use crate::replicator::typed::oid_to_sqlite_affinity;

/// A [`SchemaSource`] that reads the publication's typed schema from the source
/// Postgres via a pool-of-one `tokio_postgres::Client` (mirrors
/// [`PgSnapshotter`](crate::snapshot_source::PgSnapshotter)'s lazy-connect
/// discipline). Constructed by the composition root under
/// `NOSTOS_REPLICATOR=pg`; injected into the server for `GET /schema`.
pub struct PgSchemaSource {
    pg_url: String,
    publication: String,
    /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection is replaced
    /// transparently on the next `fetch` — same discipline as `PgSnapshotter`.
    client: Arc<Mutex<Option<tokio_postgres::Client>>>,
}

impl PgSchemaSource {
    /// Construct with a libpq-style URL + the publication name. Does NOT
    /// connect — the first `fetch` opens the connection lazily (and reopens it
    /// transparently if it ever dies).
    #[must_use]
    pub fn new(pg_url: &str, publication: &str) -> Self {
        Self {
            pg_url: pg_url.to_string(),
            publication: publication.to_string(),
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Obtain a connected client, opening the connection lazily if none is
    /// cached (identical to `PgSnapshotter::client`).
    async fn client(&self) -> Result<tokio_postgres::Client, SchemaError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.take() {
            return Ok(c);
        }
        let (client, conn) = tokio_postgres::connect(&self.pg_url, NoTls)
            .await
            .map_err(|e| SchemaError::Backend(format!("connect: {e}")))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(client)
    }

    /// Return a client to the pool after a successful read.
    async fn return_client(&self, client: tokio_postgres::Client) {
        let mut guard = self.client.lock().await;
        *guard = Some(client);
    }

    /// Drop the client slot after an error that may have killed the
    /// connection. The next `fetch` reopens.
    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
    }
}

#[async_trait]
impl SchemaSource for PgSchemaSource {
    async fn fetch(&self) -> Result<SchemaDescriptor, SchemaError> {
        let client = self.client().await?;

        let relations = match catalog_relations(&client, &self.publication).await {
            Ok(m) => m,
            Err(e) => {
                self.drop_client().await;
                return Err(SchemaError::Backend(format!("catalog: {e}")));
            }
        };

        // catalog_relations returns a BTreeMap keyed by relation OID, so
        // into_values() yields tables in a stable order — the `/schema`
        // response is byte-stable across calls for the same publication.
        let tables = relations
            .into_values()
            .map(|rel| {
                let primary_key: Vec<String> = rel
                    .pk_indices
                    .iter()
                    .filter_map(|&i| rel.columns.get(i).map(|(n, _)| n.clone()))
                    .collect();
                let columns: Vec<SchemaColumn> = rel
                    .columns
                    .iter()
                    .map(|(name, oid)| SchemaColumn {
                        name: name.clone(),
                        pg_oid: *oid,
                        affinity: oid_to_sqlite_affinity(*oid).to_string(),
                    })
                    .collect();
                SchemaTable {
                    name: rel.qualified_name,
                    primary_key,
                    columns,
                }
            })
            .collect();

        self.return_client(client).await;
        Ok(SchemaDescriptor {
            publication: self.publication.clone(),
            tables,
        })
    }
}
