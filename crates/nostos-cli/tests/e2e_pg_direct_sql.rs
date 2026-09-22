//! Real-Postgres e2e for the SQL `nostos link --mode direct` generates.
//!
//! Env-gated exactly like `e2e_pg_cli.rs` — skipped unless `NOSTOS_E2E_PG=1`:
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-cli --test e2e_pg_direct_sql -- --test-threads=1
//! ```
//!
//! ## What this is for
//!
//! `crates/nostos-cli/src/direct.rs` has unit tests, but they only prove the
//! generator emits the *text* we meant. This file proves Postgres accepts it
//! and that the protocol's one non-negotiable property actually holds:
//! **a transaction still in flight hides every later transaction from the
//! pull**, so no committed row can ever slip below an advanced horizon. That
//! is the property the whole design rests on and it cannot be tested in Rust.
//!
//! ## Why it owns the `cairn` schema
//!
//! The generated SQL hard-codes schema `cairn`, so this test drops and
//! recreates it — like the pg e2e tests that TRUNCATE `tasks`, it must run
//! with `--test-threads=1`. The *synced* tables are random-suffixed, so only
//! the `cairn` schema is exclusive. `auth`/`realtime` are stubbed with the
//! two functions the generated SQL calls; the test refuses to run against a
//! database that has a real `auth.users`, so pointing `NOSTOS_PG_URL` at a live
//! Supabase project aborts instead of dropping its auth schema.

use nostos_cli::direct::{render, DirectTable, Scoping, DEFAULT_RETENTION};

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

/// The two Supabase functions the generated SQL calls, plus the table and the
/// role it grants to. `auth.jwt()` reads the same GUC Supabase's does, so the
/// scope expressions are exercised for real rather than mocked away.
const SUPABASE_STUBS: &str = r"
create schema if not exists auth;
create schema if not exists realtime;
do $$ begin
  if not exists (select from pg_roles where rolname = 'authenticated') then
    create role authenticated;
  end if;
end $$;
grant authenticated to current_user;
create or replace function auth.jwt() returns jsonb language sql stable as $fn$
  select coalesce(nullif(current_setting('request.jwt.claims', true), ''), '{}')::jsonb;
$fn$;
create table if not exists realtime.messages (
  id bigserial primary key, topic text, extension text, payload jsonb
);
alter table realtime.messages enable row level security;
create or replace function realtime.topic() returns text language sql stable as $fn$
  select current_setting('realtime.topic', true);
$fn$;
create or replace function realtime.send(
  payload jsonb, event text, topic text, private boolean default true)
returns void language sql as $fn$
  insert into realtime.messages (topic, extension, payload) values (topic, 'broadcast', payload);
$fn$;
grant usage on schema realtime to authenticated;
grant select on realtime.messages to authenticated;
";

/// Refuse to touch a database that looks like a real Supabase project.
async fn assert_disposable(client: &tokio_postgres::Client) {
    let real_supabase: bool = client
        .query_one("select to_regclass('auth.users') is not null", &[])
        .await
        .expect("probing auth.users")
        .get(0);
    assert!(
        !real_supabase,
        "NOSTOS_PG_URL points at a database with a real `auth.users` — refusing to run: \
         this test drops the `nostos`, `auth` and `realtime` stub schemas"
    );
}

struct Fixture {
    client: tokio_postgres::Client,
    tasks: String,
}

impl Fixture {
    async fn setup() -> Self {
        let client = sql_client().await;
        assert_disposable(&client).await;
        let tasks = format!("nostos_direct_{}", uuid::Uuid::new_v4().simple());

        client
            .batch_execute("drop schema if exists cairn cascade;")
            .await
            .expect("clearing the cairn schema");
        client
            .batch_execute(SUPABASE_STUBS)
            .await
            .expect("installing the supabase stubs");
        client
            .batch_execute(&format!(
                "create table public.{tasks} (
                   id uuid primary key default gen_random_uuid(),
                   owner_id text not null,
                   title text,
                   hits bigint not null default 0
                 );
                 grant select, insert, update, delete on public.{tasks} to authenticated;"
            ))
            .await
            .expect("creating the synced table");

        let sql = render(
            &[DirectTable {
                table: tasks.clone(),
                scoping: Scoping::Claim {
                    column: "owner_id".to_string(),
                    claim: "sub".to_string(),
                },
            }],
            DEFAULT_RETENTION,
        );
        client
            .batch_execute(&sql)
            .await
            .expect("the generated SQL must apply cleanly");
        // Re-runnable: applying it twice is the documented contract.
        client
            .batch_execute(&sql)
            .await
            .expect("the generated SQL must apply twice");

        Self { client, tasks }
    }

    async fn insert(&self, owner: &str, title: &str) {
        self.client
            .execute(
                &format!(
                    "insert into public.{} (owner_id, title) values ($1, $2)",
                    self.tasks
                ),
                &[&owner, &title],
            )
            .await
            .expect("insert");
    }

    /// Everything the pull returns, as `(xid, table, pk, op)`.
    async fn pull(&self, since: &str, max_txns: i32) -> Vec<(String, String, String, String)> {
        self.client
            .query(
                "select xid::text, table_name, pk, op from public.cairn_pull($1::text::xid8, $2) \
                 order by xid, seq",
                &[&since, &max_txns],
            )
            .await
            .expect("cairn_pull")
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
            .collect()
    }

    async fn teardown(self) {
        let _ = self
            .client
            .batch_execute(&format!(
                "drop table if exists public.{} cascade;",
                self.tasks
            ))
            .await;
        let _ = self
            .client
            .batch_execute(
                "drop schema if exists cairn cascade; \
                 drop function if exists public.cairn_pull(xid8, int); \
                 drop function if exists public.cairn_increment(text, text, text, numeric);",
            )
            .await;
    }
}

/// **The property the whole protocol rests on.** A transaction that is still
/// in flight pins the horizon, so a *later* transaction that has already
/// committed must stay invisible until the older one finishes. Get this wrong
/// and the later rows are read once, the cursor advances past the older ones,
/// and they are never seen again — silent, permanent loss.
#[tokio::test]
async fn an_in_flight_transaction_hides_every_later_commit() {
    if std::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    fx.insert("alice", "settled").await;
    let settled = fx.pull("0", 200).await;
    assert_eq!(settled.len(), 1, "a committed row is pullable");
    let horizon_before = settled[0].0.clone();

    // A second connection opens a transaction, writes, and holds it open.
    let mut held = sql_client().await;
    let held_tx = held.transaction().await.expect("begin");
    held_tx
        .execute(
            &format!(
                "insert into public.{} (owner_id, title) values ('alice', 'in-flight')",
                fx.tasks
            ),
            &[],
        )
        .await
        .expect("in-flight insert");

    // Meanwhile a third write starts AND commits — with a higher xid.
    fx.insert("alice", "committed-after").await;

    let during = fx.pull(&horizon_before, 200).await;
    assert_eq!(
        during.len(),
        1,
        "the later commit must stay below the horizon while an older txn is in flight, \
         got: {during:?}"
    );

    held_tx.commit().await.expect("commit the held txn");
    let after = fx.pull(&horizon_before, 200).await;
    assert_eq!(
        after.len(),
        3,
        "once the held transaction commits, both rows appear: {after:?}"
    );
    fx.teardown().await;
}

/// The page is transactions, not rows: a page never splits one, and the cursor
/// resumes inclusively without re-reading the whole page forever.
#[tokio::test]
async fn the_page_is_whole_transactions_and_the_cursor_advances() {
    if std::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    // Four transactions, three rows each.
    for t in 0..4 {
        fx.client
            .batch_execute(&format!(
                "begin;
                 insert into public.{0} (owner_id, title) values ('alice', 'a{t}');
                 insert into public.{0} (owner_id, title) values ('alice', 'b{t}');
                 insert into public.{0} (owner_id, title) values ('alice', 'c{t}');
                 commit;",
                fx.tasks
            ))
            .await
            .expect("batched transaction");
    }

    let mut since = "0".to_string();
    let mut seen = 0;
    let mut pages = 0;
    loop {
        let page = fx.pull(&since, 2).await;
        if page.is_empty() {
            break;
        }
        pages += 1;
        assert!(pages <= 8, "paging must terminate, not livelock");
        let xids: Vec<&String> = page.iter().map(|r| &r.0).collect();
        let distinct: std::collections::BTreeSet<&&String> = xids.iter().collect();
        assert!(distinct.len() <= 2, "max_txns = 2 must cap distinct xids");
        for x in &distinct {
            assert_eq!(
                page.iter().filter(|r| &&r.0 == *x).count(),
                3,
                "every transaction arrives whole, never split: {page:?}"
            );
        }
        let last = page.last().expect("non-empty").0.clone();
        // Inclusive resume: the last transaction is re-read, so subtract it.
        seen += page.len() - page.iter().filter(|r| r.0 == last).count();
        if last == since {
            break;
        }
        since = last;
    }
    assert_eq!(seen + 3, 12, "all four transactions were read exactly once");
    fx.teardown().await;
}

/// RLS is the only thing authorizing a direct-mode read, and the scope column
/// is namespaced so two claims cannot collide.
#[tokio::test]
async fn rls_scopes_the_log_to_the_callers_claims() {
    if std::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    fx.insert("alice", "hers").await;
    fx.insert("bob", "his").await;

    let rows = fx
        .client
        .query(
            "select set_config('request.jwt.claims', '{\"sub\":\"alice\"}', false), \
                    set_config('role', 'authenticated', false)",
            &[],
        )
        .await;
    assert!(rows.is_ok(), "switching to the authenticated role");

    let scoped = fx
        .client
        .query(
            "select pk, op from public.cairn_pull('0'::text::xid8, 200)",
            &[],
        )
        .await
        .expect("pull as authenticated");
    assert_eq!(
        scoped.len(),
        1,
        "alice sees exactly her own row, not bob's: {} rows",
        scoped.len()
    );

    fx.client
        .batch_execute("reset role; select set_config('request.jwt.claims', '', false);")
        .await
        .expect("back to the owner");
    fx.teardown().await;
}

/// The op PostgREST cannot express, plus the scope-change rule: a row that
/// moves to another owner is logged as a delete under the old scope, so the
/// losing device is told to drop it.
#[tokio::test]
async fn increment_is_atomic_and_a_scope_change_emits_a_delete() {
    if std::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    fx.insert("alice", "counter").await;
    let id: String = fx
        .client
        .query_one(&format!("select id::text from public.{}", fx.tasks), &[])
        .await
        .expect("read back the id")
        .get(0);

    for _ in 0..5 {
        fx.client
            .execute(
                "select public.cairn_increment($1, $2, 'hits', 1)",
                &[&fx.tasks, &id],
            )
            .await
            .expect("cairn_increment");
    }
    let hits: i64 = fx
        .client
        .query_one(&format!("select hits from public.{}", fx.tasks), &[])
        .await
        .expect("read hits")
        .get(0);
    assert_eq!(hits, 5, "five increments land as five");

    let rejected = fx
        .client
        .execute(
            "select public.cairn_increment('pg_class', '1', 'hits', 1)",
            &[],
        )
        .await;
    assert!(rejected.is_err(), "an unsynced table is refused");

    fx.client
        .execute(
            &format!(
                "update public.{} set owner_id = 'bob' where id::text = $1",
                fx.tasks
            ),
            &[&id],
        )
        .await
        .expect("hand the row to bob");

    let scopes: Vec<(String, Option<String>)> = fx
        .client
        .query("select op, scope from cairn.changes order by seq", &[])
        .await
        .expect("read the log")
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        scopes.last().map(|(op, s)| (op.as_str(), s.as_deref())),
        Some(("update", Some("sub:bob"))),
        "the new owner gets the update: {scopes:?}"
    );
    assert!(
        scopes
            .iter()
            .any(|(op, s)| op == "delete" && s.as_deref() == Some("sub:alice")),
        "the old owner is told to drop it: {scopes:?}"
    );
    fx.teardown().await;
}
