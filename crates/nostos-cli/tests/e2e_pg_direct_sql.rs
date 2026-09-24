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

use nostos_cli::direct::{
    inspect, parse_visible, render_with_push, templates_sql, DirectTable, PushConfig, Scoping,
    Verdict, DEFAULT_RETENTION,
};

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
  -- `anon` too: the generated revokes name the Data API roles rather than
  -- just PUBLIC, because Supabase's default privileges grant EXECUTE to them
  -- BY NAME and a revoke from the PUBLIC pseudo-role leaves that standing.
  -- The file has always required `authenticated`; this is the same class of
  -- requirement, and `nostos link --mode direct` already refuses any backend
  -- but Supabase.
  if not exists (select from pg_roles where rolname = 'anon') then
    create role anon;
  end if;
  if not exists (select from pg_roles where rolname = 'service_role') then
    create role service_role;
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
-- pg_net stands in as a recorder: the wake trigger only has to prove it fires
-- once per scope per cooldown, not that HTTP works.
create schema if not exists net;
create table if not exists net.sent (id bigserial primary key, url text, body jsonb);
create or replace function net.http_post(url text, body jsonb default '{}'::jsonb,
  params jsonb default '{}'::jsonb, headers jsonb default '{}'::jsonb, timeout_milliseconds int default 5000)
returns bigint language sql as $fn$
  insert into net.sent (url, body) values (url, body) returning id;
$fn$;
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

        let sql = render_with_push(
            &[DirectTable {
                table: tasks.clone(),
                scoping: Scoping::Claim {
                    column: "owner_id".to_string(),
                    claim: "sub".to_string(),
                },
            }],
            DEFAULT_RETENTION,
            Some(&PushConfig {
                endpoint: "https://example.test/nostos-push".to_string(),
                presence_window: "90 seconds".to_string(),
                cooldown: "30 seconds".to_string(),
                templates: Vec::new(),
            }),
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
        // The shared secret is set out of band so it never lands in a file —
        // `nostos doctor --mode direct` fails while it is unset, on purpose.
        client
            .batch_execute("update cairn.push_config set secret = 'test-secret' where id = 1;")
            .await
            .expect("set the push secret");

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
    ///
    /// The RPC hands back ONE jsonb array rather than a set of rows — see the
    /// comment over `cairn_pull` in the generator for why (PostgREST truncates
    /// a set at `db-max-rows` and says so only in a header). Over the wire the
    /// device reads that array directly; here we expand it so the assertions
    /// below can stay written in rows.
    async fn pull(&self, since: &str, max_txns: i32) -> Vec<(String, String, String, String)> {
        self.client
            .query(
                "select t.xid, t.table_name, t.pk, t.op \
                 from jsonb_array_elements(public.cairn_pull($1::text::xid8, $2)) \
                      with ordinality as a(e, ord), \
                 lateral jsonb_to_record(a.e) as t(xid text, table_name text, \
                                                   pk text, op text) \
                 order by a.ord",
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
                "drop schema if exists net cascade; \
                 drop schema if exists cairn cascade; \
                 drop function if exists public.cairn_pull(xid8, int); \
                 drop function if exists public.cairn_increment(text, text, text, numeric); \
                 drop function if exists public.cairn_register_push_token(text, text); \
                 drop function if exists public.cairn_deregister_push_token(text); \
                 drop function if exists public.cairn_heartbeat(text);",
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
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
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
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
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
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
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
            "select t.pk, t.op \
             from jsonb_array_elements(public.cairn_pull('0'::text::xid8, 200)) e, \
             lateral jsonb_to_record(e) as t(pk text, op text)",
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
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
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

/// `nostos doctor --mode direct` has to pass a fresh deploy, and — the reason
/// the check exists at all — catch a `cairn_pull` that pages by ROWS. That
/// deployment still returns rows and still advances a horizon, so it looks
/// healthy from the device while handing out half a transaction. Nothing on
/// the client can see it.
#[tokio::test]
async fn doctor_passes_a_fresh_deploy_and_catches_a_row_limited_pull() {
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    let tables = [DirectTable {
        table: fx.tasks.clone(),
        scoping: Scoping::Claim {
            column: "owner_id".to_string(),
            claim: "sub".to_string(),
        },
    }];

    let checks = inspect(&fx.client, &tables).await.expect("inspect");
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| c.verdict == Verdict::Fail)
        .map(|c| c.label.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "a fresh deploy must be clean: {failed:?}"
    );
    assert!(
        checks
            .iter()
            .any(|c| c.label.contains("Allow public access")),
        "the one thing SQL cannot see must still be reported"
    );

    // Now deploy the bug: same signature, same output columns, row-limited.
    // Dropped first because the shipped one returns a scalar jsonb and a
    // return type cannot be replaced in place.
    fx.client
        .batch_execute(
            r#"drop function if exists public.cairn_pull(xid8, int);
               create or replace function public.cairn_pull(since xid8, max_txns int default 200)
               returns table (horizon xid8, seq bigint, xid xid8,
                              table_name text, pk text, op text, "row" jsonb)
               language sql stable security invoker set search_path = '' as $fn$
                 with h as (select pg_snapshot_xmin(pg_current_snapshot()) as horizon)
                 select h.horizon, c.seq, c.xid, c.table_name, c.pk, c.op, c."row"
                 from cairn.changes c cross join h
                 where c.xid >= since and c.xid < h.horizon
                 order by c.xid, c.seq
                 limit max_txns;
               $fn$;"#,
        )
        .await
        .expect("deploy the row-limited pull");

    let checks = inspect(&fx.client, &tables).await.expect("inspect");
    let caught = checks
        .iter()
        .find(|c| c.label.contains("page by transaction"))
        .expect("the paging check must be present");
    assert_eq!(
        caught.verdict,
        Verdict::Fail,
        "a row-limited pull must be caught: {}",
        caught.label
    );
    fx.teardown().await;
}

/// Offline past the retention window. Pruning the log leaves a hole that no
/// later pull can fill, so `cairn_pull` must refuse a horizon below it rather
/// than return a short answer — a short answer is indistinguishable from
/// "nothing happened", and the rows in the hole would never arrive.
#[tokio::test]
async fn a_horizon_below_the_pruned_window_is_refused_with_pt410() {
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    fx.insert("alice", "old").await;
    let old_horizon = fx.pull("0", 200).await[0].0.clone();

    // Age the log past the window and prune it.
    fx.client
        .batch_execute("update cairn.changes set logged_at = now() - interval '30 days';")
        .await
        .expect("age the log");
    let pruned: i64 = fx
        .client
        .query_one("select cairn.prune()", &[])
        .await
        .expect("prune")
        .get(0);
    assert_eq!(pruned, 1, "one row was old enough");

    // A fresh device is fine; a device resuming into the hole is not.
    assert!(
        fx.pull("0", 200).await.is_empty(),
        "a fresh horizon still works after a prune"
    );
    let err = fx
        .client
        .query(
            "select * from public.cairn_pull($1::text::xid8, 200)",
            &[&old_horizon],
        )
        .await
        .expect_err("resuming into a pruned gap must raise");
    let code = err.code().map(|c| c.code().to_string()).unwrap_or_default();
    assert_eq!(
        code, "PT410",
        "PostgREST turns PT410 into HTTP 410, which the client maps to \
         PostgrestError::Gone; got {code}: {err}"
    );
    fx.teardown().await;
}

/// The way back from a PT410. A refusal the device cannot act on would be a
/// device bricked by a long holiday, so the snapshot is part of the retention
/// design: current rows of every synced table plus a horizon, from ONE
/// statement, scoped by the same RLS that scopes a pull.
#[tokio::test]
async fn a_pruned_device_can_re_snapshot_and_resume() {
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    fx.insert("alice", "kept").await;
    fx.insert("bob", "theirs").await;

    // ONE jsonb value, not a set — that is the whole point of the signature
    // (`db-max-rows` cannot truncate a scalar), so read it as one.
    let snapshot: serde_json::Value = fx
        .client
        .query_one("select public.cairn_snapshot()", &[])
        .await
        .expect("snapshot")
        .get(0);
    let rows = snapshot.as_array().expect("the snapshot is a jsonb array");
    assert!(!rows.is_empty(), "a snapshot always carries its horizon");

    let horizons: std::collections::HashSet<&str> =
        rows.iter().filter_map(|r| r["horizon"].as_str()).collect();
    assert_eq!(
        horizons.len(),
        1,
        "every row must come from ONE snapshot or the tables disagree"
    );

    // The header row per table is what makes an empty table distinguishable
    // from a table the snapshot forgot.
    let headers: Vec<&str> = rows
        .iter()
        .filter(|r| r["pk"].is_null())
        .filter_map(|r| r["table_name"].as_str())
        .collect();
    assert!(
        headers.contains(&fx.tasks.as_str()),
        "the snapshot must announce {}, got {headers:?}",
        fx.tasks
    );

    let bodies = rows.iter().filter(|r| !r["pk"].is_null()).count();
    assert_eq!(bodies, 2, "both rows are in the picture (no RLS role set)");

    // And the horizon it hands back is a horizon a pull will accept.
    let horizon = horizons
        .into_iter()
        .next()
        .expect("one horizon")
        .to_string();
    fx.client
        .query(
            "select * from public.cairn_pull($1::text::xid8, 200)",
            &[&horizon],
        )
        .await
        .expect("the snapshot's horizon must be a valid resume point");
    fx.teardown().await;
}

/// How many requests the `pg_net` recorder stub has seen.
async fn sent(client: &tokio_postgres::Client) -> i64 {
    client
        .query_one("select count(*)::bigint from net.sent", &[])
        .await
        .expect("count")
        .get(0)
}

/// Push only fires for a scope nobody is listening on, and at most once per
/// cooldown. The second half is not politeness: `pg_net` sends one HTTP
/// request per call, so without the per-scope debounce a bulk write would
/// become one request per row.
#[tokio::test]
async fn push_skips_awake_devices_and_debounces_the_rest() {
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;

    // A signed-in device registers a token. The scope comes from the JWT, so
    // the device cannot name someone else's.
    fx.client
        .batch_execute("select set_config('request.jwt.claims', '{\"sub\":\"alice\"}', false);")
        .await
        .expect("claims");
    fx.client
        .execute(
            "select public.cairn_register_push_token('fcm', 'tok-alice')",
            &[],
        )
        .await
        .expect("register");
    let scope: String = fx
        .client
        .query_one("select scope from cairn.push_tokens", &[])
        .await
        .expect("read the token")
        .get(0);
    assert_eq!(scope, "sub:alice", "the scope is stamped from the JWT");

    // Awake: the Realtime ring already reached them, so no push.
    fx.client
        .execute("select public.cairn_heartbeat('device-1')", &[])
        .await
        .expect("heartbeat");
    fx.insert("alice", "while awake").await;
    assert_eq!(sent(&fx.client).await, 0, "an awake device is not pushed");

    // Asleep: the first write wakes them, the next four are debounced.
    fx.client
        .batch_execute(
            "update cairn.device_presence set last_seen = now() - interval '10 minutes';",
        )
        .await
        .expect("age the presence row");
    for i in 0..5 {
        fx.insert("alice", &format!("while asleep {i}")).await;
    }
    assert_eq!(
        sent(&fx.client).await,
        1,
        "five writes to one sleeping scope are one push, not five"
    );

    let body: serde_json::Value = fx
        .client
        .query_one("select body from net.sent", &[])
        .await
        .expect("read the request")
        .get(0);
    assert_eq!(
        body,
        serde_json::json!({ "scope": "sub:alice" }),
        "the doorbell carries no data \u{2014} the device pulls, and RLS decides"
    );

    fx.client
        .batch_execute("select set_config('request.jwt.claims', '', false);")
        .await
        .expect("reset");
    fx.teardown().await;
}

/// A table with a visible template pushes EVERY change, not one per cooldown:
/// iOS shows an alert to a user-quit app but never wakes it for a silent
/// doorbell, so for these tables the push is the only news the user gets, and
/// a debounced one is a lost one (atlet, 2026-09-23).
#[tokio::test]
async fn a_visible_template_pushes_every_change_with_its_row() {
    if nostos_infra::env::var(E2E_FLAG).ok().as_deref() != Some("1") {
        eprintln!("skipping: set {E2E_FLAG}=1");
        return;
    }
    let fx = Fixture::setup().await;
    // Exactly what `nostos link --visible` writes, so its quoting meets a real
    // Postgres here.
    let spec = format!(
        "{}:action@/tasks/{{id}}:task_status:Task update:Now: {{title}}",
        fx.tasks
    );
    fx.client
        .batch_execute(&templates_sql(&[parse_visible(&spec).expect("spec")]))
        .await
        .expect("template");

    for i in 0..3 {
        fx.insert("alice", &format!("visible {i}")).await;
    }
    assert_eq!(sent(&fx.client).await, 3, "no debounce on a visible table");

    let body: serde_json::Value = fx
        .client
        .query_one("select body from net.sent order by id limit 1", &[])
        .await
        .expect("read the request")
        .get(0);
    assert_eq!(body["scope"], "sub:alice");
    assert_eq!(body["title"], "Task update");
    assert_eq!(body["body"], "Now: {title}", "the Edge Function fills it");
    assert_eq!(body["category"], "task_status");
    assert_eq!(body["route"], "/tasks/{id}");
    assert_eq!(body["row"]["title"], "visible 0");
    fx.teardown().await;
}
