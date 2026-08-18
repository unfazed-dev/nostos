//! P5 sync streams (docs/plans/p5-sync-streams-design.md §6): the PG-gated
//! end-to-end for named, client-parameterized streams — lazy mid-session
//! subscribe, targeted per-stream snapshot, unsubscribe, cross-tenant abuse
//! gate, two-streams-one-table dedup, reconnect re-subscribe, and the
//! rules-denied non-fatal reject.
//!
//! ## Running
//!
//! ```sh
//! make pg-up
//! NOSTOS_E2E_PG=1 cargo test -p nostos-infra --features pg --test e2e_pg_sync_streams -- --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is REQUIRED: these tests TRUNCATE the shared `tasks`
//! table and assert exact snapshot contents; a parallel within-binary run
//! races (the same landmine the other pg e2e files document).

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use nostos_application::ports::SyncAuth;
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, Principal, ReplicationEvent, StreamRule, SyncMode, SyncRules};
use nostos_infra::auth::AllowAnonymous;
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::snapshot_source::PgSnapshotter;
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";

fn pg_url() -> String {
    std::env::var("NOSTOS_PG_URL")
        .unwrap_or_else(|_| "postgresql://cairn:cairn@localhost:5433/cairn".into())
}

/// Init tracing so server-side stream/snapshot logs surface under
/// --nocapture (diagnoses routing vs snapshot failures).
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=debug,info")
        .with_test_writer()
        .try_init();
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

/// A `SyncAuth` test-double: the token names the tenant; principals carry a
/// UUID tenant id because `tasks.org_id` is `UUID NOT NULL` (mirrors
/// `e2e_pg_writeback.rs::TwoTenantAuth`).
struct TwoTenantAuth {
    tenants: std::collections::HashMap<String, Principal>,
}

impl TwoTenantAuth {
    fn new(acme_org: &str, other_org: &str) -> Self {
        let mut tenants = std::collections::HashMap::new();
        tenants.insert(
            "acme".to_string(),
            Principal::new("acme-user", acme_org.to_string()),
        );
        tenants.insert(
            "other".to_string(),
            Principal::new("other-user", other_org.to_string()),
        );
        Self { tenants }
    }
}

#[async_trait::async_trait]
impl SyncAuth for TwoTenantAuth {
    async fn authenticate(&self, token: &str) -> Option<Principal> {
        self.tenants.get(token).cloned()
    }
}

/// Spin up the in-process axum sync server with a PgReplicator driver, a
/// PgSnapshotter (the stream-snapshot adapter under test), and a ruleset
/// carrying `[streams]` definitions. Mirrors `e2e_pg_writeback::spawn_server_with`.
async fn spawn_stream_server(
    slot: &str,
    auth: Arc<dyn SyncAuth>,
    tenant_column: Option<&str>,
    rules: SyncRules,
) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)));

    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), slot, "cairn_pub").expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg);
    let fanout_drv = Arc::clone(&fanout);
    tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let parsed: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            parsed
                .get(col)
                .and_then(|v| v.as_str())
                .map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    let snapshotter: Arc<dyn nostos_application::ports::SnapshotSource> =
        Arc::new(PgSnapshotter::new(&pg_url()));
    let compiled = nostos_application::ActiveRuleset::compile(&rules).expect("rules compile");
    let rules_shared = Arc::new(tokio::sync::RwLock::new(compiled.clone()));
    let (rules_tx, rules_changed) = watch::channel(compiled.checksum());

    let mut state = SyncRouterState::new(Arc::clone(&manager), auth)
        .with_buffer(1024)
        .with_snapshotter(snapshotter)
        .with_rules(rules_shared, rules_changed, rules_tx);
    if let Some(col) = tenant_column {
        state = state.with_tenant_column(col);
    }
    let app = axum::Router::new()
        .route("/sync", get(sync_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });
    (addr, shutdown_tx, server)
}

/// Hex-decode a row frame's payload (the wire carries hex; tests need the
/// JSON inside).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What the socket received, classified. Row frames are the raw JSON values
/// (single objects or C3-batched arrays, flattened).
#[derive(Default)]
struct Collected {
    /// Row payloads (the `payload` object of each row frame) as JSON values.
    rows: Vec<serde_json::Value>,
    /// `(table, stream, begin)` control frames, in arrival order.
    boundaries: Vec<(String, Option<String>, bool)>,
    /// `(id, error)` stream_error frames.
    stream_errors: Vec<(String, String)>,
}

impl Collected {
    fn rows_containing(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .filter(|r| r.to_string().contains(needle))
            .count()
    }
}

/// Collect frames until `deadline`, classifying rows / boundaries /
/// stream_errors. Batched arrays are flattened to their members.
async fn collect_for<S>(ws: &mut S, window: Duration) -> Collected
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut out = Collected::default();
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        else {
            continue;
        };
        let bytes = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let frames: Vec<serde_json::Value> = if v.is_array() {
            v.as_array().unwrap().clone()
        } else {
            vec![v]
        };
        for f in frames {
            // Row frames have NO "type" key ({"lsn","op","table","pk",
            // "payload"}); control frames do. Dispatch on that.
            match f.get("type").and_then(|t| t.as_str()) {
                Some("snapshot_begin") | Some("snapshot_end") => {
                    let t = f["type"].as_str().unwrap();
                    let table = f["table"].as_str().unwrap_or("").to_string();
                    let stream = f.get("stream").and_then(|s| s.as_str()).map(str::to_string);
                    out.boundaries.push((table, stream, t == "snapshot_begin"));
                }
                Some("stream_error") => {
                    out.stream_errors.push((
                        f["id"].as_str().unwrap_or("").to_string(),
                        f["error"].as_str().unwrap_or("").to_string(),
                    ));
                }
                Some("resume_info") | Some("write_result") => {}
                Some(_) => {}
                None => {
                    // A row frame: lsn/op/table/pk + HEX-encoded payload —
                    // decode the hex, then parse the payload JSON inside.
                    if f.get("lsn").is_some() {
                        if let Some(hex) = f["payload"].as_str() {
                            if let Some(bytes) = decode_hex(hex) {
                                if let Ok(payload) =
                                    serde_json::from_slice::<serde_json::Value>(&bytes)
                                {
                                    out.rows.push(payload);
                                    continue;
                                }
                            }
                        }
                        out.rows.push(f["payload"].clone());
                    }
                }
            }
        }
    }
    out
}

/// Send a `subscribe_stream` frame.
async fn send_subscribe_stream<S>(ws: &mut S, id: &str, stream: &str, params: serde_json::Value)
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let frame = serde_json::json!({
        "type": "subscribe_stream",
        "id": id,
        "stream": stream,
        "params": params,
    });
    ws.send(Message::Text(frame.to_string())).await.unwrap();
}

/// A base subscribe frame carrying a where_sql predicate (ADR-0012) — local
/// because `common::subscribe_frame*` has no where_sql variant.
fn subscribe_where(table: &str, where_sql: &str) -> String {
    serde_json::json!({
        "type": "subscribe",
        "table": table,
        "filters": [],
        "where_sql": where_sql,
    })
    .to_string()
}

fn titled_rules(extra: &[StreamRule]) -> SyncRules {
    let mut streams = vec![StreamRule {
        name: "titled".into(),
        table: "tasks".into(),
        template: "title = :t".into(),
    }];
    streams.extend(extra.iter().cloned());
    SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::All,
        tables: vec![],
        hand: vec![],
        streams,
    }
}

/// §6.1 — lazy mid-session stream: base table subscribed (with a never-
/// matching where_sql so the BASE session delivers nothing), stream added
/// mid-session → targeted snapshot of exactly the matching rows, then a live
/// matching INSERT arrives and a non-matching one never does.
#[tokio::test]
async fn lazy_stream_snapshot_then_live_delta() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    init_tracing();
    let slot = format!("e2e_stream1_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    let (addr, shutdown, _server) = spawn_stream_server(
        &slot,
        Arc::new(AllowAnonymous::new()),
        None,
        titled_rules(&[]),
    )
    .await;

    let title_hit = format!("stream-hit-{}", uuid::Uuid::new_v4());
    let title_miss = format!("stream-miss-{}", uuid::Uuid::new_v4());

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    // Base subscribe with a where_sql that matches nothing — isolates the
    // stream's delivery from the base session's (the base table-level
    // snapshot on an empty table is an empty begin/end pair).
    ws.send(Message::Text(subscribe_where(
        "tasks",
        "title = 'zzz-never-matches'",
    )))
    .await
    .unwrap();
    let _ = collect_for(&mut ws, Duration::from_millis(800)).await;

    // Seed AFTER the base snapshot drained: 2 matching + 1 non-matching row.
    for title in [&title_hit, &title_hit, &title_miss] {
        sql.execute(
            "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
            &[&uuid::Uuid::new_v4(), title],
        )
        .await
        .unwrap();
    }
    // The base session matches nothing, so these live inserts deliver nothing.
    let pre = collect_for(&mut ws, Duration::from_millis(800)).await;
    assert_eq!(
        pre.rows.len(),
        0,
        "base where_sql matched something: {:?}",
        pre.rows
    );

    // Lazy mid-session stream add → targeted snapshot of exactly the matches.
    send_subscribe_stream(&mut ws, "s1", "titled", serde_json::json!({"t": title_hit})).await;
    let got = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert!(
        got.stream_errors.is_empty(),
        "unexpected stream_error: {:?}",
        got.stream_errors
    );
    assert_eq!(
        got.rows_containing(&title_hit),
        2,
        "stream snapshot must deliver exactly the 2 matching rows; got rows {:?}, boundaries {:?}",
        got.rows,
        got.boundaries
    );
    assert_eq!(
        got.rows_containing(&title_miss),
        0,
        "non-matching row leaked"
    );
    assert!(
        got.boundaries
            .iter()
            .any(|(t, s, b)| t == "tasks" && s.as_deref() == Some("s1") && *b),
        "stream-tagged snapshot_begin missing: {:?}",
        got.boundaries
    );

    // Live delta: a matching INSERT arrives; a non-matching one never does.
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &title_hit],
    )
    .await
    .unwrap();
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &title_miss],
    )
    .await
    .unwrap();
    let live = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert!(
        live.rows_containing(&title_hit) >= 1,
        "live matching insert must arrive via the stream session"
    );
    assert_eq!(
        live.rows_containing(&title_miss),
        0,
        "non-matching live insert must NOT arrive (stream predicate)"
    );

    drop(ws);
    let _ = shutdown.send(());
    drop_slot(&sql, &slot).await;
}

/// §6.2 + §6.4 — unsubscribe stops flow (post-unsubscribe mutations never
/// arrive) while a second stream on the SAME table keeps flowing; a row
/// matching BOTH streams arrives exactly once (shared sink dedup ring).
#[tokio::test]
async fn unsubscribe_stops_flow_and_two_streams_dedup() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    init_tracing();
    let slot = format!("e2e_stream2_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    // Two streams, same table, same template — a row matching both params
    // exercises the dedup ring; disjoint params exercise independence.
    let rules = titled_rules(&[StreamRule {
        name: "titled2".into(),
        table: "tasks".into(),
        template: "title = :t".into(),
    }]);
    let (addr, shutdown, _server) =
        spawn_stream_server(&slot, Arc::new(AllowAnonymous::new()), None, rules).await;

    let shared = format!("stream-shared-{}", uuid::Uuid::new_v4());
    let only_b = format!("stream-onlyb-{}", uuid::Uuid::new_v4());

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(subscribe_where(
        "tasks",
        "title = 'zzz-never-matches'",
    )))
    .await
    .unwrap();
    let _ = collect_for(&mut ws, Duration::from_millis(800)).await;

    // s1 and s2 with the SAME param (both match `shared`).
    send_subscribe_stream(&mut ws, "s1", "titled", serde_json::json!({"t": shared})).await;
    send_subscribe_stream(&mut ws, "s2", "titled2", serde_json::json!({"t": shared})).await;
    let _ = collect_for(&mut ws, Duration::from_secs(2)).await;

    // A row matching BOTH streams arrives exactly once (dedup ring).
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &shared],
    )
    .await
    .unwrap();
    let got = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(
        got.rows_containing(&shared),
        1,
        "row matching two streams must arrive exactly once; got {:?}",
        got.rows
    );

    // Unsubscribe s1; s2 keeps flowing.
    ws.send(Message::Text(
        serde_json::json!({"type": "unsubscribe_stream", "id": "s1"}).to_string(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Post-unsubscribe: insert ANOTHER row with the same `shared` title —
    // s1 is gone, s2 still matches it exactly → the row arrives exactly once
    // via s2.
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &shared],
    )
    .await
    .unwrap();
    // A row matching neither stream — nothing must arrive.
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &only_b],
    )
    .await
    .unwrap();
    let got = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(
        got.rows_containing(&shared),
        1,
        "s2 must still deliver after s1 unsubscribed; got {:?}",
        got.rows
    );
    assert_eq!(
        got.rows_containing(&only_b),
        0,
        "no stream matches only_b — nothing must arrive; got {:?}",
        got.rows
    );

    drop(ws);
    let _ = shutdown.send(());
    drop_slot(&sql, &slot).await;
}

/// §6.3 — the hard gate: authenticated as tenant A, params attempting escape
/// (another tenant's id on a tenant-column placeholder, plus metacharacter
/// values). Only tenant-A rows may EVER arrive; a rogue binding yields an
/// empty snapshot (the fail-closed AND-wrap), never an interpolation error.
#[tokio::test]
async fn cross_tenant_param_abuse_never_leaks() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    init_tracing();
    let slot = format!("e2e_stream3_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    let acme_org = uuid::Uuid::new_v4().to_string();
    let other_org = uuid::Uuid::new_v4().to_string();
    let acme_title = format!("acme-{}", uuid::Uuid::new_v4());
    let other_title = format!("other-{}", uuid::Uuid::new_v4());
    // org_id is `UUID NOT NULL` — bind typed UUIDs (a String bind fails ToSql).
    let acme_uuid = uuid::Uuid::parse_str(&acme_org).unwrap();
    let other_uuid = uuid::Uuid::parse_str(&other_org).unwrap();
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&acme_uuid, &acme_title],
    )
    .await
    .unwrap();
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&other_uuid, &other_title],
    )
    .await
    .unwrap();

    let rules = SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::All,
        tables: vec![],
        hand: vec![],
        streams: vec![StreamRule {
            name: "by_org".into(),
            table: "tasks".into(),
            template: "org_id = :org".into(),
        }],
    };
    let (addr, shutdown, _server) = spawn_stream_server(
        &slot,
        Arc::new(TwoTenantAuth::new(&acme_org, &other_org)),
        Some("org_id"),
        rules,
    )
    .await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync?token=acme"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(subscribe_where(
        "tasks",
        "title = 'zzz-never-matches'",
    )))
    .await
    .unwrap();
    let _ = collect_for(&mut ws, Duration::from_millis(800)).await;

    // (a) Tenant escape attempt: bind the OTHER tenant's id.
    send_subscribe_stream(
        &mut ws,
        "rogue",
        "by_org",
        serde_json::json!({"org": other_org}),
    )
    .await;
    let got = collect_for(&mut ws, Duration::from_secs(3)).await;
    assert_eq!(
        got.rows.len(),
        0,
        "tenant-b param under tenant-A auth must yield ZERO rows; got {:?}",
        got.rows
    );
    assert!(
        got.stream_errors.is_empty(),
        "the AND-wrap is fail-closed-but-valid: no error frame expected; got {:?}",
        got.stream_errors
    );

    // (b) Metacharacter values: injection text is just a string bind — the
    // snapshot returns data-or-empty, NEVER an interpolation error.
    for evil in ["x' OR '1'='1", "'; DROP TABLE tasks;--"] {
        send_subscribe_stream(
            &mut ws,
            &format!("evil-{}", &evil[..6].replace(['\'', ' ', ';'], "_")),
            "by_org",
            serde_json::json!({"org": evil}),
        )
        .await;
    }
    let got = collect_for(&mut ws, Duration::from_secs(3)).await;
    assert_eq!(
        got.rows.len(),
        0,
        "metachar params matched nothing (expected)"
    );
    assert!(
        got.stream_errors.is_empty(),
        "metachar binds must not error — they are inert strings; got {:?}",
        got.stream_errors
    );

    // (c) Live: a new acme row arrives (via a legitimately-bound stream), a
    // new other-org row NEVER does.
    send_subscribe_stream(
        &mut ws,
        "legit",
        "by_org",
        serde_json::json!({"org": acme_org}),
    )
    .await;
    let _ = collect_for(&mut ws, Duration::from_secs(2)).await;
    let acme_live = format!("acme-live-{}", uuid::Uuid::new_v4());
    let other_live = format!("other-live-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&acme_uuid, &acme_live],
    )
    .await
    .unwrap();
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&other_uuid, &other_live],
    )
    .await
    .unwrap();
    let got = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert!(
        got.rows_containing(&acme_live) >= 1,
        "tenant-A live row must arrive via the legit stream"
    );
    assert_eq!(
        got.rows_containing(&other_live),
        0,
        "tenant-B live row must NEVER arrive on tenant A's socket"
    );

    drop(ws);
    let _ = shutdown.send(());
    drop_slot(&sql, &slot).await;
}

/// §6.5 — reconnect: a fresh socket re-subscribing the same stream gets a
/// fresh targeted snapshot (no per-stream resume in v1; the client's
/// checkpoint + idempotent apply own dedup, asserted at that layer).
#[tokio::test]
async fn reconnect_resubscribes_and_resnapshots() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    init_tracing();
    let slot = format!("e2e_stream5_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    let (addr, shutdown, _server) = spawn_stream_server(
        &slot,
        Arc::new(AllowAnonymous::new()),
        None,
        titled_rules(&[]),
    )
    .await;
    let title = format!("stream-re-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &title],
    )
    .await
    .unwrap();

    for attempt in 1..=2 {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
            .await
            .expect("ws connect");
        ws.send(Message::Text(subscribe_where(
            "tasks",
            "title = 'zzz-never-matches'",
        )))
        .await
        .unwrap();
        send_subscribe_stream(&mut ws, "s1", "titled", serde_json::json!({"t": title})).await;
        let got = collect_for(&mut ws, Duration::from_secs(5)).await;
        assert!(
            got.rows_containing(&title) >= 1,
            "attempt {attempt}: re-subscribed stream must re-snapshot the row"
        );
        drop(ws); // close the socket — the server-side stream session dies with it
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = shutdown.send(());
    drop_slot(&sql, &slot).await;
}

/// §6.6 — a stream on a rules-denied (`toggles`) table rejects with a
/// NON-fatal `stream_error`; the socket stays up and the synced base table
/// keeps flowing.
#[tokio::test]
async fn stream_on_rules_denied_table_errors_non_fatally() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `make pg-up` to run)");
        return;
    }
    init_tracing();
    let slot = format!("e2e_stream6_{}", std::process::id());
    let sql = sql_client().await;
    drop_slot(&sql, &slot).await;
    sql.execute("TRUNCATE TABLE tasks;", &[]).await.unwrap();

    let rules = SyncRules {
        version: nostos_domain::RULES_VERSION,
        mode: SyncMode::Toggles,
        tables: vec![
            nostos_domain::TableRule {
                table: "tasks".into(),
                sync: true,
                scope: None,
            },
            nostos_domain::TableRule {
                table: "providers".into(),
                sync: false,
                scope: None,
            },
        ],
        hand: vec![],
        streams: vec![StreamRule {
            name: "denied".into(),
            table: "providers".into(),
            template: "name = :n".into(),
        }],
    };
    let (addr, shutdown, _server) =
        spawn_stream_server(&slot, Arc::new(AllowAnonymous::new()), None, rules).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/sync"))
        .await
        .expect("ws connect");
    ws.send(Message::Text(common::subscribe_frame("tasks", &[])))
        .await
        .unwrap();
    let _ = collect_for(&mut ws, Duration::from_millis(800)).await;

    send_subscribe_stream(&mut ws, "s1", "denied", serde_json::json!({"n": "x"})).await;
    let got = collect_for(&mut ws, Duration::from_secs(3)).await;
    assert_eq!(
        got.stream_errors.len(),
        1,
        "expected one stream_error; got {:?}",
        got.stream_errors
    );
    assert_eq!(got.stream_errors[0].0, "s1");
    assert!(
        got.stream_errors[0].1.contains("not synced"),
        "error should name the rules denial; got {}",
        got.stream_errors[0].1
    );

    // Socket stays up: a live insert on the SYNCED base table still flows.
    let title = format!("after-denied-{}", uuid::Uuid::new_v4());
    sql.execute(
        "INSERT INTO tasks (org_id, title) VALUES ($1, $2)",
        &[&uuid::Uuid::new_v4(), &title],
    )
    .await
    .unwrap();
    let got = collect_for(&mut ws, Duration::from_secs(5)).await;
    assert!(
        got.rows_containing(&title) >= 1,
        "base table must keep flowing after a non-fatal stream_error"
    );

    drop(ws);
    let _ = shutdown.send(());
    drop_slot(&sql, &slot).await;
}
