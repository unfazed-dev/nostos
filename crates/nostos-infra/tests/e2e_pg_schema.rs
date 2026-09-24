//! `PgSchemaSource` end-to-end (WS1, ADR-0021): the `/schema` adapter must read
//! the real Postgres catalog and report the correct columns, affinities, and
//! primary key — not just the hand-fed unit tests in `typed.rs`.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_schema -- --nocapture --test-threads=1
//! ```
//!
//! Self-skips when `NOSTOS_E2E_PG` is unset (no real Postgres) — the same gate
//! convention as the rest of the pg e2e suite.
//!
//! ## Fixture
//!
//! A dedicated `schema_probe` table + a throwaway publication scoped to just
//! that table (`nostos_pub_schema_ws1`, NOT the shared `nostos_pub`), so this test
//! can't perturb other suites. Covers one column per affinity arm: bool/int4
//! (INTEGER), float8 (REAL), and the string-rendered types int8/text/
//! timestamptz/uuid/jsonb (TEXT) — plus a composite-safe uuid PK.

#![cfg(feature = "pg")]

use nostos_application::ports::{SchemaColumn, SchemaSource};
use nostos_infra::PgSchemaSource;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const PUBLICATION: &str = "nostos_pub_schema_ws1";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://nostos:nostos@localhost:5433/nostos".into())
}

async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Idempotently create a dedicated fixture table + its own throwaway
/// publication. Reusing the shared `nostos_pub` (scoped to `tasks`) would
/// perturb the other e2e suites' event streams.
async fn ensure_schema_probe(sql: &tokio_postgres::Client) {
    sql.batch_execute(
        "CREATE TABLE IF NOT EXISTS schema_probe ( \
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
            flag BOOLEAN, \
            qty INT4, \
            big INT8, \
            ratio FLOAT8, \
            body TEXT, \
            ts TIMESTAMPTZ, \
            uid UUID, \
            meta JSONB \
        );",
    )
    .await
    .expect("create schema_probe");
    sql.batch_execute(&format!(
        "DO $$ BEGIN \
         IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{PUBLICATION}') THEN \
         CREATE PUBLICATION {PUBLICATION} FOR TABLE schema_probe; \
         END IF; \
         END $$;"
    ))
    .await
    .expect("create schema_probe publication");
}

/// Assert a column with `name` exists with the expected `(pg_oid, affinity)`.
fn assert_col(columns: &[SchemaColumn], name: &str, oid: i32, affinity: &str) {
    let c = columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("column {name} missing from schema_probe"));
    assert_eq!(c.pg_oid, oid, "pg_oid for {name}");
    assert_eq!(c.affinity.as_str(), affinity, "affinity for {name}");
}

#[tokio::test]
async fn schema_source_reports_typed_catalog() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("{E2E_FLAG} not set — skipping (needs real Postgres; see `make pg-up`)");
        return;
    }
    let sql = sql_client().await;
    ensure_schema_probe(&sql).await;

    let src = PgSchemaSource::new(&pg_url(), PUBLICATION);
    let descriptor = src.fetch().await.expect("schema fetch");

    assert_eq!(descriptor.publication.as_str(), PUBLICATION);
    let table = descriptor
        .tables
        .iter()
        .find(|t| t.name == "schema_probe")
        .expect("schema_probe table present in descriptor");

    // REAL pk from pg_index.indisprimary — not a hardcoded "id" guess (the WS1
    // win over PgSnapshotter::PK_COLUMN / PgWriteBack).
    assert_eq!(table.primary_key, vec!["id".to_string()]);
    assert_eq!(table.columns.len(), 9, "unexpected column count");

    //        column    pg_oid  affinity   (Postgres type — why)
    assert_col(&table.columns, "id", 2950, "TEXT"); // uuid (string-rendered)
    assert_col(&table.columns, "flag", 16, "INTEGER"); // bool → bare JSON bool
    assert_col(&table.columns, "qty", 23, "INTEGER"); // int4 → bare JSON int
    assert_col(&table.columns, "big", 20, "TEXT"); // int8 → string (precision)
    assert_col(&table.columns, "ratio", 701, "REAL"); // float8 → bare JSON number
    assert_col(&table.columns, "body", 25, "TEXT"); // text (unrecognized OID → wildcard)
    assert_col(&table.columns, "ts", 1184, "TEXT"); // timestamptz → RFC3339 string
    assert_col(&table.columns, "uid", 2950, "TEXT"); // uuid
    assert_col(&table.columns, "meta", 3802, "TEXT"); // jsonb → JSON string
}
