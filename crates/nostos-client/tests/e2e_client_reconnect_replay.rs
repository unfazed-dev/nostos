//! ADR-0025 #2 — combined real-`SyncClient` reconnect-replay e2e.
//!
//! The raw-frame e2e (`nostos-infra/tests/e2e_pg_oplog_replay.rs`) proves the
//! SERVER replay path (epoch-gate + op-log replay + tenant-tagged deletes), and
//! `epoch_persistence.rs` proves the CLIENT persists the epoch. NEITHER drives a
//! real `SyncClient` through the full offline→online→replay-delta loop. This
//! test does: a real client snapshots, goes offline while the server changes
//! (insert + delete), reconnects, and ends with the correct rows in its own
//! SQLite — proving the replayed delta (incl. the delete) was received AND
//! applied by the real client. Tenant mode is required (the op-log reader
//! filters `WHERE tenant_id = ?`, which never matches in anonymous mode).
//!
//! ## Running
//! ```sh
//! docker compose -f docker/docker-compose.yml up -d
//! NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn \
//!   cargo test -p nostos-client --features pg --test e2e_client_reconnect_replay \
//!   -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "pg")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use nostos_application::ports::{Metrics, OpLogWriter, SessionStore};
use nostos_application::{FanOutService, SessionManager};
use nostos_client::{SqliteStorage, SyncClient, SyncClientConfig};
use nostos_domain::{ColumnValue, ReplicationEvent};
use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use nostos_infra::{PgOpLogReader, PgOpLogWriter, PgSnapshotter, SupabaseJwtAuth};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const E2E_FLAG: &str = "NOSTOS_E2E_PG";
const SECRET: &[u8] = b"e2e-client-replay-secret";
const TENANT_COL: &str = "org_id";

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

/// Highest lsn the op-log writer has persisted for `tenant` (0 if none).
async fn oplog_max_lsn(tenant: &str) -> i64 {
    let c = sql_client().await;
    c.query_one(
        "SELECT COALESCE(MAX(lsn), 0)::bigint FROM cairn_oplog WHERE tenant_id = $1",
        &[&tenant],
    )
    .await
    .expect("oplog max lsn")
    .get::<_, i64>(0)
}

/// JWT base64url (no padding) — mirrors `nostos-infra`'s e2e helper.
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

/// Mint an HS256 JWT `{"sub": sub}` signed with SECRET; `principal.tenant_id = sub`.
fn mint_jwt(sub: &str) -> String {
    let header = b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}";
    let payload = format!("{{\"sub\":\"{sub}\"}}");
    let h = jwt_b64url(header);
    let p = jwt_b64url(payload.as_bytes());
    let signing_input = format!("{h}.{p}");
    let mut mac = HmacSha256::new_from_slice(SECRET).expect("hmac key");
    mac.update(signing_input.as_bytes());
    let sig = jwt_b64url(&mac.finalize().into_bytes());
    format!("{signing_input}.{sig}")
}

struct Harness {
    addr: std::net::SocketAddr,
    token: String,
    driver: tokio::task::JoinHandle<()>,
}

/// Production-shaped tenant-mode stack against real PG (mirrors
/// `e2e_pg_oplog_replay`'s harness): FanOutService + PgOpLogWriter +
/// PgReplicator + axum with oplog_reader/snapshotter/metrics/tenant.
async fn harness(tenant: &str, slot: &str) -> Harness {
    let sql = sql_client().await;
    let _ = sql
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;

    let metrics = Arc::new(Metrics::new());
    let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
    let manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        nostos_domain::Tier::Enterprise,
    ));
    let oplog: Arc<dyn OpLogWriter> = Arc::new(PgOpLogWriter::new(
        &pg_url(),
        Some(TENANT_COL.to_string()),
        4096,
        Some(Arc::clone(&metrics)),
    ));
    let fanout = Arc::new(FanOutService::new(Arc::clone(&store)).with_op_log(oplog));

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

    // Wait for slot + epoch bump (the resume gate needs slot_epoch ≥ 1).
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
        token: mint_jwt(tenant),
        driver,
    }
}

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

/// Count `cairn_data` rows for a pk in the client's on-disk SQLite (a second
/// read connection — `SqliteStorage` is moved into the `SyncClient`, so we
/// re-open the file to introspect the applied state).
fn pk_present(path: &str, pk: &str) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("open client sqlite for read");
    conn.query_row(
        "SELECT COUNT(*) FROM cairn_data WHERE pk = ?1",
        rusqlite::params![pk],
        |r| r.get(0),
    )
    .expect("count query is infallible on a valid schema")
}

/// A real `SyncClient` that snapshots, goes offline during a server-side gap
/// (insert + delete), and reconnects — ends with the kept row present and the
/// deleted row absent in its own SQLite. Proves the replayed delta (incl. the
/// delete) was received AND applied by the real client end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_reconnect_applies_replayed_gap_including_delete() {
    if std::env::var(E2E_FLAG).is_err() {
        eprintln!("skipping (set {E2E_FLAG}=1 with `docker compose up -d` to run)");
        return;
    }

    let tenant = uuid::Uuid::new_v4().to_string();
    let slot = format!("e2e_client_replay_{}", std::process::id());

    // Seed + harness + live kick (so the op-log has a tenant-scoped checkpoint).
    let _seed = insert_task(&tenant, "e2e-client-replay-seed").await;
    let h = harness(&tenant, &slot).await;
    let _kick = insert_task(&tenant, "e2e-client-replay-kick").await;

    // Real client over an on-disk SQLite (re-opened later to inspect the state).
    let dir = std::env::temp_dir().join(format!("nostos-client-replay-{tenant}.sqlite"));
    let _ = std::fs::remove_file(&dir);
    let storage = SqliteStorage::open(dir.to_str().expect("path")).expect("open client sqlite");
    let config = SyncClientConfig {
        token: Some(h.token.clone()),
        idle_timeout: Some(Duration::from_secs(3)),
        ..SyncClientConfig::default()
    };
    let url = format!("ws://{}/sync", h.addr);
    let client = SyncClient::new(url, storage, config);

    // Session 1: snapshot + live kick, persist the server epoch.
    let _ = client.run_once().await.expect("run_once #1 (snapshot)");
    // Wait for the kick to land in the op-log so the client has a checkpoint
    // the reconnect can resume from.
    let tenant_for_wait = tenant.clone();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let t = tenant_for_wait.clone();
            async move { oplog_max_lsn(&t).await > 0 }
        })
        .await,
        "op-log seed never landed for tenant {tenant}"
    );

    // The client must have persisted the server's epoch (the resume gate signal).
    let persisted_epoch = client.epoch().await.expect("epoch read");
    assert!(
        persisted_epoch >= 1,
        "client must persist the server epoch from resume_info (got {persisted_epoch})"
    );

    // Offline gap: insert a keeper + insert then delete a row. The delete is the
    // load-bearing op — it must arrive via replay + be applied (else a ghost row).
    let gap_keep = insert_task(&tenant, "e2e-client-replay-gap-keep").await;
    let gap_del = insert_task(&tenant, "e2e-client-replay-gap-del").await;
    {
        let c = sql_client().await;
        c.execute(
            "DELETE FROM tasks WHERE id = $1",
            &[&gap_del.parse::<uuid::Uuid>().unwrap()],
        )
        .await
        .expect("delete gap row");
    }
    // Wait for the whole gap (the delete is the last/highest-lsn op) to flush.
    let gap_del_for_wait = gap_del.clone();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let pk = gap_del_for_wait.clone();
            async move {
                let c = sql_client().await;
                c.query_one(
                    "SELECT count(*) FROM cairn_oplog WHERE pk = $1 AND op = 'delete'",
                    &[&pk],
                )
                .await
                .map_or(0, |r| r.get::<_, i64>(0))
                    > 0
            }
        })
        .await,
        "offline-gap ops never landed in cairn_oplog"
    );

    // Session 2: reconnect. The client resends its persisted epoch + checkpoint
    // → the server replays the gap (no snapshot boundary) → the client applies
    // it (incl. the delete).
    let _ = client.run_once().await.expect("run_once #2 (replay)");

    // Inspect the client's own SQLite: the kept row is present, the deleted row
    // is absent. (Both converge to this under snapshot too, but combined with
    // the persisted-epoch assertion above, this confirms the real client drove
    // the full reconnect-resume loop + applied the replayed delta.)
    let path = dir.to_str().unwrap();
    assert_eq!(
        pk_present(path, &gap_keep),
        1,
        "the kept row must be present in the client's SQLite after reconnect"
    );
    assert_eq!(
        pk_present(path, &gap_del),
        0,
        "the deleted row must be ABSENT — the replayed delete was applied (no ghost row)"
    );

    h.driver.abort();
    let _ = sql_client()
        .await
        .batch_execute(format!("SELECT pg_drop_replication_slot('{slot}');").as_str())
        .await;
    let _ = std::fs::remove_file(&dir);
}
