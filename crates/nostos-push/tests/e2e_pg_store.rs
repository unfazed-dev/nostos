//! v1.1 (ADR-0038 §4 addendum) — real-Postgres registry e2e for `PgStore`.
//!
//! Pins the SAME five behaviors the SQLite unit tests in `src/store.rs`
//! pin, against a live Postgres: token round-trip + owner-scoped delete,
//! same-owner re-registration refresh, cross-tenant conflict until the
//! owner deletes (2026-08-17 audit finding 3), receipts ascending +
//! tenant-isolated, and the age-based sweep.
//!
//! Parallel-safe by construction (the `e2e_pg_token_store.rs` discipline):
//! each test owns disjoint token values / tenant ids and cleans exactly its
//! own rows before and after, and `PgStore::open`'s boot DDL is serialized
//! by its advisory lock, so concurrent opens cannot race on `pg_type`.
//!
//! ## Running
//!
//! Requires a live Postgres (the repo's `make pg-up` /
//! `docker/docker-compose.stack.yml`). Self-skips unless `NOSTOS_E2E_PG=1`
//! is set, so PG-less CI stays green — a skipped run reports a pass, which
//! is a FALSE positive, not a verified one:
//!
//! ```sh
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn \
//!   cargo test -p nostos-push --features pg --test e2e_pg_store -- --nocapture
//! ```

#![cfg(feature = "pg")]

use nostos_push::store::{
    now_rfc3339, DeleteOutcome, NewReceipt, Outcome, PgStore, Platform, Store, TokenRecord,
    UpsertOutcome,
};

/// Env gate (the nostos-infra pg e2e convention): the tests self-skip when
/// PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    nostos_infra::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgres://cairn:cairn@localhost:5433/cairn".into())
}

/// The self-skip gate: without `NOSTOS_E2E_PG=1` a test returns early with
/// a stderr note. A green-but-skipped run is NOT a verified pass.
fn gated() -> bool {
    if nostos_infra::env::var(E2E_FLAG).is_ok() {
        return true;
    }
    eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
    false
}

/// Open the store under test (connect + idempotent boot DDL).
async fn open() -> PgStore {
    PgStore::open(&pg_url()).await.expect("open PgStore (DDL)")
}

/// A control-plane SQL client for fixture cleanup (NOT the store under
/// test — the `e2e_pg_token_store.rs` setup idiom).
async fn sql() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Delete ONLY this test's token rows (parallel tests own disjoint tokens —
/// a full-table DELETE would wipe a sibling test's fixtures).
async fn clean_tokens(tokens: &[&str]) {
    let owned: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
    sql()
        .await
        .execute("DELETE FROM push_tokens WHERE token = ANY($1)", &[&owned])
        .await
        .expect("clean token rows");
}

/// Delete one tenant's receipts (fixture isolation for the receipt tests).
async fn clean_receipts(tenant: &str) {
    sql()
        .await
        .execute("DELETE FROM receipts WHERE tenant_id = $1", &[&tenant])
        .await
        .expect("clean receipt rows");
}

fn receipt(push_id: &str, tenant: &str, provider_ts: &str) -> NewReceipt {
    NewReceipt {
        tenant_id: tenant.to_string(),
        push_id: push_id.to_string(),
        token: "pgst-token".to_string(),
        outcome: Outcome::Delivered,
        detail: None,
        metadata: None,
        provider_ts: provider_ts.to_string(),
    }
}

/// Behavior 1 of 5 (the SQLite unit-test twin: `token_roundtrip_and_
/// owner_scoped_delete`): register → lookup round-trip, tenant-scoped
/// invisibility, and the Foreign/Deleted/Missing delete ladder.
#[tokio::test]
async fn token_roundtrip_and_owner_scoped_delete() {
    if !gated() {
        return;
    }
    let s = open().await;
    clean_tokens(&["pgst-rt-1"]).await;
    s.upsert_token("pgst-tenant-a", "pgst-rt-1", Platform::Fcm, Some("acct"))
        .await
        .expect("upsert");
    let rec = s
        .lookup_token("pgst-tenant-a", "pgst-rt-1")
        .await
        .expect("lookup");
    assert_eq!(
        rec,
        Some(TokenRecord {
            platform: Platform::Fcm,
            account_tag: Some("acct".to_string())
        })
    );
    // Tenant-scoped lookup: tenant B sees nothing of tenant A's row.
    assert_eq!(
        s.lookup_token("pgst-tenant-b", "pgst-rt-1").await.unwrap(),
        None
    );
    // Foreign delete is reported for the 204; own delete succeeds; the
    // second own delete is Missing (idempotent 204).
    assert_eq!(
        s.delete_token_owner_scoped("pgst-tenant-b", "pgst-rt-1")
            .await
            .unwrap(),
        DeleteOutcome::Foreign
    );
    assert_eq!(
        s.delete_token_owner_scoped("pgst-tenant-a", "pgst-rt-1")
            .await
            .unwrap(),
        DeleteOutcome::Deleted
    );
    assert_eq!(
        s.delete_token_owner_scoped("pgst-tenant-a", "pgst-rt-1")
            .await
            .unwrap(),
        DeleteOutcome::Missing
    );
    assert_eq!(
        s.lookup_token("pgst-tenant-a", "pgst-rt-1").await.unwrap(),
        None
    );
    clean_tokens(&["pgst-rt-1"]).await;
}

/// Behavior 2 of 5 (`upsert_re_registration_refreshes_platform`):
/// re-registering one's OWN row refreshes platform + account_tag, both
/// times `Registered`.
#[tokio::test]
async fn upsert_re_registration_refreshes_platform() {
    if !gated() {
        return;
    }
    let s = open().await;
    clean_tokens(&["pgst-reg-1"]).await;
    assert_eq!(
        s.upsert_token("pgst-tenant-a", "pgst-reg-1", Platform::Apns, None)
            .await
            .unwrap(),
        UpsertOutcome::Registered
    );
    assert_eq!(
        s.upsert_token(
            "pgst-tenant-a",
            "pgst-reg-1",
            Platform::Webpush,
            Some("tag")
        )
        .await
        .unwrap(),
        UpsertOutcome::Registered
    );
    let rec = s
        .lookup_token("pgst-tenant-a", "pgst-reg-1")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(rec.platform, Platform::Webpush);
    clean_tokens(&["pgst-reg-1"]).await;
}

/// Behavior 3 of 5 (audit finding 3, `cross_tenant_upsert_conflicts_until_
/// owner_deletes`): a cross-tenant re-register is a Conflict — ownership
/// and platform survive the refused attempt — until the owner deletes,
/// after which the new tenant registers cleanly (the documented migration
/// path).
#[tokio::test]
async fn cross_tenant_upsert_conflicts_until_owner_deletes() {
    if !gated() {
        return;
    }
    let s = open().await;
    clean_tokens(&["pgst-x-1"]).await;
    s.upsert_token("pgst-tenant-a", "pgst-x-1", Platform::Apns, None)
        .await
        .unwrap();
    assert_eq!(
        s.upsert_token("pgst-tenant-b", "pgst-x-1", Platform::Fcm, None)
            .await
            .unwrap(),
        UpsertOutcome::Conflict
    );
    // Ownership and platform survived the refused attempt.
    let rec = s.lookup_token("pgst-tenant-a", "pgst-x-1").await.unwrap();
    assert_eq!(rec.expect("row").platform, Platform::Apns);
    assert_eq!(
        s.lookup_token("pgst-tenant-b", "pgst-x-1").await.unwrap(),
        None
    );
    // Migration path: the owner deletes, then the new tenant registers.
    assert_eq!(
        s.delete_token_owner_scoped("pgst-tenant-a", "pgst-x-1")
            .await
            .unwrap(),
        DeleteOutcome::Deleted
    );
    assert_eq!(
        s.upsert_token("pgst-tenant-b", "pgst-x-1", Platform::Fcm, None)
            .await
            .unwrap(),
        UpsertOutcome::Registered
    );
    let rec = s.lookup_token("pgst-tenant-b", "pgst-x-1").await.unwrap();
    assert_eq!(rec.expect("row").platform, Platform::Fcm);
    clean_tokens(&["pgst-x-1"]).await;
}

/// Behavior 4 of 5 (`receipts_append_ascending_and_tenant_isolated`):
/// receipt seq is monotonic, the listing is ascending and tenant-isolated,
/// and the since-cursor pages correctly. provider_ts values are NOW-based
/// so a concurrently running sweep test can never reap them (ordering is
/// by seq, not provider_ts).
#[tokio::test]
async fn receipts_append_ascending_and_tenant_isolated() {
    const A: &str = "pgst-rec-a";
    const B: &str = "pgst-rec-b";
    if !gated() {
        return;
    }
    let s = open().await;
    clean_receipts(A).await;
    clean_receipts(B).await;
    let now = now_rfc3339();
    let s1 = s.append_receipt(&receipt("pg-p1", A, &now)).await.unwrap();
    let s2 = s.append_receipt(&receipt("pg-p2", B, &now)).await.unwrap();
    let s3 = s.append_receipt(&receipt("pg-p3", A, &now)).await.unwrap();
    assert!(s2 > s1 && s3 > s2, "monotonic seq");
    let a = s.list_receipts(A, 0, 100).await.unwrap();
    assert_eq!(a.len(), 2, "tenant B's receipt is invisible to tenant A");
    assert!(a[0].seq < a[1].seq, "ascending");
    // Cursor: everything after the first of A's receipts.
    let tail = s.list_receipts(A, a[0].seq, 100).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].push_id, "pg-p3");
    clean_receipts(A).await;
    clean_receipts(B).await;
}

/// Behavior 5 of 5 (`sweep_deletes_only_old_receipts`): the retention
/// sweep deletes ONLY receipts older than the window. The sweep is global
/// (as in SQLite), so the test first removes ancient garbage (< 2021)
/// left by any crashed earlier run — nothing else inserts pre-2021 rows,
/// which makes the swept count exactly 1.
#[tokio::test]
async fn sweep_deletes_only_old_receipts() {
    const T: &str = "pgst-sweep";
    if !gated() {
        return;
    }
    let s = open().await;
    sql()
        .await
        .execute(
            "DELETE FROM receipts WHERE provider_ts < '2021-01-01T00:00:00.000000000Z'",
            &[],
        )
        .await
        .expect("pre-sweep ancient garbage");
    clean_receipts(T).await;
    s.append_receipt(&receipt("pg-old", T, "2020-01-01T00:00:00.000000000Z"))
        .await
        .unwrap();
    s.append_receipt(&receipt("pg-new", T, &now_rfc3339()))
        .await
        .unwrap();
    let swept = s.sweep_receipts(60).await.unwrap();
    assert_eq!(swept, 1);
    let left = s.list_receipts(T, 0, 100).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].push_id, "pg-new");
    clean_receipts(T).await;
}
