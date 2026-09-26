use atlet_harness::migrations::MigrationCatalog;
use serde_json::json;

#[test]
fn initial_migration_covers_the_flutter_tables_and_protects_private_rows() {
    let catalog = MigrationCatalog::load().expect("versioned migration must parse");
    let first = &catalog.migrations()[0];

    assert_eq!(first.version(), 1);
    assert_eq!(first.database_id(), "atlet");
    assert_eq!(first.tables().len(), 10);

    let second = &catalog.migrations()[1];
    assert_eq!(second.version(), 2);
    let mutations = second.table("sync_mutations").expect("idempotency ledger");
    assert!(mutations.permissions().is_empty());

    for id in [
        "sessions",
        "products",
        "cart_items",
        "orders",
        "order_events",
        "attachments",
        "user_profiles",
    ] {
        let table = first.table(id).expect("Flutter table missing");
        assert!(table.row_security(), "{id} needs row security");
        assert!(table.permissions().is_empty(), "{id} grants direct access");
    }

    for id in ["sync_clock", "sync_changes", "schema_migrations"] {
        let table = first.table(id).expect("protocol table missing");
        assert!(table.permissions().is_empty(), "{id} grants direct access");
    }
}

#[test]
fn migration_checksum_is_a_stable_sha256_of_the_checked_in_file() {
    let catalog = MigrationCatalog::load().expect("versioned migration must parse");
    assert_eq!(
        catalog.migrations()[0].checksum(),
        "b6df8b4c72d1b94e2d3fbe8626887702268457e81cf70765578d287873d92821"
    );
}

#[test]
fn remote_schema_check_refuses_a_table_that_exposes_private_rows() {
    let catalog = MigrationCatalog::load().expect("versioned migration must parse");
    let sessions = catalog.migrations()[0]
        .table("sessions")
        .expect("sessions declaration");
    let remote = json!({
        "$id": "sessions",
        "$permissions": ["read(\"any\")"],
        "rowSecurity": false,
        "columns": sessions.columns,
        "indexes": sessions.indexes,
    });

    let error = sessions
        .check_remote(&remote)
        .expect_err("a public sessions table must fail the migration check");
    assert!(error.to_string().contains("row security"));
}
