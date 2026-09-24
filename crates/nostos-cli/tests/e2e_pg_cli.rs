//! Real-Postgres e2e for the CLI's control-plane logic (`PgControl`).
//!
//! Env-gated exactly like `nostos-infra`'s `e2e_pg_*` tests (see
//! `crates/nostos-infra/tests/e2e_pg_replication.rs`) — skipped unless
//! `NOSTOS_E2E_PG=1` so unit-test CI stays green:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-cli --test e2e_pg_cli -- --nocapture --test-threads=1
//! ```
//!
//! Uses its own publication name (`nostos_cli_test_pub*`, never `cairn_pub`)
//! and throwaway tables (random-suffixed, dropped at the end) so this never
//! collides with other agents' concurrent e2e runs against the same docker
//! Postgres.

use nostos_cli::pg::{PgControl, PublicationAction};

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

fn unique_table(label: &str) -> String {
    format!("nostos_cli_test_{label}_{}", uuid::Uuid::new_v4().simple())
}

/// Covers the `init`/`doctor` control-plane path: connect, `wal_level`,
/// create/reconcile the publication (create → unchanged → tables-updated),
/// read it back read-only, and read slot headroom/status — everything
/// `PgControl` does, driven the same way `commands::init`/`commands::doctor`
/// drive it, without needing a live terminal for stdin prompts.
#[tokio::test]
async fn init_and_doctor_flow_against_real_postgres() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }

    let table_a = unique_table("a");
    let table_b = unique_table("b");
    let publication = "nostos_cli_test_pub";

    let sql = sql_client().await;
    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .expect("drop stale publication");
    sql.batch_execute(&format!("CREATE TABLE {table_a} (id serial primary key)"))
        .await
        .expect("create table a");
    sql.batch_execute(&format!("CREATE TABLE {table_b} (id serial primary key)"))
        .await
        .expect("create table b");

    let pg = PgControl::connect(&pg_url())
        .await
        .expect("PgControl::connect");

    // wal_level=logical — docker-compose sets this (docker/docker-compose.yml).
    assert_eq!(pg.wal_level().await.expect("wal_level"), "logical");

    // Fresh publication -> Created.
    let action = pg
        .ensure_publication(publication, std::slice::from_ref(&table_a))
        .await
        .expect("create publication");
    assert_eq!(action, PublicationAction::Created);

    // Re-run with the same table set -> Unchanged (idempotent re-init).
    let action = pg
        .ensure_publication(publication, std::slice::from_ref(&table_a))
        .await
        .expect("unchanged publication");
    assert_eq!(action, PublicationAction::Unchanged);

    // Re-run with a different table set -> TablesUpdated (config drift reconciled).
    let action = pg
        .ensure_publication(publication, &[table_a.clone(), table_b.clone()])
        .await
        .expect("update publication tables");
    assert_eq!(action, PublicationAction::TablesUpdated);

    // Read-only path (what `doctor` uses) reflects the update.
    let mut tables = pg
        .publication_tables(publication)
        .await
        .expect("publication_tables")
        .expect("publication exists");
    tables.sort();
    let mut expected = vec![table_a.clone(), table_b.clone()];
    expected.sort();
    assert_eq!(tables, expected);

    // Slot headroom is readable and internally consistent.
    let headroom = pg.slot_headroom().await.expect("slot_headroom");
    assert!(
        headroom.max > 0,
        "max_replication_slots should be > 0 in the test compose"
    );
    assert!(headroom.used <= headroom.max);

    // This test never runs nostos-server/PgReplicator, so the eventual
    // application slot must not exist yet — `init`/`doctor` never create it
    // (see crate::pg module docs: the server owns slot creation).
    let status = pg
        .slot_status("nostos_cli_test_slot_never_created")
        .await
        .expect("slot_status");
    assert!(!status.exists);

    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();
    sql.batch_execute(&format!("DROP TABLE IF EXISTS {table_a}"))
        .await
        .ok();
    sql.batch_execute(&format!("DROP TABLE IF EXISTS {table_b}"))
        .await
        .ok();
}

/// `CREATE PUBLICATION ... FOR TABLE` requires every listed table to exist —
/// `ensure_publication` must surface that as a clear, actionable error
/// rather than a raw driver error.
#[tokio::test]
async fn publication_for_a_nonexistent_table_errors_clearly() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }

    let publication = "nostos_cli_test_pub_missing_table";
    let sql = sql_client().await;
    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();

    let pg = PgControl::connect(&pg_url())
        .await
        .expect("PgControl::connect");
    let missing_table = unique_table("missing");
    let err = pg
        .ensure_publication(publication, &[missing_table])
        .await
        .expect_err("publication for a missing table must fail");
    assert!(
        format!("{err:#}").contains("creating publication"),
        "error should name the operation, got: {err:#}"
    );

    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();
}
