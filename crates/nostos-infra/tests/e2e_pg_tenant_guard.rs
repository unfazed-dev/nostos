//! Boot-time tenant-column guard (audit_tenant_column): a tenant-column
//! deploy must be able to SEE, at boot, which synced tables don't carry the
//! configured tenant column — the 2026-08-27 incident (starving shop catalog,
//! swallowed snapshot 42703s, default org_id nobody set) becomes a one-log-
//! line diagnosis instead of an hour of raw-WS probing.
//!
//! ## Running
//!
//! make pg-up; NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_tenant_guard -- --nocapture --test-threads=1
//!
//! Self-skips when NOSTOS_E2E_PG is unset — the suite's shared convention.
//! Uses dedicated throwaway tables (NOT the shared cairn_pub fixtures) so
//! the classification queries touch nothing other suites assert on.

#![cfg(feature = "pg")]

use nostos_infra::snapshot_source::audit_tenant_column;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
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

/// Three shapes, classified exactly: a tenant-scoped table (has the column),
/// a deliberately-global table (exists, lacks the column), and a ruleset
/// typo (table that does not exist).
#[tokio::test]
async fn audit_classifies_columnless_missing_and_ok() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("SKIPPED: {E2E_FLAG} not set (no real Postgres)");
        return;
    }
    let sql = sql_client().await;
    sql.execute(
        "CREATE TABLE IF NOT EXISTS tg_scoped_ws5 (id int primary key, org_id text)",
        &[],
    )
    .await
    .unwrap();
    sql.execute(
        "CREATE TABLE IF NOT EXISTS tg_global_ws5 (id int primary key, name text)",
        &[],
    )
    .await
    .unwrap();

    let audit = audit_tenant_column(
        &pg_url(),
        "org_id",
        &[
            "tg_scoped_ws5".to_string(),
            "tg_global_ws5".to_string(),
            "tg_typo_ws5".to_string(),
        ],
    )
    .await
    .expect("audit runs against the catalog");

    assert_eq!(audit.columnless, vec!["tg_global_ws5".to_string()]);
    assert_eq!(audit.missing, vec!["tg_typo_ws5".to_string()]);
    // The scoped table appears in NEITHER list.
    assert!(!audit
        .columnless
        .iter()
        .chain(audit.missing.iter())
        .any(|t| t == "tg_scoped_ws5"));
}
