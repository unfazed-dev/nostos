//! Cross-tenant snapshot scoping (audit fix 2026-08-17): `PgSnapshotter`
//! MUST restrict its SELECT to the connection's `TenantScope` — before the
//! fix, the snapshot SELECT was unfiltered while the composition root wired
//! the snapshotter unconditionally, so any subscriber in a multi-tenant
//! deployment received EVERY tenant's rows on subscribe. This test pins the
//! contract at the adapter level against real Postgres: a scoped snapshot
//! returns only the principal's tenant rows; an unscoped (anonymous /
//! single-tenant) snapshot stays legitimately unfiltered.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_snapshot_tenant_scope -- --nocapture --test-threads=1
//! ```
//!
//! Gate convention matches the rest of the pg e2e suite (`NOSTOS_E2E_PG=1`
//! self-skips otherwise). Fixtures are DISJOINT per run (fresh org UUIDs)
//! with cleanup, so the shared `tasks` table needs no TRUNCATE here.

#![cfg(feature = "pg")]

use nostos_application::ports::SnapshotSource;
use nostos_domain::{Lsn, RowOp, TenantScope};
use nostos_infra::PgSnapshotter;

/// Env gate. Self-skips when PG isn't available so PG-less CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
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

/// Extract (pk, org_id) from a snapshot event's Insert payload.
fn pk_org(ev: &nostos_domain::ReplicationEvent) -> (String, String) {
    let RowOp::Insert { pk, payload, .. } = &ev.op else {
        panic!("snapshot events are Inserts");
    };
    let v: serde_json::Value = serde_json::from_slice(payload).expect("payload json");
    (
        pk.clone(),
        v["org_id"].as_str().expect("org_id string").to_string(),
    )
}

#[tokio::test]
async fn scoped_snapshot_returns_only_the_principals_tenant_rows() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set NOSTOS_E2E_PG=1)");
        return;
    }
    let sql = sql_client().await;
    // Disjoint per-run fixtures: two fresh tenant UUIDs, two rows each.
    let org_a = uuid::Uuid::new_v4().to_string();
    let org_b = uuid::Uuid::new_v4().to_string();
    for (org, titles) in [(&org_a, ["a1", "a2"]), (&org_b, ["b1", "b2"])] {
        for t in titles {
            sql.execute(
                "INSERT INTO tasks (org_id, title) VALUES (($1::text)::uuid, $2)",
                &[&org, &t],
            )
            .await
            .expect("seed row");
        }
    }

    let snap = PgSnapshotter::new(&pg_url());

    // Scoped to org A: exactly A's two rows, every payload org-stamped A.
    let events = snap
        .snapshot(
            "tasks",
            Lsn::new(0),
            Some(TenantScope::new("org_id", &org_a)),
        )
        .await
        .expect("scoped snapshot A");
    let rows: Vec<(String, String)> = events.iter().map(pk_org).collect();
    assert_eq!(
        rows.len(),
        2,
        "scope A sees exactly its own seeded rows, never tenant B rows: {rows:?}",
    );
    assert!(
        rows.iter().all(|(_, org)| org == &org_a),
        "every scoped row belongs to tenant A: {rows:?}",
    );

    // Scoped to org B: symmetric.
    let events = snap
        .snapshot(
            "tasks",
            Lsn::new(0),
            Some(TenantScope::new("org_id", &org_b)),
        )
        .await
        .expect("scoped snapshot B");
    assert!(
        events.iter().map(pk_org).all(|(_, org)| org == org_b),
        "every scoped row belongs to tenant B",
    );

    // Unscoped (anonymous / single-tenant): legitimately unfiltered — the
    // seeded A and B rows all appear (the shared table may carry other
    // suites' rows too, hence contains-not-equals).
    let events = snap
        .snapshot("tasks", Lsn::new(0), None)
        .await
        .expect("unscoped snapshot");
    let orgs: Vec<String> = events.iter().map(pk_org).map(|(_, org)| org).collect();
    assert!(
        orgs.contains(&org_a) && orgs.contains(&org_b),
        "unscoped snapshot includes both tenants",
    );

    // Cleanup: leave the shared table as we found it.
    sql.execute(
        "DELETE FROM tasks WHERE org_id::text = ANY($1)",
        &[&vec![org_a, org_b]],
    )
    .await
    .expect("cleanup");
}
