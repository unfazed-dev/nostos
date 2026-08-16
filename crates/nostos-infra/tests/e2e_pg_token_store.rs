//! ADR-0037 §3 — real-Postgres push-token registry e2e.
//!
//! Direct adapter tests for `PgTokenStore` (no server/replication machinery,
//! mirroring the `PgWriteBack` increment/OR-set tests in
//! `e2e_pg_writeback.rs`): upsert / prune / list-by-account round-trip, the
//! ADR's identity semantics (a token re-registered under a different account
//! MIGRATES — the previous principal must resolve to zero devices), tenant
//! isolation of the account lookup, and the cross-tenant conflict gate (a
//! re-register from another tenant NEVER migrates the row).
//!
//! Parallel-safe by construction: the DDL runs once per process (three
//! concurrent `CREATE TABLE IF NOT EXISTS` race on `pg_type`), and each test
//! touches only its own token rows — no full-table wipes.
//!
//! ## Running
//!
//! Requires a live Postgres (the repo's `make pg-up`). Skipped unless
//! `NOSTOS_E2E_PG=1` is set, so it never breaks PG-less CI:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn \
//!   cargo test -p nostos-infra --features pg --test e2e_pg_token_store -- --nocapture
//! ```

#![cfg(feature = "pg")]

use nostos_infra::PgTokenStore;

/// Env gate. The test self-skips when PG isn't available so unit-test CI stays green.
const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

/// Connect a control-plane SQL client for setup/teardown.
async fn sql_client() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls)
        .await
        .expect("connect to PG");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// Table DDL, run exactly once per test process. Same idempotent DDL as
/// `docker/pg-init/01-sources.sql` / `supabase/schema.sql` (a no-op on an
/// already-migrated DB, and makes the test self-sufficient on a container
/// initialized before the table was added to pg-init). `OnceCell` — not a
/// plain call in each test — because concurrent `CREATE TABLE IF NOT EXISTS`
/// for the same name races on the `pg_type` unique index.
async fn ensure_table() {
    static TABLE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    TABLE
        .get_or_init(|| async {
            let sql = sql_client().await;
            sql.batch_execute(
                "CREATE TABLE IF NOT EXISTS cairn_push_tokens ( \
                     token      TEXT        PRIMARY KEY, \
                     platform   TEXT        NOT NULL, \
                     account_id TEXT        NOT NULL, \
                     tenant_id  TEXT        NOT NULL, \
                     updated_at TIMESTAMPTZ NOT NULL DEFAULT now() \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_cairn_push_tokens_account \
                     ON cairn_push_tokens (account_id, tenant_id);",
            )
            .await
            .expect("create cairn_push_tokens");
        })
        .await;
}

/// Delete ONLY this test's token rows (parallel tests own disjoint tokens —
/// a full-table DELETE here would wipe a sibling test's fixtures).
async fn clean_tokens(tokens: &[&str]) {
    let sql = sql_client().await;
    let owned: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
    sql.execute(
        "DELETE FROM cairn_push_tokens WHERE token = ANY($1)",
        &[&owned],
    )
    .await
    .expect("clean token rows");
}

/// Upsert → list-by-account → prune round-trip, including multi-device
/// fan-out shape (two tokens, two platforms, one account) and prune
/// idempotence (pruning an absent token is 0 rows, not an error).
#[tokio::test]
async fn upsert_list_prune_roundtrip() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["rt-iphone", "rt-android"]).await;
    let store = PgTokenStore::new(&pg_url());

    // Two devices on one account (the offline-account push fan-out shape).
    store
        .upsert("apns", "rt-iphone", "rt-acct-1", "rt-tenant")
        .await
        .expect("upsert apns");
    store
        .upsert("fcm", "rt-android", "rt-acct-1", "rt-tenant")
        .await
        .expect("upsert fcm");

    let mut devices = store
        .list_by_account("rt-tenant", "rt-acct-1")
        .await
        .expect("list");
    devices.sort_by(|a, b| a.token.cmp(&b.token));
    assert_eq!(
        devices,
        vec![
            nostos_infra::PushToken {
                platform: "fcm".into(),
                token: "rt-android".into()
            },
            nostos_infra::PushToken {
                platform: "apns".into(),
                token: "rt-iphone".into()
            },
        ],
        "both devices must resolve for the offline account"
    );

    // Prune (the APNs-410 / FCM-UNREGISTERED path): one row gone, the other
    // survives, and pruning again is idempotent.
    assert_eq!(store.prune("rt-iphone").await.expect("prune"), 1);
    let remaining = store
        .list_by_account("rt-tenant", "rt-acct-1")
        .await
        .expect("list after prune");
    assert_eq!(remaining.len(), 1, "only the pruned device disappears");
    assert_eq!(store.prune("rt-iphone").await.expect("re-prune"), 0);

    // A different account resolves to nothing (baseline isolation).
    assert!(store
        .list_by_account("rt-tenant", "rt-acct-2")
        .await
        .expect("list other account")
        .is_empty());

    clean_tokens(&["rt-iphone", "rt-android"]).await;
}

/// Token-rotation hygiene (manual-run postmortem: three live FCM rows, one
/// per reinstall): an install rotation cannot know its predecessor's token
/// and FCM keeps accepting sends to it, so the rail-level `UNREGISTERED`
/// prune never fires. `upsert` must sweep same-account tokens not
/// re-registered within the TTL — while fresh multi-device rows survive.
#[tokio::test]
async fn upsert_sweeps_stale_sibling_tokens() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["ttl-stale", "ttl-live", "ttl-new"]).await;
    let store = PgTokenStore::new(&pg_url());

    // An old install's token, plus a second active device on the account.
    store
        .upsert("fcm", "ttl-stale", "ttl-acct", "ttl-tenant")
        .await
        .expect("stale registration");
    store
        .upsert("fcm", "ttl-live", "ttl-acct", "ttl-tenant")
        .await
        .expect("live sibling registration");

    // Age ONLY the stale one past the 30-day TTL.
    sql_client()
        .await
        .execute(
            "UPDATE cairn_push_tokens SET updated_at = now() - interval '31 days' \
             WHERE token = 'ttl-stale'",
            &[],
        )
        .await
        .expect("backdate stale token");

    // A fresh install registers its new token — the sweep rides along.
    store
        .upsert("fcm", "ttl-new", "ttl-acct", "ttl-tenant")
        .await
        .expect("new-install registration");

    let mut devices = store
        .list_by_account("ttl-tenant", "ttl-acct")
        .await
        .expect("list after sweep");
    devices.sort_by(|a, b| a.token.cmp(&b.token));
    assert_eq!(
        devices,
        vec![
            nostos_infra::PushToken {
                platform: "fcm".into(),
                token: "ttl-live".into()
            },
            nostos_infra::PushToken {
                platform: "fcm".into(),
                token: "ttl-new".into()
            },
        ],
        "the stale install's token is swept; fresh devices survive"
    );

    clean_tokens(&["ttl-stale", "ttl-live", "ttl-new"]).await;
}

/// ADR-0037's identity semantics: re-registering a token under a different
/// account MIGRATES the row — after a device changes hands, the previous
/// principal's lookup must return zero devices (no pushing the previous
/// user's data to the next user).
#[tokio::test]
async fn re_registration_migrates_token_to_new_account() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["mig-shared"]).await;
    let store = PgTokenStore::new(&pg_url());

    store
        .upsert("apns", "mig-shared", "mig-acct-prev", "mig-tenant")
        .await
        .expect("first registration");
    store
        .upsert("apns", "mig-shared", "mig-acct-next", "mig-tenant")
        .await
        .expect("re-registration under a new account");

    assert!(
        store
            .list_by_account("mig-tenant", "mig-acct-prev")
            .await
            .expect("previous principal lookup")
            .is_empty(),
        "the previous principal must resolve to ZERO devices after migration"
    );
    let next = store
        .list_by_account("mig-tenant", "mig-acct-next")
        .await
        .expect("next principal lookup");
    assert_eq!(
        next.len(),
        1,
        "exactly one row exists, owned by the new account"
    );
    assert_eq!(next[0].token, "mig-shared");

    clean_tokens(&["mig-shared"]).await;
}

/// M2: a cross-tenant re-registration NEVER migrates the row — the conflict
/// update is gated on the existing row's tenant. Both shapes: another
/// tenant's account re-registering the token, and the SAME account id under
/// another tenant (tenant mismatch ⇒ keep either way). Contrast: a
/// same-tenant re-register still migrates.
#[tokio::test]
async fn cross_tenant_re_registration_keeps_the_existing_row() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["x-tenant-tok"]).await;
    let store = PgTokenStore::new(&pg_url());

    store
        .upsert("apns", "x-tenant-tok", "x-acct-1", "x-tenant-a")
        .await
        .expect("tenant-a registration");

    // Another tenant's account re-registers the same token: a silent no-op.
    store
        .upsert("fcm", "x-tenant-tok", "x-acct-2", "x-tenant-b")
        .await
        .expect("cross-tenant re-register must not error");
    let rows = store
        .list_by_tenant("x-tenant-a")
        .await
        .expect("tenant-a lookup");
    assert_eq!(rows.len(), 1, "the row must stay in tenant-a");
    assert_eq!(rows[0].account_id, "x-acct-1");
    assert_eq!(
        rows[0].platform, "apns",
        "the losing registration must not even touch platform"
    );
    assert!(
        store.list_by_tenant("x-tenant-b").await.unwrap().is_empty(),
        "tenant-b must gain nothing"
    );

    // Same account id, other tenant: same rule (tenant mismatch ⇒ keep).
    store
        .upsert("fcm", "x-tenant-tok", "x-acct-1", "x-tenant-b")
        .await
        .expect("same-account cross-tenant re-register");
    let rows = store.list_by_tenant("x-tenant-a").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tenant_id, "x-tenant-a");

    // Contrast: a SAME-tenant re-register still migrates the row.
    store
        .upsert("fcm", "x-tenant-tok", "x-acct-3", "x-tenant-a")
        .await
        .expect("same-tenant re-register migrates");
    assert!(
        store
            .list_by_account("x-tenant-a", "x-acct-1")
            .await
            .unwrap()
            .is_empty(),
        "same-tenant migration still applies"
    );
    assert_eq!(
        store
            .list_by_account("x-tenant-a", "x-acct-3")
            .await
            .unwrap()
            .len(),
        1
    );

    clean_tokens(&["x-tenant-tok"]).await;
}

/// The account lookup is tenant-isolated (ADR-0018): the same account id
/// under two tenants must only ever resolve its own tenant's devices.
#[tokio::test]
async fn list_by_account_is_tenant_isolated() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["iso-tenant-a", "iso-tenant-b"]).await;
    let store = PgTokenStore::new(&pg_url());

    store
        .upsert("apns", "iso-tenant-a", "iso-acct-same", "iso-tenant-a")
        .await
        .expect("tenant-a registration");
    store
        .upsert("fcm", "iso-tenant-b", "iso-acct-same", "iso-tenant-b")
        .await
        .expect("tenant-b registration");

    let a = store
        .list_by_account("iso-tenant-a", "iso-acct-same")
        .await
        .expect("tenant-a lookup");
    assert_eq!(a.len(), 1, "tenant-a must see only its own device");
    assert_eq!(a[0].token, "iso-tenant-a");
    let b = store
        .list_by_account("iso-tenant-b", "iso-acct-same")
        .await
        .expect("tenant-b lookup");
    assert_eq!(b.len(), 1, "tenant-b must see only its own device");
    assert_eq!(b[0].token, "iso-tenant-b");

    clean_tokens(&["iso-tenant-a", "iso-tenant-b"]).await;
}

/// `list_by_tenant` (ADR-0037 §1 amendment): the tenant-wide expansion
/// lookup — returns every account's tokens within ONE tenant (grouped by
/// account for the coalescer's presence filter), never another tenant's.
#[tokio::test]
async fn list_by_tenant_groups_accounts_and_isolates_tenants() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    ensure_table().await;
    clean_tokens(&["lt-a1", "lt-a2", "lt-b1"]).await;
    let store = PgTokenStore::new(&pg_url());

    // Two accounts (multi-device) in tenant A; one in tenant B.
    store
        .upsert("apns", "lt-a1", "lt-acct-1", "lt-tenant-a")
        .await
        .expect("a1");
    store
        .upsert("fcm", "lt-a2", "lt-acct-1", "lt-tenant-a")
        .await
        .expect("a2");
    store
        .upsert("apns", "lt-a1", "lt-acct-2", "lt-tenant-a")
        .await
        .expect("a1 re-registered under acct-2 (migrated)");
    store
        .upsert("apns", "lt-b1", "lt-acct-1", "lt-tenant-b")
        .await
        .expect("b1 (same account id, other tenant)");

    let mut rows = store
        .list_by_tenant("lt-tenant-a")
        .await
        .expect("list by tenant");
    rows.sort_by(|a, b| a.token.cmp(&b.token));
    assert_eq!(
        rows,
        vec![
            nostos_infra::RegisteredToken {
                tenant_id: "lt-tenant-a".into(),
                account_id: "lt-acct-2".into(),
                platform: "apns".into(),
                token: "lt-a1".into(),
            },
            nostos_infra::RegisteredToken {
                tenant_id: "lt-tenant-a".into(),
                account_id: "lt-acct-1".into(),
                platform: "fcm".into(),
                token: "lt-a2".into(),
            },
        ],
        "tenant A resolves its own accounts' devices — including the migrated \
         token — and never tenant B's row for the same account id"
    );

    // Owner-scoped delete (plan 3.1): removing acct-1's token leaves
    // acct-2's row alone even though a bare prune would hit it.
    assert_eq!(
        store
            .delete_for_owner("lt-tenant-a", "lt-acct-1", "lt-a2")
            .await
            .expect("scoped delete"),
        1
    );
    assert_eq!(store.list_by_tenant("lt-tenant-a").await.unwrap().len(), 1);
    // A different owner's delete is a no-op.
    assert_eq!(
        store
            .delete_for_owner("lt-tenant-a", "lt-acct-9", "lt-a1")
            .await
            .expect("scoped delete no-op"),
        0
    );

    clean_tokens(&["lt-a1", "lt-a2", "lt-b1"]).await;
}
