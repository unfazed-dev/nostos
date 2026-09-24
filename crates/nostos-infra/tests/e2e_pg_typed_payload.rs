//! Typed payload mapping end-to-end (ADR-0019): `PgReplicator`'s OID-keyed
//! JSON mapping must actually fire against real Postgres wire/COPY text —
//! not just the hand-fed unit tests in `typed.rs` — and the snapshot path
//! and the streaming path must render the SAME row content byte-identically.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_typed_payload -- --nocapture --test-threads=1
//! ```
//!
//! Gate convention matches the rest of the pg e2e suite: `NOSTOS_E2E_PG=1`
//! self-skips when Postgres isn't available.
//!
//! ## Fixture
//!
//! A dedicated `typed_probe` table + a throwaway publication scoped to just
//! that table (created idempotently at test start — NOT added to the shared
//! `nostos_pub`, so this test can't perturb other suites' event streams).
//! Covers every OID `typed.rs` special-cases: bool, int2/int4/int8, numeric,
//! float8 (including `NaN`), timestamptz (with a non-UTC input offset — PG's
//! `+05:30` normalizes to `+00` text since the session `TimeZone` is UTC,
//! exercising the offset-parse path even though the visible offset ends up
//! zero), uuid, bytea, jsonb, and an explicit NULL column.

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use nostos_application::ports::ReplicatorStream;
use nostos_domain::ReplicationEvent;
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use serde_json::Value;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const PUBLICATION: &str = "nostos_pub_typed_f5";

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

async fn drop_slot(sql: &tokio_postgres::Client, slot: &str) {
    let _ = sql
        .batch_execute(&format!("SELECT pg_drop_replication_slot('{slot}');"))
        .await;
}

/// Idempotently create the fixture table + its own publication. A dedicated
/// publication (rather than reusing `nostos_pub`, which is scoped to `tasks`)
/// keeps this test's event stream isolated.
///
/// The TRUNCATE makes the fixture RE-ENTRANT (2026-08-17): rows accumulated
/// across historical runs (22 found) eventually exceeded the tests' 8-event
/// collection window, so a fresh-slot snapshot paged only stale rows and the
/// run's own row never appeared. Safe under the file's documented
/// `--test-threads=1` convention (same as e2e_pg_snapshot.rs on `tasks`).
async fn ensure_typed_probe(sql: &tokio_postgres::Client) {
    // CREATE must precede TRUNCATE: a freshly seeded database (e.g. a wiped
    // Docker volume) has no typed_probe yet — truncate-first fails with
    // 42P01 (caught 2026-08-17 on a recreated nostos-postgres volume).
    sql.batch_execute(
        "CREATE TABLE IF NOT EXISTS typed_probe ( \
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
            flag BOOLEAN, \
            small INT2, \
            medium INT4, \
            big INT8, \
            amount NUMERIC, \
            ratio FLOAT8, \
            ts TIMESTAMPTZ, \
            uid UUID, \
            blob BYTEA, \
            payload JSONB, \
            note TEXT \
        );",
    )
    .await
    .expect("create typed_probe");
    sql.batch_execute("TRUNCATE TABLE typed_probe;")
        .await
        .expect("truncate typed_probe (re-entrant fixture)");
    sql.batch_execute(&format!(
        "DO $$ BEGIN \
         IF NOT EXISTS (SELECT 1 FROM pg_publication WHERE pubname = '{PUBLICATION}') THEN \
         CREATE PUBLICATION {PUBLICATION} FOR TABLE typed_probe; \
         END IF; \
         END $$;"
    ))
    .await
    .expect("create typed_probe publication");
}

/// Insert one row covering every mapped OID, using literal SQL (not bound
/// parameters — `tokio_postgres`'s `ToSql` doesn't cover `NUMERIC`/`bytea`
/// hex literals without extra crate features, and every value here is a
/// hardcoded test constant, so a literal statement is simpler and injection
/// is not a concern). `note` is left NULL to exercise the NULL path.
async fn insert_probe_row(sql: &tokio_postgres::Client, id: uuid::Uuid, uid: uuid::Uuid) {
    let stmt = format!(
        r#"INSERT INTO typed_probe (id, flag, small, medium, big, amount, ratio, ts, uid, blob, payload, note)
           VALUES ('{id}', true, 7, -42, 9223372036854775807, 3.14159265358979323846, 'NaN',
                   '2026-07-12 23:30:00+05:30', '{uid}', '\x6869'::bytea,
                   '{{"a": 1, "b": [1, 2, 3]}}'::jsonb, NULL)"#
    );
    sql.batch_execute(&stmt).await.expect("insert probe row");
}

async fn collect_events(
    repl: &mut PgReplicator,
    max: usize,
    overall: Duration,
) -> Vec<ReplicationEvent> {
    let mut out = Vec::with_capacity(max);
    let deadline = tokio::time::Instant::now() + overall;
    while out.len() < max && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(400), repl.next_event()).await {
            Ok(Some(ev)) => out.push(ev),
            Ok(None) => break,
            Err(_) => {}
        }
    }
    out
}

/// Find the event whose payload's `id` field equals `id`, parse the payload
/// as JSON.
fn find_payload(events: &[ReplicationEvent], id: uuid::Uuid) -> Value {
    let id = id.to_string();
    events
        .iter()
        .find_map(|ev| {
            let v: Value = serde_json::from_slice(ev.payload_bytes()).ok()?;
            (v.get("id")?.as_str()? == id).then_some(v)
        })
        .unwrap_or_else(|| panic!("no event with id {id} found among {} events", events.len()))
}

/// Assert every OID-mapped field renders with the expected JSON type/value.
/// `uid` is the random uuid value inserted for the `uid` column (compared
/// separately since it's not a fixed constant).
fn assert_typed_shape(payload: &Value, uid: uuid::Uuid) {
    assert_eq!(
        payload["flag"],
        Value::Bool(true),
        "bool -> JSON bool: {payload}"
    );
    assert_eq!(
        payload["small"],
        Value::from(7),
        "int2 -> JSON number: {payload}"
    );
    assert_eq!(
        payload["medium"],
        Value::from(-42),
        "int4 -> JSON number: {payload}"
    );
    assert_eq!(
        payload["big"],
        Value::String("9223372036854775807".to_string()),
        "int8 (>2^53) -> JSON string: {payload}"
    );
    assert_eq!(
        payload["amount"],
        Value::String("3.14159265358979323846".to_string()),
        "numeric -> JSON string (arbitrary precision preserved): {payload}"
    );
    assert_eq!(
        payload["ratio"],
        Value::String("NaN".to_string()),
        "float8 NaN -> quoted string (RFC 8259 guard): {payload}"
    );
    assert_eq!(
        payload["ts"],
        Value::String("2026-07-12T18:00:00Z".to_string()),
        "timestamptz -> RFC 3339 UTC ('+05:30' input normalizes via the session's UTC TimeZone): {payload}"
    );
    assert_eq!(
        payload["uid"],
        Value::String(uid.to_string()),
        "uuid -> canonical lowercase string: {payload}"
    );
    assert_eq!(
        payload["blob"],
        Value::String("aGk=".to_string()),
        "bytea (hex 6869 = \"hi\") -> base64 string: {payload}"
    );
    assert_eq!(
        payload["payload"],
        Value::String(r#"{"a": 1, "b": [1, 2, 3]}"#.to_string()),
        "jsonb -> the serialized JSON text AS a JSON string (Debezium convention): {payload}"
    );
    assert_eq!(
        payload["note"],
        Value::Null,
        "NULL -> JSON null (never fabricated): {payload}"
    );
}

/// The core F5 assertion: every OID-mapped column renders correctly via the
/// SNAPSHOT path (fresh-slot pre-existing row).
#[tokio::test]
async fn snapshot_row_renders_typed_json() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_typed_snap_{}", std::process::id());
    let sql = sql_client().await;
    ensure_typed_probe(&sql).await;
    drop_slot(&sql, &slot).await;

    let id = uuid::Uuid::new_v4();
    let uid = uuid::Uuid::new_v4();
    insert_probe_row(&sql, id, uid).await;

    let mut repl =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, PUBLICATION).unwrap());
    repl.ensure_connected().await.unwrap();

    let events = collect_events(&mut repl, 8, Duration::from_secs(5)).await;
    assert!(!events.is_empty(), "snapshot yielded no events");
    let payload = find_payload(&events, id);
    assert_typed_shape(&payload, uid);

    drop(repl);
    for _ in 0..40 {
        let active = sql
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|r| r.get::<_, bool>(0));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop_slot(&sql, &slot).await;
}

/// The same assertion via the STREAMING path (live INSERT after connect).
#[tokio::test]
async fn streamed_row_renders_typed_json() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_typed_stream_{}", std::process::id());
    let sql = sql_client().await;
    ensure_typed_probe(&sql).await;
    drop_slot(&sql, &slot).await;

    let mut repl =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, PUBLICATION).unwrap());
    repl.ensure_connected().await.unwrap();

    let id = uuid::Uuid::new_v4();
    let uid = uuid::Uuid::new_v4();
    insert_probe_row(&sql, id, uid).await;

    let events = collect_events(&mut repl, 8, Duration::from_secs(5)).await;
    let payload = find_payload(&events, id);
    assert_typed_shape(&payload, uid);

    drop(repl);
    for _ in 0..40 {
        let active = sql
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|r| r.get::<_, bool>(0));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop_slot(&sql, &slot).await;
}

/// The "one mapping, both callers" contract, proven directly: a row seeded
/// BEFORE the replicator connects (delivered via the snapshot) and a row
/// with IDENTICAL non-pk content inserted AFTER connecting (delivered via
/// the live stream) must render byte-identical JSON once the `id` field
/// (necessarily different) is excluded from the comparison.
#[tokio::test]
async fn snapshot_and_streamed_rows_of_identical_content_are_byte_identical() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    let slot = format!("e2e_typed_both_{}", std::process::id());
    let sql = sql_client().await;
    ensure_typed_probe(&sql).await;
    drop_slot(&sql, &slot).await;

    // Same `uid` value for both rows too, so the ONLY difference between the
    // two payloads is the `id` primary key.
    let shared_uid = uuid::Uuid::new_v4();
    let snapshot_id = uuid::Uuid::new_v4();
    insert_probe_row(&sql, snapshot_id, shared_uid).await;

    let mut repl =
        PgReplicator::new(PgReplicatorConfig::from_url(&pg_url(), &slot, PUBLICATION).unwrap());
    repl.ensure_connected().await.unwrap();

    let snapshot_events = collect_events(&mut repl, 8, Duration::from_secs(5)).await;
    let mut snapshot_payload = find_payload(&snapshot_events, snapshot_id);

    let streamed_id = uuid::Uuid::new_v4();
    insert_probe_row(&sql, streamed_id, shared_uid).await;
    let streamed_events = collect_events(&mut repl, 8, Duration::from_secs(5)).await;
    let mut streamed_payload = find_payload(&streamed_events, streamed_id);

    // Strip the necessarily-different pk before comparing.
    snapshot_payload.as_object_mut().unwrap().remove("id");
    streamed_payload.as_object_mut().unwrap().remove("id");
    assert_eq!(
        snapshot_payload, streamed_payload,
        "snapshot row and streamed row of identical content must render byte-identical JSON \
         (minus the necessarily-different pk)"
    );

    drop(repl);
    for _ in 0..40 {
        let active = sql
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|r| r.get::<_, bool>(0));
        if !active {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop_slot(&sql, &slot).await;
}
