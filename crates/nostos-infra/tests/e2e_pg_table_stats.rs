//! `PgTableStats` end-to-end (ADR-0031): the `all`-mode startup warning reads
//! row-count estimates from the real Postgres catalog (`pg_class.reltuples`),
//! never `count(*)` — a full scan at boot is unacceptable.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_table_stats -- --nocapture --test-threads=1
//! ```
//!
//! Self-skips when `NOSTOS_E2E_PG` is unset (no real Postgres) — the same gate
//! convention as the rest of the pg e2e suite (see `e2e_pg_schema.rs`).
//!
//! ## Fixture
//!
//! Two dedicated tables in a throwaway publication
//! (`nostos_pub_table_stats_ws4`, NOT the shared `nostos_pub`): `stats_analyzed`
//! is `ANALYZE`d after being seeded, `stats_unanalyzed` is left untouched. The
//! hard assertion is never-negative/never-panic — Postgres may autovacuum the
//! unanalyzed table before this test runs, so its estimate is allowed to come
//! back `Some(_)` too.

#![cfg(feature = "pg")]

use nostos_application::ports::{TableStat, TableStatsSource};
use nostos_infra::PgTableStats;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const PUBLICATION: &str = "nostos_pub_table_stats_ws4";

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

/// Idempotently create two fixture tables + a dedicated throwaway
/// publication. Reusing the shared `nostos_pub` (scoped to `tasks`) would
/// perturb the other e2e suites' event streams.
async fn ensure_table_stats_probe(sql: &tokio_postgres::Client) {
    sql.batch_execute(
        "CREATE TABLE IF NOT EXISTS stats_analyzed ( \
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
            body TEXT \
        ); \
         CREATE TABLE IF NOT EXISTS stats_unanalyzed ( \
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
            body TEXT \
        );",
    )
    .await
    .expect("create stats_analyzed/stats_unanalyzed");

    // Seed rows so an ANALYZE has something to count.
    sql.batch_execute(
        "INSERT INTO stats_analyzed (body) \
         SELECT 'row ' || g FROM generate_series(1, 25) g \
         WHERE NOT EXISTS (SELECT 1 FROM stats_analyzed);",
    )
    .await
    .expect("seed stats_analyzed");

    sql.batch_execute(&format!(
        "DO $$ BEGIN \
         IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{PUBLICATION}') THEN \
         CREATE PUBLICATION {PUBLICATION} FOR TABLE stats_analyzed, stats_unanalyzed; \
         END IF; \
         END $$;"
    ))
    .await
    .expect("create table_stats publication");

    // ANALYZE only stats_analyzed — stats_unanalyzed is left untouched so its
    // reltuples estimate may legitimately be "never analyzed" (`-1`), unless
    // autovacuum has already run for it.
    sql.batch_execute("ANALYZE stats_analyzed;")
        .await
        .expect("analyze stats_analyzed");
}

fn find<'a>(stats: &'a [TableStat], table: &str) -> &'a TableStat {
    stats
        .iter()
        .find(|s| s.table == table)
        .unwrap_or_else(|| panic!("table {table} missing from table_stats"))
}

#[tokio::test]
async fn reltuples_estimate_or_unknown() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("{E2E_FLAG} not set — skipping (needs real Postgres; see `make pg-up`)");
        return;
    }
    let sql = sql_client().await;
    ensure_table_stats_probe(&sql).await;

    let src = PgTableStats::new(&pg_url(), PUBLICATION);
    let stats = src.table_stats().await.expect("table_stats fetch");

    assert_eq!(stats.len(), 2, "unexpected table count");

    let analyzed = find(&stats, "stats_analyzed");
    // ANALYZE ran, so Postgres has a real (non-negative) estimate. The only
    // hard requirement is "never negative" — the estimate value itself is a
    // planner statistic that can drift, so we assert presence, not an exact
    // count.
    assert!(
        analyzed.estimated_rows.is_some(),
        "analyzed table should report an estimate"
    );

    // PG may have autovacuumed stats_unanalyzed before the test ran — both
    // `None` (never analyzed) and `Some(_)` (autovacuum beat us to it) are
    // acceptable, so there is nothing to assert about its value beyond the
    // loop below. `find` already proved the row exists (never a panic).
    let _unanalyzed = find(&stats, "stats_unanalyzed");

    // The hard assertion, for every row regardless of analyze state: no
    // negative estimate ever reaches TableStat (u64 makes this a type-level
    // guarantee, but assert the intent explicitly for anyone reading the
    // test).
    for s in &stats {
        if let Some(rows) = s.estimated_rows {
            assert!(rows < u64::MAX, "estimate for {} looks bogus", s.table);
        }
    }
}
