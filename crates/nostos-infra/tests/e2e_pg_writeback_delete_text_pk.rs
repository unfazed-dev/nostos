//! Regression: `PgWriteBack::delete` bound a uuid-shaped pk / tenant value in
//! a TEXT column as a typed `Uuid` — tokio-postgres rejects that client-side
//! with `ok:false "error serializing parameter 0"` (found live in the atlet
//! checkout: `cart_items` uses `id text` holding uuid strings, so every
//! cart-clear delete at checkout failed and dead-lettered). The upsert path
//! coerces its values against the prepared statement (`coerce_params`); the
//! delete path never did.
//!
//! Drives the adapter directly — the WS layer above it is contract-tested in
//! `e2e_pg_writeback.rs`; this bug lives entirely in `delete()`'s binding.
//! Real-PG verification, so it self-skips when `NOSTOS_E2E_PG` is unset
//! (NOSTOS_E2E_PG convention — false-positive green without the flag).
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg \
//!   --test e2e_pg_writeback_delete_text_pk -- --nocapture
//! ```

#![cfg(feature = "pg")]

use std::collections::HashSet;

use nostos_application::ports::WriteBack;
use nostos_domain::TenantScope;
use nostos_infra::PgWriteBack;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://nostos:nostos@localhost:5433/nostos".into())
}

/// A uuid-shaped pk and tenant value in TEXT columns (the atlet `cart_items`
/// shape) must delete the row on both the tenant-scoped (ADR-0018 CTE) and
/// the unscoped path.
#[tokio::test]
async fn delete_binds_uuid_shaped_text_pk() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("SKIP: {E2E_FLAG} not set (needs a real Postgres)");
        return;
    }
    let (sql, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sql.execute("DROP TABLE IF EXISTS del_text_pk_e2e;", &[])
        .await
        .unwrap();
    sql.execute(
        "CREATE TABLE del_text_pk_e2e (id text PRIMARY KEY, user_id text NOT NULL);",
        &[],
    )
    .await
    .unwrap();
    let pk = uuid::Uuid::new_v4().to_string();
    let tenant = uuid::Uuid::new_v4().to_string();
    sql.execute(
        "INSERT INTO del_text_pk_e2e (id, user_id) VALUES ($1, $2)",
        &[&pk, &tenant],
    )
    .await
    .unwrap();

    let wb = PgWriteBack::new(&pg_url(), HashSet::from(["del_text_pk_e2e".to_string()]));

    // Tenant-scoped path: uuid-shaped pk AND tenant value, both TEXT columns.
    wb.delete(
        "del_text_pk_e2e",
        &pk,
        Some(TenantScope {
            column: "user_id",
            value: &tenant,
        }),
    )
    .await
    .expect("tenant-scoped delete of a uuid-shaped text pk");

    let n: i64 = sql
        .query_one("SELECT count(*) FROM del_text_pk_e2e WHERE id = $1", &[&pk])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "row must be gone (tenant-scoped)");

    // Unscoped path: same uuid-shaped text pk.
    sql.execute(
        "INSERT INTO del_text_pk_e2e (id, user_id) VALUES ($1, $2)",
        &[&pk, &tenant],
    )
    .await
    .unwrap();
    wb.delete("del_text_pk_e2e", &pk, None)
        .await
        .expect("unscoped delete of a uuid-shaped text pk");
    let n: i64 = sql
        .query_one("SELECT count(*) FROM del_text_pk_e2e WHERE id = $1", &[&pk])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "row must be gone (unscoped)");

    sql.execute("DROP TABLE del_text_pk_e2e;", &[])
        .await
        .unwrap();
}
