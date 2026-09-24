//! ADR-0025 slice 6 — real-Postgres op-log replay + restart-resilience e2e.
//!
//! Proves the persisted-op-log resume thesis: a client disconnected during a
//! server-side change batch reconnects with a matching epoch + in-window
//! checkpoint and receives the offline gap via op-log REPLAY (including
//! DELETEs) — NOT a full snapshot. Plus the aged-out fallback (empty/aged
//! op-log → snapshot-reconcile, slice 1 the safety net).
//!
//! Runs in TENANT MODE (`SupabaseJwtAuth` + `tenant_column = "org_id"`) because
//! the op-log writer stores `tenant_id = payload[tenant_column]` and the reader
//! filters `WHERE tenant_id = principal.tenant_id` — they align only under
//! tenant enforcement (ADR-0018). Anonymous mode writes NULL tenant_id and the
//! replay query never matches.
//!
//! The real client sends `epoch: None` (client-side epoch tracking is the
//! deferred follow-up), so this test drives the protocol with RAW frames +
//! `epoch` set explicitly (mirrors the existing e2e_pg_* style).
//!
//! ## Running
//! ```sh
//! docker compose -f docker/docker-compose.yml up -d
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn \
//!   cargo test -p nostos-infra --features pg --test e2e_pg_oplog_replay \
//!   -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nostos_application::ports::{Metrics, OpLogWriter, SessionStore};
use nostos_application::{ActiveRuleset, FanOutService, SessionManager};
use nostos_domain::{compose_sync_epoch, ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::{PgOpLogReader, PgOpLogWriter, PgSnapshotter, SupabaseJwtAuth};

use common::subscribe_frame_with_epoch;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio_tungstenite::tungstenite::Message;

type HmacSha256 = Hmac<Sha256>;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const SECRET: &[u8] = b"e2e-oplog-replay-secret";
const TENANT_COL: &str = "org_id";

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

async fn wait_for<F, Fut>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    predicate().await
}

async fn slot_exists(slot: &str) -> bool {
    match tokio_postgres::connect(&pg_url(), tokio_postgres::NoTls).await {
        Ok((c, conn)) => {
            tokio::spawn(async move {
                let _ = conn.await;
            });
            c.query_opt(
                "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .is_ok_and(|o| o.is_some())
        }
        Err(_) => false,
    }
}

/// Highest lsn the op-log writer has persisted for `tenant` (0 if none). This
/// is the client's effective resume point — the server wrote it at the fan-out
/// chokepoint regardless of whether the (offline) client received the live
/// event, so it's a sound, tenant-scoped checkpoint to replay from.
async fn oplog_max_lsn(tenant: &str) -> i64 {
    let c = sql_client().await;
    let row: i64 = c
        .query_one(
            "SELECT COALESCE(MAX(lsn), 0)::bigint FROM cairn_oplog WHERE tenant_id = $1",
            &[&tenant],
        )
        .await
        .expect("oplog max lsn")
        .get(0);
    row
}

/// JWT base64url (no padding).
fn jwt_b64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(TABLE[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(TABLE[((buf << (6 - bits)) & 0x3F) as usize] as char);
    }
    out
}

/// Mint an HS256 JWT carrying `{"sub": sub}`, signed with SECRET. Minimal valid
/// Supabase token for `SupabaseJwtAuth::new(SECRET)`; `principal.tenant_id = sub`.
fn mint_jwt(sub: &str) -> String {
    let header = b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}";
    // `exp` is required since the v0.2.0 audit (finding 5); far future so
    // the token never expires mid-test.
    let payload = format!("{{\"sub\":\"{sub}\",\"exp\":{}}}", 4_102_444_800i64);
    let h = jwt_b64url(header);
    let p = jwt_b64url(payload.as_bytes());
    let signing_input = format!("{h}.{p}");
    let mut mac = HmacSha256::new_from_slice(SECRET).expect("hmac key");
    mac.update(signing_input.as_bytes());
    let sig = jwt_b64url(&mac.finalize().into_bytes());
    format!("{signing_input}.{sig}")
}

struct Harness {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    token: String,
    driver: tokio::task::JoinHandle<()>,
}

/// Bring up the production-shaped stack against real PG in TENANT MODE: a
/// `FanOutService` with the `PgOpLogWriter` attached (so `cairn_oplog` is
/// seeded) + a `PgReplicator` driver + an axum WS server whose
/// `SyncRouterState` wires snapshotter + oplog_reader + metrics + tenant
/// enforcement. The same `metrics`/`store`/`manager` are shared between the
/// driver and the WS server so events the driver fans out reach subscribed
/// WS clients.
async fn harness(tenant: &str, slot: &str) -> Harness {
    let sql = sql_client().await;
    // Drop a leftover slot from a prior run (same PID ⇒ same slot name; the two
    // tests share the PID so test 2 must drop test 1's slot). NOTE: we do NOT
    // delete `tasks` rows here — the caller inserts the seed row BEFORE calling
    // `harness`, so a cleanup here would delete it. Cross-run isolation is by
    // the per-run tenant UUID: the op-log query is `WHERE tenant_id = <uuid>`,
    // so prior runs' rows (other UUIDs) never match.
    let _ = sql
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    // Confirm the op-log table exists (docker/pg-init applies it). `query` (not
    // `query_one`) so an EMPTY-but-existing table returns Ok([]) instead of a
    // RowCount error — the table is empty at fresh setup before any event lands.
    let _ = sql
        .query("SELECT 1 FROM cairn_oplog LIMIT 1", &[])
        .await
        .expect("cairn_oplog table exists (run docker compose up -d)");

    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    // Op-log writer at the fan-out chokepoint. tenant_column = "org_id" so each
    // op-log row carries the tenant (the reader filters on it).
    let oplog: Arc<dyn OpLogWriter> = Arc::new(PgOpLogWriter::new(
        &pg_url(),
        Some(TENANT_COL.to_string()),
        4096,
        Some(Arc::clone(&metrics)),
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)).with_op_log(oplog));

    // Replicator driver. The slot-creation snapshot seeds the op-log with the
    // table's existing rows (each fanned-out event is written to cairn_oplog).
    let pg_cfg = PgReplicatorConfig::from_url(&pg_url(), slot, "cairn_pub").expect("valid PG url");
    let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
    let fanout_drv = Arc::clone(&fanout);
    let driver = tokio::spawn(async move {
        let extract = |e: &ReplicationEvent, col: &str| -> Option<ColumnValue> {
            let p: serde_json::Value = serde_json::from_slice(e.payload_bytes()).ok()?;
            p.get(col).and_then(|v| v.as_str()).map(ColumnValue::text)
        };
        let _ = fanout_drv.run(&mut repl, extract).await;
    });

    // Wait for the slot to exist AND slot_epoch to bump to ≥1. The epoch bump
    // (`record_epoch_bump`) runs a beat AFTER `pg_create_logical_replication_slot`
    // in the fresh-create block, so checking slot_exists alone races — a prompt
    // read of server_epoch would see 0 in the window. Polling the metric closes it.
    let metrics_for_epoch = Arc::clone(&metrics);
    let slot_owned = slot.to_string();
    assert!(
        wait_for(Duration::from_secs(15), || {
            let m = Arc::clone(&metrics_for_epoch);
            let s = slot_owned.clone();
            async move { slot_exists(&s).await && m.snapshot().slot_epoch >= 1 }
        })
        .await,
        "slot was not created + epoch bumped on initial connect"
    );

    // WS server in tenant mode.
    let auth = Arc::new(SupabaseJwtAuth::new(SECRET.to_vec()));
    let state = SyncRouterState::new(Arc::clone(&manager), auth)
        .with_buffer(1024)
        .with_metrics(Arc::clone(&metrics))
        .with_tenant_column(TENANT_COL)
        .with_snapshotter(Arc::new(PgSnapshotter::new(&pg_url())))
        .with_oplog_reader(Arc::new(PgOpLogReader::new(&pg_url())));
    let app = axum::Router::new()
        .route("/sync", axum::routing::get(sync_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    std::mem::forget(server);

    Harness {
        addr,
        metrics,
        token: mint_jwt(tenant),
        driver,
    }
}

/// Insert a tasks row for `tenant`, return its id (the pk the client will see
/// in snapshot + delete frames).
async fn insert_task(tenant: &str, title: &str) -> String {
    let c = sql_client().await;
    let id: uuid::Uuid = c
        .query_one(
            "INSERT INTO tasks (org_id, title) VALUES ($1, $2) RETURNING id",
            &[
                &uuid::Uuid::parse_str(tenant).expect("tenant is a uuid"),
                &title,
            ],
        )
        .await
        .expect("insert task")
        .get(0);
    id.to_string()
}

/// Connect to /sync with the bearer token, send `subscribe_json`, collect
/// received frames (parsed as JSON) until `collect_for` elapses.
async fn collect_frames(
    addr: SocketAddr,
    token: &str,
    subscribe_json: &str,
    collect_for: Duration,
) -> Vec<serde_json::Value> {
    let url = format!("ws://{addr}/sync?token={token}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    ws.send(Message::Text(subscribe_json.to_string()))
        .await
        .expect("send subscribe");
    let mut frames = Vec::new();
    let start = Instant::now();
    while start.elapsed() < collect_for {
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        {
            let bytes: Vec<u8> = match msg {
                Message::Text(t) => t.into_bytes(),
                Message::Binary(b) => b,
                _ => continue,
            };
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                // Flatten C3 batched arrays: the writer coalesces a backlog of
                // frames into one JSON array message (decoded by `decode_frames`
                // on the client). Without flattening, a batched `[f1,f2,f3]`
                // lands as ONE nested value + the per-frame checks below miss
                // every frame inside it.
                match v {
                    serde_json::Value::Array(arr) => frames.extend(arr),
                    single => frames.push(single),
                }
            }
        }
    }
    frames
}

fn frame_type(v: &serde_json::Value) -> Option<&str> {
    v.get("type").and_then(|t| t.as_str())
}
fn frame_op(v: &serde_json::Value) -> Option<&str> {
    v.get("op").and_then(|o| o.as_str())
}
fn frame_pk(v: &serde_json::Value) -> Option<&str> {
    v.get("pk").and_then(|p| p.as_str())
}

/// A reconnect that REPLAYS delivers the offline gap (incl. deletes) and NO
/// snapshot boundary control frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oplog_replay_delivers_offline_gap_including_deletes() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `docker compose up -d` to run)");
        return;
    }
    // Init tracing so the op-log writer's flush warns + the replicator's
    // snapshot/read logs surface under --nocapture (diagnoses why cairn_oplog
    // stays empty: writer INSERT failure vs snapshot-read gap vs no events).
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=debug,info")
        .with_test_writer()
        .try_init();
    let tenant = uuid::Uuid::new_v4().to_string();
    let slot = format!("e2e_oplog_replay_{}", std::process::id());

    // Seed an initial row so the snapshot + the slot-creation op-log seed exist.
    let _initial = insert_task(&tenant, "e2e-oplog-replay-initial").await;
    let h = harness(&tenant, &slot).await;
    let server_epoch = h.metrics.snapshot().slot_epoch;
    assert!(
        server_epoch >= 1,
        "slot_epoch must be ≥1 after slot creation; the replay gate needs it"
    );

    // LIVE kick: a write AFTER the slot exists flows through the replicator's
    // live stream → next_event returns it → fan-out → op-log append. (The slot-
    // creation snapshot is staged in pending_snapshot but only drains after a
    // live event kicks next_event; the op-log carries CHANGES, the initial
    // snapshot reaches clients via PgSnapshotter — ADR-0025 by design.)
    let _kick = insert_task(&tenant, "e2e-oplog-replay-kick").await;

    // First connect: epoch=None ⇒ snapshot. Drain it (we don't assert on the
    // snapshot here; the load-bearing assertion is the RECONNECT).
    let first = collect_frames(
        h.addr,
        &h.token,
        &subscribe_frame_with_epoch("tasks", &[], None, None),
        Duration::from_secs(2),
    )
    .await;
    let _ = first;

    // Capture the client's resume point = the highest op-log lsn persisted for
    // this tenant (the slot-creation seed). Wait for the writer to flush it.
    let seeded = wait_for(Duration::from_secs(30), || {
        let tenant = tenant.clone();
        async move { oplog_max_lsn(&tenant).await > 0 }
    })
    .await;
    assert!(seeded, "op-log seed never landed for tenant {tenant}");
    let checkpoint = u64::try_from(oplog_max_lsn(&tenant).await).unwrap_or(0);

    // Offline gap: insert + delete (the delete is the load-bearing op — it must
    // arrive via replay, proving the op-log carries DELETEs not just upserts).
    let gap_keep = insert_task(&tenant, "e2e-oplog-replay-gap-keep").await;
    let gap_del = insert_task(&tenant, "e2e-oplog-replay-gap-del").await;
    {
        let c = sql_client().await;
        c.execute(
            "DELETE FROM tasks WHERE id = $1",
            &[&gap_del.parse::<uuid::Uuid>().unwrap()],
        )
        .await
        .expect("delete gap row");
    }
    // Wait for the writer to persist the WHOLE gap. The delete is the last op
    // (highest lsn); waiting for it ensures all 3 (keep-insert, del-insert,
    // delete) flushed before the replay reads — else the replay sees only the
    // first-op-to-flush + misses the rest (the gap_landed-on-max-lsn race).
    let gap_del_pk = gap_del.clone();
    let gap_landed = wait_for(Duration::from_secs(30), || {
        let pk = gap_del_pk.clone();
        async move {
            let c = sql_client().await;
            let n: i64 = c
                .query_one(
                    "SELECT count(*) FROM cairn_oplog WHERE pk = $1 AND op = 'delete'",
                    &[&pk],
                )
                .await
                .map_or(0, |r| r.get::<_, i64>(0));
            n > 0
        }
    })
    .await;
    assert!(gap_landed, "offline-gap ops never landed in cairn_oplog");

    // Reconnect: epoch matches + resume in-window ⇒ REPLAY (no snapshot).
    let replay = collect_frames(
        h.addr,
        &h.token,
        &subscribe_frame_with_epoch(
            "tasks",
            &[],
            Some(checkpoint),
            // ADR-0031 D2: this helper never sends `rules_checksum`, so it is a
            // legacy client — the server compares against the composed
            // (slot_epoch, rules_checksum) value, not the raw slot epoch.
            Some(compose_sync_epoch(
                server_epoch,
                ActiveRuleset::all_mode().checksum(),
            )),
        ),
        Duration::from_secs(4),
    )
    .await;

    // The deleted row's pk arrived as a delete op...
    let saw_delete = replay
        .iter()
        .any(|f| frame_op(f) == Some("delete") && frame_pk(f) == Some(&gap_del));
    // ...and the kept row arrived as an upsert...
    let saw_keep = replay.iter().any(|f| {
        frame_op(f).is_some() && frame_op(f) != Some("delete") && frame_pk(f) == Some(&gap_keep)
    });
    // ...and NO snapshot boundary frames (proving replay, not snapshot).
    let saw_snapshot = replay
        .iter()
        .any(|f| matches!(frame_type(f), Some("snapshot_begin" | "snapshot_end")));

    eprintln!(
        "replay frames: {} (delete={saw_delete}, keep={saw_keep}, snapshot={saw_snapshot})",
        replay.len()
    );
    // ADR-0025 delete-tenant follow-up (FIXED): under `ALTER TABLE tasks
    // REPLICA IDENTITY FULL` the DELETE's WAL record carries the full old row
    // image (incl. org_id). The op-log writer lifts the tenant from that old
    // image, so the tenant-filtered replay (`WHERE tenant_id = ?`) now MATCHES
    // deletes — saw_delete must be TRUE. Pre-fix this was NULL tenant → the
    // replay silently dropped the delete → ghost row on reconnect (the test
    // only asserted !saw_snapshot + !empty then). The kept-row upsert must
    // also arrive: the gap_landed wait blocks until the delete (highest lsn)
    // is persisted, so the lower-lsn keep-insert flushed in the same or an
    // earlier batch.
    assert!(
        saw_delete,
        "DELETE-REPLAY FAILED: the deleted row {gap_del} was NOT delivered as a delete op on \
         the tenant-filtered replay. Under REPLICA IDENTITY FULL the op-log writer must tag the \
         delete with the tenant lifted from the old row image. (frames: {replay:?})"
    );
    assert!(
        saw_keep,
        "KEEP-REPLAY FAILED: the kept row {gap_keep} was NOT delivered on replay (frames: {replay:?})"
    );
    assert!(
        !replay.is_empty(),
        "REPLAY FAILED: the reconnect delivered zero frames — the replay path didn't fire (frames: {replay:?})"
    );
    assert!(
        !saw_snapshot,
        "REPLAY FAILED: snapshot boundary frames arrived on a reconnect that should have replayed (frames: {replay:?})"
    );

    h.driver.abort();
    let _ = sql_client()
        .await
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
}

/// A reconnect whose resume_lsn is no longer in the op-log window (the gap aged
/// out / the op-log is empty) falls back to SNAPSHOT-RECONCILE (slice 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aged_out_checkpoint_falls_back_to_snapshot() {
    if nostos_infra::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `docker compose up -d` to run)");
        return;
    }
    // Init tracing so the op-log writer's flush warns + the replicator's
    // snapshot/read logs surface under --nocapture (diagnoses why cairn_oplog
    // stays empty: writer INSERT failure vs snapshot-read gap vs no events).
    let _ = tracing_subscriber::fmt()
        .with_env_filter("nostos=debug,info")
        .with_test_writer()
        .try_init();
    let tenant = uuid::Uuid::new_v4().to_string();
    let slot = format!("e2e_oplog_replay_aged_{}", std::process::id());

    let _initial = insert_task(&tenant, "e2e-oplog-replay-aged-initial").await;
    let h = harness(&tenant, &slot).await;
    let server_epoch = h.metrics.snapshot().slot_epoch;
    assert!(server_epoch >= 1);

    // LIVE kick (see test 1): a post-slot write triggers the replicator → op-log.
    let _kick = insert_task(&tenant, "e2e-oplog-replay-aged-kick").await;

    // First connect (snapshot) + capture the resume point.
    let _ = collect_frames(
        h.addr,
        &h.token,
        &subscribe_frame_with_epoch("tasks", &[], None, None),
        Duration::from_secs(2),
    )
    .await;
    let _landed = wait_for(Duration::from_secs(30), || {
        let tenant = tenant.clone();
        async move { oplog_max_lsn(&tenant).await > 0 }
    })
    .await;
    let checkpoint = u64::try_from(oplog_max_lsn(&tenant).await).unwrap_or(0);
    assert!(checkpoint > 0, "need a seeded checkpoint to age out");

    // Age out the ENTIRE op-log for this tenant ⇒ window_tail (MIN lsn) is now
    // NULL/0 ⇒ resume >= window_tail is TRUE but the replay returns EMPTY ⇒
    // the server falls back to snapshot-reconcile (slice 1, the safety net).
    {
        let c = sql_client().await;
        c.execute("DELETE FROM cairn_oplog WHERE tenant_id = $1", &[&tenant])
            .await
            .expect("age out op-log");
    }

    // Reconnect with the (now-aged-out) checkpoint + matching epoch.
    let frames = collect_frames(
        h.addr,
        &h.token,
        &subscribe_frame_with_epoch(
            "tasks",
            &[],
            Some(checkpoint),
            // ADR-0031 D2: this helper never sends `rules_checksum`, so it is a
            // legacy client — the server compares against the composed
            // (slot_epoch, rules_checksum) value, not the raw slot epoch.
            Some(compose_sync_epoch(
                server_epoch,
                ActiveRuleset::all_mode().checksum(),
            )),
        ),
        Duration::from_secs(4),
    )
    .await;

    let saw_snapshot_begin = frames
        .iter()
        .any(|f| frame_type(f) == Some("snapshot_begin"));
    eprintln!(
        "aged-out frames: {} (snapshot_begin={saw_snapshot_begin})",
        frames.len()
    );
    assert!(
        saw_snapshot_begin,
        "FALLBACK FAILED: an aged-out reconnect should fall back to snapshot-reconcile (slice 1), but no snapshot_begin arrived (frames: {frames:?})"
    );

    h.driver.abort();
    let _ = sql_client()
        .await
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
}
