//! Real-Postgres e2e for `nostos rules init` (Task 15) — the introspecting
//! path (`NostosConfig` + `.env` -> `PgControl::publication_tables` ->
//! `nostos_rules.toml`).
//!
//! Env-gated exactly like `e2e_pg_cli.rs` — skipped unless `NOSTOS_E2E_PG=1`
//! so unit-test CI stays green:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-cli --test e2e_pg_rules -- --nocapture --test-threads=1
//! ```
//!
//! Uses its own publication name (`nostos_cli_rules_test_pub*`, never
//! `cairn_pub`) and throwaway tables (random-suffixed, dropped at the end) so
//! this never collides with other agents' concurrent e2e runs against the
//! same docker Postgres.

use nostos_cli::commands::rules::{InitRulesArgs, RulesArgs, RulesCommand};
use nostos_cli::config::{NostosConfig, DbSection, ServerSection, SyncSection, DEFAULT_FILE_NAME};
use nostos_cli::dotenv;
use nostos_infra::rules_file;

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
    format!(
        "nostos_cli_rules_test_{label}_{}",
        uuid::Uuid::new_v4().simple()
    )
}

fn temp_cwd() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nostos-cli-rules-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_nostos_config(cwd: &std::path::Path, publication: &str, tables: &[String]) {
    let cfg = NostosConfig {
        sync: SyncSection {
            tables: tables.to_vec(),
            write_tables: vec![],
            tenant_column: "org_id".to_string(),
        },
        db: DbSection {
            url_env: "NOSTOS_PG_URL".to_string(),
            publication: publication.to_string(),
            slot: "nostos_cli_rules_test_slot_never_created".to_string(),
        },
        supabase: None,
        server: ServerSection::default(),
    };
    cfg.save(&cwd.join(DEFAULT_FILE_NAME))
        .expect("save nostos.toml");
    dotenv::set(&cwd.join(".env"), "NOSTOS_PG_URL", &pg_url()).expect("write .env");
}

#[tokio::test]
async fn init_writes_one_entry_per_publication_table() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }

    let table_a = unique_table("a");
    let table_b = unique_table("b");
    let publication = "nostos_cli_rules_test_pub";

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
    sql.batch_execute(&format!(
        "CREATE PUBLICATION {publication} FOR TABLE {table_a}, {table_b}"
    ))
    .await
    .expect("create publication");

    let cwd = temp_cwd();
    write_nostos_config(&cwd, publication, &[table_a.clone(), table_b.clone()]);

    let args = RulesArgs {
        command: RulesCommand::Init(InitRulesArgs {
            force: false,
            mode: "toggles".to_string(),
            sync_all: false,
        }),
    };
    nostos_cli::commands::rules::run(args, &cwd)
        .await
        .expect("rules init");

    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);
    let rules = rules_file::load(&rules_path)
        .expect("load rules")
        .expect("rules file exists");
    assert_eq!(rules.tables.len(), 2);
    let mut names: Vec<&str> = rules.tables.iter().map(|t| t.table.as_str()).collect();
    names.sort_unstable();
    let mut expected = vec![table_a.as_str(), table_b.as_str()];
    expected.sort_unstable();
    assert_eq!(names, expected);
    assert!(rules.tables.iter().all(|t| !t.sync));

    // Re-running without --force refuses to overwrite.
    let args = RulesArgs {
        command: RulesCommand::Init(InitRulesArgs {
            force: false,
            mode: "toggles".to_string(),
            sync_all: false,
        }),
    };
    let err = nostos_cli::commands::rules::run(args, &cwd)
        .await
        .expect_err("must refuse to overwrite without --force");
    assert!(
        format!("{err:#}").contains("nostos rules edit"),
        "error should suggest `nostos rules edit`, got: {err:#}"
    );

    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();
    sql.batch_execute(&format!("DROP TABLE IF EXISTS {table_a}"))
        .await
        .ok();
    sql.batch_execute(&format!("DROP TABLE IF EXISTS {table_b}"))
        .await
        .ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// The "empty DB degrades gracefully" behaviour: a publication with zero
/// tables must not fail `init` — it writes a template file and exits 0.
#[tokio::test]
async fn init_on_empty_publication_writes_template_and_succeeds() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }

    let publication = "nostos_cli_rules_test_pub_empty";
    let sql = sql_client().await;
    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();
    sql.batch_execute(&format!("CREATE PUBLICATION {publication}"))
        .await
        .expect("create empty publication");

    let cwd = temp_cwd();
    write_nostos_config(&cwd, publication, &[]);

    let args = RulesArgs {
        command: RulesCommand::Init(InitRulesArgs {
            force: false,
            mode: "toggles".to_string(),
            sync_all: false,
        }),
    };
    nostos_cli::commands::rules::run(args, &cwd)
        .await
        .expect("rules init on empty publication must succeed");

    let rules_path = cwd.join(rules_file::RULES_FILE_NAME);
    let rules = rules_file::load(&rules_path)
        .expect("load rules")
        .expect("rules file exists");
    assert!(rules.tables.is_empty());
    assert!(rules.validate().is_ok());

    let text = std::fs::read_to_string(&rules_path).expect("read rules file");
    assert!(
        text.contains("[tables.example]"),
        "empty-DB file should carry a commented template entry, got:\n{text}"
    );

    sql.batch_execute(&format!("DROP PUBLICATION IF EXISTS {publication}"))
        .await
        .ok();
    std::fs::remove_dir_all(&cwd).ok();
}
