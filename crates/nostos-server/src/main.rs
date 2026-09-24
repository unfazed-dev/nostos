//! # nostos-server
//!
//! The composition root. Reads config, constructs the concrete adapters
//! (`InMemorySessionStore`, `FakeReplicator`/`PgReplicator`, WebSocket
//! transport), injects them into the application-layer use-cases
//! (`SessionManager`, `FanOutService`), and binds axum.
//!
//! This is the **only** binary that knows how the adapters are wired —
//! swapping `FakeReplicator` for `PgReplicator` (the `NOSTOS_REPLICATOR` env) is
//! a one-line change here, with zero edits to domain/application code. That's
//! the hexagonal payoff (ADR-0001).

mod admin_auth;
mod all_mode_warning;
mod config;
mod cors;
mod endpoints;
mod ingest;
mod push_api;
mod push_config;
mod rules;
mod telemetry;
mod typed_column;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::routing::get;
use nostos_application::ports::{SchemaSource, TableStat};
use nostos_application::{FanOutService, SessionManager};
use nostos_domain::{ColumnValue, ReplicationEvent, SyncMode};
use nostos_infra::replicator::{FakeReplicator, FakeReplicatorConfig};
use nostos_infra::store::InMemorySessionStore;
use nostos_infra::transport::{sync_handler, SyncRouterState};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::all_mode_warning::format_all_mode_warning;
use crate::config::{eviction_policy, exposes_anonymous_sync, Config};
use crate::cors::{build_cors_layer, parse_origin_list};
use crate::endpoints::{healthz, metrics_handler, schema, PROTECT_METADATA};
use crate::push_config::{parse_push_tables, push_wiring, resolve_tenant_col, PushWiring};
use crate::rules::{put_rules_handler, rules_handler, watch_rules};
use crate::telemetry::{init_tracing, redacted_request_span};
use crate::typed_column::extract_typed_column;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cfg = nostos_infra::env::parse::<Config>();
    // ADR-0046: resolve the default once, so boot load, the reload poll and
    // PUT /rules all use the same (possibly pre-rename) file.
    if cfg.rules_file == nostos_infra::rules_file::RULES_FILE_NAME {
        let resolved = nostos_infra::rules_file::path_in(std::path::Path::new(""));
        cfg.rules_file = resolved.display().to_string();
    }
    init_tracing(&cfg.log);

    // ---- admin auth (Task 21, ADR-0031 addendum): NOSTOS_ADMIN_TOKEN gates
    // PUT /rules. Env-only by design (NOT a clap flag), same reasoning as
    // NOSTOS_LICENSE_SECRET below: it must never land on argv / `ps`. Unset
    // -> the route 404s (fail-closed, see admin_auth.rs). Set-but-short ->
    // refuse to start rather than serve a guessable admin route; the
    // message reports the length only, never the token itself.
    if let Some(token) = admin_auth::admin_token_from_env() {
        let len = token.expose().len();
        if len < admin_auth::MIN_ADMIN_TOKEN_LEN {
            anyhow::bail!(
                "NOSTOS_ADMIN_TOKEN is set but only {len} chars (minimum {}) — refusing to \
                 start rather than serve a guessable admin route on PUT /rules",
                admin_auth::MIN_ADMIN_TOKEN_LEN
            );
        }
        info!("admin auth: NOSTOS_ADMIN_TOKEN set — PUT /rules enabled");
    } else {
        info!("admin auth: NOSTOS_ADMIN_TOKEN unset — PUT /rules disabled (404)");
    }

    // ---- construct the adapters (the infra ring) ----
    // Coerce to the trait object once so both use-cases share one store.
    let store: Arc<dyn nostos_application::ports::SessionStore> =
        Arc::new(InMemorySessionStore::new());

    // ---- inject into the application use-cases ----
    // The licensed tier gates the concurrent-device cap. OSS self-host defaults
    // to Enterprise (unlimited); a managed deploy sets NOSTOS_TIER=hobby|pro|scale.
    // ---- resolve the licensed tier (ADR-0006 trust boundary) ----
    // OSS self-host: no NOSTOS_LICENSE → fall back to NOSTOS_TIER (default
    // `enterprise` = unlimited). Managed deploy: presents a signed
    // NOSTOS_LICENSE; the token's tier + device_cap are authoritative, and a
    // presented-but-invalid token is FATAL (no silent downgrade to OSS default).
    let fallback_tier = match cfg.tier.as_str() {
        "hobby" => nostos_domain::Tier::Hobby,
        "pro" => nostos_domain::Tier::Pro,
        "scale" => nostos_domain::Tier::Scale,
        _ => nostos_domain::Tier::Enterprise,
    };
    // NOSTOS_LICENSE_SECRET is env-only by design (NOT a clap flag): it signs
    // every license a cloud deploy mints, so it must never land on argv / `ps`.
    let license_secret = nostos_infra::env::var("NOSTOS_LICENSE_SECRET").unwrap_or_default();
    let entitlement =
        nostos_license::resolve_entitlement(&cfg.license, license_secret.as_bytes(), fallback_tier)
            .context("NOSTOS_LICENSE verification failed — refusing to start")?;
    info!(
        tier = ?entitlement.tier,
        devices_cap = entitlement.device_cap,
        project_id = %entitlement.project_id,
        licensed = !cfg.license.is_empty(),
        "entitlement resolved"
    );
    let manager = Arc::new(
        SessionManager::with_device_cap(
            Arc::clone(&store),
            entitlement.tier,
            entitlement.device_cap,
        )
        .with_per_principal_cap(cfg.per_principal_session_cap),
    );

    // ---- /sync authentication (ADR-0010) ----
    // The OSS self-host default is `none` (anonymous — single-tenant dev). A
    // managed deploy sets `supabase-jwt` + the secret; without it the server
    // cannot enforce tenant scoping and refuses to inject predicates.
    let auth: Arc<dyn nostos_application::ports::SyncAuth> = match cfg.sync_auth.as_str() {
        "supabase-jwt" => {
            // Config resolution (ADR-0010 addendum): at least one of the
            // legacy HS256 secret or a JWKS source must be set. Explicit
            // NOSTOS_SUPABASE_JWKS_URL wins over one derived from
            // NOSTOS_SUPABASE_URL. Both a secret and a JWKS source may be set
            // at once — each token's header `alg` then picks the verifier
            // (HS256 -> secret, RS256/ES256/EdDSA -> JWKS); neither is ever
            // checked against the other's key material.
            let secret = (!cfg.supabase_jwt_secret.is_empty())
                .then(|| cfg.supabase_jwt_secret.as_bytes().to_vec());
            let jwks_url = if !cfg.supabase_jwks_url.is_empty() {
                Some(cfg.supabase_jwks_url.clone())
            } else if !cfg.supabase_url.is_empty() {
                Some(format!(
                    "{}/auth/v1/.well-known/jwks.json",
                    cfg.supabase_url.trim_end_matches('/')
                ))
            } else {
                None
            };
            if secret.is_none() && jwks_url.is_none() {
                anyhow::bail!(
                    "NOSTOS_SYNC_AUTH=supabase-jwt requires at least one of \
                     NOSTOS_SUPABASE_JWT_SECRET (legacy HS256) or \
                     NOSTOS_SUPABASE_URL/NOSTOS_SUPABASE_JWKS_URL (RS256/ES256/EdDSA)"
                );
            }
            info!(
                hs256 = secret.is_some(),
                jwks = jwks_url.is_some(),
                "sync auth: supabase-jwt (tenant-enforced)"
            );
            Arc::new(
                nostos_infra::SupabaseJwtAuth::from_config(secret, jwks_url)
                    .with_allow_missing_exp(cfg.allow_jwt_without_exp)
                    .with_issuers(
                        cfg.jwt_issuers
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string)
                            .collect(),
                    )
                    .with_jwks_max_stale(std::time::Duration::from_secs(cfg.jwks_max_stale_secs)),
            )
        }
        "bearer" => {
            if cfg.sync_bearer_token.is_empty() {
                anyhow::bail!(
                    "NOSTOS_SYNC_AUTH=bearer requires NOSTOS_SYNC_BEARER_TOKEN \
                     (the shared secret every /sync connection must present)"
                );
            }
            info!(
                "sync auth: static bearer — one fixed principal (single-tenant \
                 self-host; push registration enabled)"
            );
            Arc::new(nostos_infra::StaticBearerAuth::new(&cfg.sync_bearer_token))
        }
        "none" => {
            // Fail-open pair guard. A `warn!` is not enforcement — it scrolls
            // past in container logs, and the deploy that needed to read it is
            // exactly the one nobody is watching. Refuse the PAIR instead:
            // anonymous auth reachable from somewhere that is not this
            // machine. Parsed leniently on purpose — an unparseable bind is
            // not this check's error to report, and the canonical
            // "invalid bind address" bail follows a few hundred lines down.
            if !cfg.insecure_anonymous && exposes_anonymous_sync(&cfg.bind) {
                anyhow::bail!(
                    "refusing to start: NOSTOS_SYNC_AUTH=none (no authentication, no \
                     tenant filter) with NOSTOS_BIND={} (reachable off-host). Pick one: \
                     set NOSTOS_SYNC_AUTH=supabase-jwt (or bearer) for a real deploy; \
                     set NOSTOS_BIND=127.0.0.1:8800 to keep it local; or set \
                     NOSTOS_INSECURE_ANONYMOUS=1 if something in front of this server \
                     is doing the authenticating.",
                    cfg.bind
                );
            }
            warn!(
                "sync auth: NONE — /sync is unauthenticated. Single-tenant/dev only; \
                 set NOSTOS_SYNC_AUTH=supabase-jwt for any multi-tenant deploy."
            );
            Arc::new(nostos_infra::AllowAnonymous::new())
        }
        other => anyhow::bail!(
            "unknown NOSTOS_SYNC_AUTH value: {other} (expected 'none', 'bearer', or 'supabase-jwt')"
        ),
    };

    // Aggregate metrics shared between the fan-out service (writer) and the
    // /metrics endpoint (reader). The session gauge is updated on connect/
    // disconnect by the manager — for now we snapshot the store count on read.
    let metrics = Arc::new(nostos_application::ports::Metrics::new());
    // WAL-bloat protection: ON by default at 1 GiB since the v0.2.0 audit
    // (finding 1, ADR-0043). `NOSTOS_SLOT_MAX_LAG=0` opts back OUT, which
    // restores the old behaviour where a client that acks nothing pins the
    // slot forever (ADR-0016) — loud, never silent.
    let eviction = eviction_policy(cfg.slot_max_lag);
    if eviction.max_lag.is_none() {
        warn!(
            "NOSTOS_SLOT_MAX_LAG=0: WAL-bloat eviction is OFF — a client that never acks pins the \
             replication slot and grows WAL on the primary without bound (disk exhaustion). \
             Set NOSTOS_SLOT_MAX_LAG (default 1073741824 = 1 GiB) and Postgres \
             max_slot_wal_keep_size / NOSTOS_PG_SLOT_WAL_KEEP_SIZE (ADR-0043)."
        );
    }
    // Op-log writer (ADR-0025 slice 2): persisted op-log for in-window
    // reconnect replay. Only under `NOSTOS_REPLICATOR=pg` — the fake replicator
    // has no source database to durably write to (the bench drives a
    // RecordingOpLogWriter directly). Shares the metrics handle so `/metrics`
    // surfaces the drop + flush-failure counters.
    #[cfg(feature = "pg")]
    let op_log: Option<Arc<dyn nostos_application::ports::OpLogWriter>> = if cfg.replicator == "pg"
    {
        Some(Arc::new(nostos_infra::PgOpLogWriter::new(
            &cfg.pg_url,
            Some(cfg.tenant_column.clone()),
            cfg.oplog_buffer,
            Some(Arc::clone(&metrics)),
        )))
    } else {
        None
    };
    // Retain a clone of the op-log writer so we can drain it on graceful
    // shutdown (after axum returns) — the original Arc moves into the
    // FanOutService below. ADR-0025 slice-6 follow-up.
    #[cfg(feature = "pg")]
    let op_log_shutdown = op_log.clone();

    // Op-log compactor (ADR-0025 slice 5): bounds cairn_oplog growth via
    // periodic collapse (keep latest op per (table_name, pk) — a trailing
    // delete survives as a tombstone) + retention (age out old rows). Only
    // under NOSTOS_REPLICATOR=pg. Detached background task (runs until process
    // exit). The compactor's swept-row count surfaces in `/metrics`.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let _compactor = nostos_infra::PgOpLogCompactor::new(
            &cfg.pg_url,
            cfg.oplog_retention_secs,
            cfg.oplog_compact_interval_secs,
            Arc::clone(&metrics),
        );
    }

    // ---- tenant column (used by the push wiring below AND the WS transport
    // state further down). Tenant column is enforced only under supabase-jwt
    // auth — the anonymous mode has no principal to scope with (see
    // ADR-0011). An *empty* `NOSTOS_TENANT_COLUMN=` is the explicit opt-out
    // (single-tenant deploys scoping per-table via nostos_rules.toml
    // instead): before this guard, the empty string was passed through as a
    // real column name, injecting `"" = <tenant>` into every predicate — a
    // column no row has, so every authenticated subscription silently
    // snapshot/streamed zero rows.
    // Fail-closed config gate (audit 2026-08-17 M1): `tenant_column == "id"`
    // INVERTS the write-path upsert guard into a tautology — the ON CONFLICT
    // guard becomes `WHERE "id" = EXCLUDED."id"`, true by definition on
    // any conflict, so a cross-tenant upsert silently overwrites the victim's
    // row (ADR-0018 bypassed; patch/delete still fail closed, which is what
    // made the old comment's fail-closed story look right). The v1 PK
    // convention is the column named "id", so that name can never be a
    // tenant column: bail loudly at boot instead of shipping the inversion.
    if cfg.sync_auth == "supabase-jwt" && cfg.tenant_column == "id" {
        anyhow::bail!(
            "NOSTOS_TENANT_COLUMN=id is invalid: \"id\" is the primary-key column \
             (v1 convention), and using it as the tenant column silently \
             disables cross-tenant upsert protection — pick a real tenant \
             column (e.g. org_id) or set NOSTOS_TENANT_COLUMN empty to opt out"
        );
    }
    let tenant_col = resolve_tenant_col(&cfg.sync_auth, &cfg.tenant_column);
    if cfg.sync_auth == "supabase-jwt" && cfg.tenant_column.is_empty() {
        tracing::info!(
            "NOSTOS_TENANT_COLUMN is empty — tenant scoping disabled; \
             use nostos_rules.toml scopes for per-table row filtering"
        );
    }

    // ---- ADR-0037 push doorbell (plan 1.3 + 2.4) ----
    // Rails from env (`from_env` per rail: `Ok(None)` = unconfigured); the
    // per-table config from NOSTOS_PUSH_TABLES; the token registry — Pg under
    // pg mode, in-memory otherwise so the REST surface still works in dev
    // builds (no persistence across restarts in fake mode).
    let push_cfg =
        parse_push_tables(&cfg.push_tables, tenant_col).context("invalid NOSTOS_PUSH_TABLES")?;
    let rails = nostos_infra::RailSet::from_env().context("push rail configuration")?;
    #[cfg(feature = "pg")]
    let push_registry: std::sync::Arc<dyn nostos_infra::PushTokenRegistry> =
        if cfg.replicator == "pg" {
            info!("push tokens: PgTokenStore (real registry)");
            Arc::new(nostos_infra::PgTokenStore::new(&cfg.pg_url))
        } else {
            info!(
            "push tokens: in-memory registry (fake replicator — registrations are not persisted)"
        );
            Arc::new(nostos_infra::InMemoryTokenRegistry::new())
        };
    #[cfg(not(feature = "pg"))]
    let push_registry: std::sync::Arc<dyn nostos_infra::PushTokenRegistry> =
        Arc::new(nostos_infra::InMemoryTokenRegistry::new());
    // The notifier — PRECEDENCE (ADR-0038 §3, plan task 2.3); the decision
    // itself is [`push_wiring`] below so the precedence is unit-tested.
    let wiring = push_wiring(
        &cfg.push_remote_url,
        &cfg.push_remote_key,
        !(rails.is_empty() && push_cfg.tables.tables.is_empty()),
    )
    .map_err(anyhow::Error::msg)
    .context("invalid push delegation config")?;
    let push_notifier: Arc<dyn nostos_application::ports::PushNotifier> = if wiring
        == PushWiring::Remote
    {
        info!(
            url = %cfg.push_remote_url,
            tables = push_cfg.tables.tables.len(),
            live_activity_tables = push_cfg.live_activities.len(),
            "push: RemoteNotifier delegation to nostos-pushd active (embedded PushRouter skipped)"
        );
        let receipts_state = (!cfg.push_remote_state_path.trim().is_empty())
            .then(|| std::path::PathBuf::from(cfg.push_remote_state_path.clone()));
        Arc::new(nostos_infra::push::remote::RemoteNotifier::new(
            &cfg.push_remote_url,
            &cfg.push_remote_key,
            Arc::clone(&push_registry),
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_cfg.tables.clone(),
                live_activities: push_cfg.live_activities.clone(),
            },
            Arc::clone(&metrics),
            receipts_state,
        ))
    } else if wiring == PushWiring::Noop {
        info!("push: off (no rails configured, no NOSTOS_PUSH_TABLES)");
        Arc::new(nostos_application::ports::NoopNotifier)
    } else {
        if rails.is_empty() {
            warn!(
                "NOSTOS_PUSH_TABLES is set but no push rail is configured — \
                     hints enqueue but no provider can deliver"
            );
        }
        info!(
            tables = push_cfg.tables.tables.len(),
            live_activity_tables = push_cfg.live_activities.len(),
            debounce_ms = cfg.push_debounce_ms,
            "push: PushRouter coalescer active"
        );
        Arc::new(nostos_infra::PushRouter::new(
            Arc::new(rails),
            Arc::clone(&push_registry),
            Arc::clone(&store),
            nostos_infra::push::router::RouterConfig {
                tables: push_cfg.tables.clone(),
                live_activities: push_cfg.live_activities,
            },
            std::time::Duration::from_millis(cfg.push_debounce_ms),
            Arc::clone(&metrics),
        ))
    };

    let fanout = Arc::new({
        let builder = FanOutService::new(Arc::clone(&store))
            .with_metrics(Arc::clone(&metrics))
            .with_eviction(eviction);
        // Attach the op-log when built (cfg-gated so the non-pg build never
        // references the (absent) PgOpLogWriter type).
        #[cfg(feature = "pg")]
        let builder = match op_log {
            Some(w) => builder.with_op_log(w),
            None => builder,
        };
        builder
            .with_ack_progress_every(cfg.ack_progress_interval)
            .with_push_tables(push_cfg.tables)
            .with_push_notifier(push_notifier)
    });

    // ---- start the replicator → fan-out driver ----
    // The extractor lifts named columns out of an event's payload so predicates
    // (which match on column equality) can be evaluated. For the FakeReplicator
    // the payload is opaque bytes, so we return `Any` (table-only matching). For
    // the PgReplicator the payload is a small JSON object {col:val}, so we parse
    // it and return real values — enabling filter predicates like org_id=acme.
    //
    // C6 (late-append P1): retain the pg replicator's JoinHandle so we can
    // abort it BEFORE draining the op-log at shutdown — prevents the silent
    // late-append-into-final-flush race (ghost row on matching-epoch reconnect;
    // see oplog.rs `drain_boundary_late_append_during_final_flush_is_lost`).
    // The fake branch intentionally detaches (`mem::forget`): it's the bench
    // path, the op-log is pg-only, and FakeReplicator has no producer window.
    #[cfg(feature = "pg")]
    let mut repl_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Driver-liveness flag (M6): flipped when the replicator→fan-out driver
    // task exits on its own; folded into /healthz. Wired into state below.
    let driver_dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // B2 mirror (ADR-0042): the ingest half of the channel replicator, built
    // in the `mirror` arm below and handed to the /ingest route mount. The
    // handle implements SnapshotSource, so the snapshotter injection reuses
    // the same object — one buffer, two views.
    let mut mirror_ingest: Option<ingest::IngestState> = None;
    match cfg.replicator.as_str() {
        "fake" => {
            let mut repl = FakeReplicator::new(
                FakeReplicatorConfig::small(u64::MAX)
                    .paced(cfg.fake_events_per_sec)
                    .recycling_keys(cfg.fake_distinct_keys),
            );
            let fanout_drv = Arc::clone(&fanout);
            let dead = Arc::clone(&driver_dead);
            let drv = tokio::spawn(async move {
                let extract = |_e: &ReplicationEvent, _col: &str| -> Option<ColumnValue> {
                    Some(ColumnValue::Any)
                };
                let outcome = fanout_drv.run(&mut repl, extract).await;
                tracing::error!(
                    ?outcome,
                    "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                );
                dead.store(true, std::sync::atomic::Ordering::Relaxed);
                info!(?outcome, "replicator stream ended");
            });
            std::mem::forget(drv);
            info!(
                events_per_sec = cfg.fake_events_per_sec,
                distinct_keys = cfg.fake_distinct_keys,
                "replicator: FakeReplicator (synthetic; 0 = unbounded)"
            );
        }
        "mirror" => {
            // B2 (ADR-0042): the desktop-sidecar mirror — the engine is the
            // single writer and POSTs row events to /ingest; this arm drains
            // the channel into the SAME fan-out pipeline (the adapter-swap
            // payoff main.rs:8-11 advertises). No PG anywhere: the snapshot
            // comes from the handle's in-memory buffer.
            if !cfg.pg_url.trim().is_empty() {
                anyhow::bail!(
                    "NOSTOS_REPLICATOR=mirror but NOSTOS_PG_URL is set — the mirror \
                     keeps its own in-memory read model and must not point at \
                     Postgres. Unset NOSTOS_PG_URL."
                );
            }
            let (handle, mut repl) = nostos_infra::MirrorHandle::open();
            mirror_ingest = Some(ingest::IngestState { handle });
            let fanout_drv = Arc::clone(&fanout);
            let dead = Arc::clone(&driver_dead);
            let drv = tokio::spawn(async move {
                // Mirror payloads are the engine's tuple-image JSON — same
                // typed extraction as the pg path (ADR-0037 plan 1.4).
                let extract =
                    |e: &ReplicationEvent, col: &str| extract_typed_column(e.payload_bytes(), col);
                let outcome = fanout_drv.run(&mut repl, extract).await;
                tracing::error!(
                    ?outcome,
                    "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                );
                dead.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            std::mem::forget(drv);
            info!("replicator: MirrorHandle (engine mirror-out via POST /ingest)");
        }
        "pg" => {
            #[cfg(feature = "pg")]
            {
                use nostos_infra::replicator::{PgReplicator, PgReplicatorConfig};
                if cfg.pg_url.trim().is_empty() {
                    anyhow::bail!(
                        "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set. \
                         Set NOSTOS_PG_URL, e.g. after: \
                         docker compose -f docker/docker-compose.yml up -d"
                    );
                }
                let mut pg_cfg =
                    PgReplicatorConfig::from_url(&cfg.pg_url, &cfg.pg_slot, &cfg.pg_publication)
                        .context("invalid NOSTOS_PG_URL")?;
                pg_cfg.max_slot_wal_keep_size_mb = cfg.pg_slot_wal_keep_size;
                if cfg.pg_slot_wal_keep_size > 0 {
                    info!(
                        keep_size_mb = cfg.pg_slot_wal_keep_size,
                        "WAL-bloat backstop: will set max_slot_wal_keep_size on the slot"
                    );
                }
                let mut repl = PgReplicator::new(pg_cfg).with_metrics(Arc::clone(&metrics));
                let fanout_drv = Arc::clone(&fanout);
                let dead = Arc::clone(&driver_dead);
                let drv = tokio::spawn(async move {
                    // Extract a column from the JSON payload: parse the small
                    // object and return the named field. Typed (ADR-0037 plan
                    // 1.4): JSON scalars keep their type — a bare `5` yields
                    // `Number(5)`, not "absent" — so predicates over numeric/
                    // bool columns no longer match wider than intended.
                    let extract = |e: &ReplicationEvent, col: &str| {
                        extract_typed_column(e.payload_bytes(), col)
                    };
                    let outcome = fanout_drv.run(&mut repl, extract).await;
                    tracing::error!(
                        ?outcome,
                        "replicator→fan-out driver EXITED — live fan-out stopped; /healthz now degraded (M6)"
                    );
                    dead.store(true, std::sync::atomic::Ordering::Relaxed);
                });
                repl_handle = Some(drv);
                info!(
                    slot = %cfg.pg_slot,
                    publication = %cfg.pg_publication,
                    "replicator: PgReplicator (real Postgres logical replication)"
                );
            }
            #[cfg(not(feature = "pg"))]
            {
                warn!(
                    "NOSTOS_REPLICATOR=pg but this binary was built without the `pg` feature. \
                     Rebuild with `cargo build -p nostos-server --features pg`. Falling back to fake."
                );
                let mut repl = FakeReplicator::new(
                    FakeReplicatorConfig::small(u64::MAX)
                        .paced(cfg.fake_events_per_sec)
                        .recycling_keys(cfg.fake_distinct_keys),
                );
                let fanout_drv = Arc::clone(&fanout);
                let drv = tokio::spawn(async move {
                    let extract = |_e: &ReplicationEvent, _col: &str| Some(ColumnValue::Any);
                    let outcome = fanout_drv.run(&mut repl, extract).await;
                    info!(?outcome, "replicator stream ended (fallback fake)");
                });
                std::mem::forget(drv);
            }
        }
        other => {
            anyhow::bail!(
                "unknown NOSTOS_REPLICATOR value: {other} (expected 'fake', 'pg', or 'mirror')"
            );
        }
    }

    // ---- build the axum router + transport ----
    PROTECT_METADATA.store(cfg.protect_metadata, std::sync::atomic::Ordering::Relaxed);
    if cfg.protect_metadata {
        if cfg.sync_auth == "none" {
            // Say it plainly rather than let the operator believe the knob did
            // something: `AllowAnonymous` accepts the empty token, so the
            // /schema gate admits everyone. /rules is still genuinely gated —
            // it checks the admin token, which has nothing to do with sync auth.
            warn!(
                "NOSTOS_PROTECT_METADATA is set but NOSTOS_SYNC_AUTH=none — \
                 GET /schema stays effectively open (anonymous auth accepts any \
                 caller); only GET /rules is actually protected"
            );
        } else {
            info!("metadata protection: GET /schema requires sync auth, GET /rules requires NOSTOS_ADMIN_TOKEN");
        }
    } else {
        info!("metadata protection: off (NOSTOS_PROTECT_METADATA unset) — GET /schema and GET /rules are unauthenticated");
    }
    let ws_origins = parse_origin_list(&cfg.ws_origins);
    if ws_origins.is_empty() {
        info!("ws origin check: NOSTOS_WS_ORIGINS unset — /sync accepts any origin");
    } else {
        info!(origins = ?ws_origins, "ws origin check: /sync restricted to these browser origins");
    }
    let mut state_builder = SyncRouterState::new(Arc::clone(&manager), Arc::clone(&auth))
        .with_buffer(cfg.session_buffer)
        .with_resync_signal(cfg.resync_signal)
        .with_allowed_origins(ws_origins)
        .with_metrics(Arc::clone(&metrics));
    if let Some(col) = tenant_col {
        state_builder = state_builder.with_tenant_column(col);
    }

    // ---- sync-rules ruleset (ADR-0031) ----
    // A malformed/invalid file must not silently degrade to "sync everything"
    // — bail loudly instead of falling back.
    let ruleset = match nostos_infra::rules_file::load(std::path::Path::new(&cfg.rules_file)) {
        Ok(Some(raw)) => {
            // P5: name the loaded streams at boot — the boot-time template
            // validation (design §2) is only observable if the operator can
            // SEE which stream definitions passed it.
            let stream_names: Vec<&str> = raw.streams.iter().map(|s| s.name.as_str()).collect();
            let stream_count = raw.streams.len();
            let compiled = nostos_application::ActiveRuleset::compile(&raw)
                .context("nostos_rules.toml failed to compile")?;
            info!(
                sync_mode = compiled.mode().as_str(),
                tables = compiled.synced_tables().len(),
                streams = stream_count,
                stream_names = ?stream_names,
                checksum = format!("{:x}", compiled.checksum()),
                "sync rules loaded"
            );
            compiled
        }
        Ok(None) => {
            info!("no nostos_rules.toml found; sync_mode=all (zero-config default)");
            nostos_application::ActiveRuleset::all_mode()
        }
        Err(e) => return Err(e).context("failed to load nostos_rules.toml"),
    };
    let ruleset_mode = ruleset.mode();
    // Captured before the ruleset moves into `rules_shared`: the boot-time
    // tenant-column audit (below) needs the synced-table list once.
    let rules_tables: Vec<String> = ruleset
        .synced_tables()
        .into_iter()
        .map(str::to_string)
        .collect();
    let (rules_tx, rules_changed) = tokio::sync::watch::channel(ruleset.checksum());
    let rules_shared = Arc::new(tokio::sync::RwLock::new(ruleset));
    state_builder = state_builder
        .with_rules(Arc::clone(&rules_shared), rules_changed, rules_tx.clone())
        .with_rules_file_path(std::path::PathBuf::from(&cfg.rules_file));

    // ---- `all`-mode startup warning (ADR-0031, Task 13) ----
    // sync_mode = "all" means every replicated row reaches every authorised
    // client (still tenant-scoped — see the principal-scoping path — but
    // unscoped within a tenant). That's the right zero-config default, but an
    // operator who never opts into narrower rules deserves a loud heads-up
    // rather than finding out from an OOM. Row counts are estimates
    // (`pg_class.reltuples`, never `count(*)`) so this stays cheap at boot.
    if ruleset_mode == SyncMode::All {
        let stats: Vec<TableStat> = {
            #[cfg(feature = "pg")]
            {
                if cfg.replicator == "pg" {
                    use nostos_application::ports::TableStatsSource;
                    let src = nostos_infra::PgTableStats::new(&cfg.pg_url, &cfg.pg_publication);
                    match src.table_stats().await {
                        Ok(stats) => stats,
                        Err(e) => {
                            // A stats-fetch failure must not abort boot — the
                            // warning still fires, just without numbers.
                            warn!("could not estimate table sizes: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    // The fake replicator has no database to introspect.
                    Vec::new()
                }
            }
            #[cfg(not(feature = "pg"))]
            {
                Vec::new()
            }
        };
        warn!("{}", format_all_mode_warning(&stats));
    }

    // ---- write-back adapter (ADR-0013) ----
    // The writable-table allowlist is enforced by the transport FIRST (a
    // single trust-boundary gate), then again by PgWriteBack as
    // defense-in-depth. Under `NOSTOS_REPLICATOR=pg` (feature `pg`) we inject a
    // real PgWriteBack connected to the source; otherwise NoWriteBack returns
    // a clear "write-back requires pg replicator" error. The allowlist is
    // always set on the state so the transport's gate is uniform.
    let write_tables = nostos_infra::parse_allowlist(&cfg.write_tables);
    let or_set_columns = nostos_infra::parse_or_set_columns(&cfg.or_set_columns);
    let counter_columns = nostos_infra::parse_counter_columns(&cfg.counter_columns);
    #[cfg(feature = "pg")]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = if cfg.replicator == "pg" {
        if cfg.pg_url.trim().is_empty() {
            anyhow::bail!(
                "NOSTOS_REPLICATOR=pg but NOSTOS_PG_URL is not set (required for write-back). \
                 Set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d"
            );
        }
        info!(tables = ?write_tables, or_set_columns = ?or_set_columns, counter_columns = ?counter_columns, "write-back: PgWriteBack (real source)");
        Arc::new(
            nostos_infra::PgWriteBack::new(&cfg.pg_url, write_tables.clone())
                .with_or_set_columns(or_set_columns.clone())
                .with_counter_columns(counter_columns.clone()),
        )
    } else {
        info!("write-back: NoWriteBack (fake replicator — writes return pg-required error)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    #[cfg(not(feature = "pg"))]
    let write_back: Arc<dyn nostos_application::ports::WriteBack> = {
        let _ = &cfg.pg_url; // unused without the pg feature
        let _ = &or_set_columns; // unused without the pg feature
        info!("write-back: NoWriteBack (binary built without `pg` feature)");
        Arc::new(nostos_infra::NoWriteBack::new())
    };
    state_builder = state_builder
        .with_write_back(Arc::clone(&write_back))
        .with_write_tables(write_tables);

    // ---- snapshot-on-subscribe adapter (ADR-0014) ----
    // Under `NOSTOS_REPLICATOR=pg` (feature `pg`) inject a real `PgSnapshotter`
    // so a freshly-subscribing client receives the table's pre-existing rows
    // before live fan-out (closes the "Flutter app shows 1
    // of 5 rows" gap). Otherwise `snapshotter` stays `None` (the default set in
    // `SyncRouterState::new`) and subscribe-time snapshots are skipped.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        // pg_url is already known non-empty here — the write-back block above
        // bailed on an empty NOSTOS_PG_URL under the same `replicator == "pg"`.
        let snapshotter: Arc<dyn nostos_application::ports::SnapshotSource> = Arc::new(
            nostos_infra::PgSnapshotter::new(&cfg.pg_url).with_max_rows(cfg.snapshot_max_rows),
        );
        state_builder = state_builder.with_snapshotter(snapshotter);
        info!(
            max_rows = cfg.snapshot_max_rows,
            "snapshot-on-subscribe: PgSnapshotter (real source)"
        );
    }

    // B2 mirror (ADR-0042): the handle's in-memory buffer IS the snapshot
    // source — a freshly-subscribing client sees pre-ingest rows (and the
    // shared LSN allocator keeps snapshot bands unique against live events).
    if let Some(ing) = &mirror_ingest {
        let snapshotter: Arc<dyn nostos_application::ports::SnapshotSource> =
            Arc::new(ing.handle.clone());
        state_builder = state_builder.with_snapshotter(snapshotter);
        info!("snapshot-on-subscribe: MirrorHandle (in-memory mirror buffer)");
    }

    // ---- boot-time tenant-column audit (2026-08-27 incident guard) ----
    // A tenant-column deploy whose tables lack the column produced only
    // swallowed snapshot errors and starving shops — and the clap default
    // (`org_id`) made that the config you get by UNSETTING the env var.
    // One catalog scan at boot names every affected table so the operator
    // reads the diagnosis instead of probing raw WS frames for an hour.
    // All-mode has no table list to audit (`synced_tables()` is empty), so
    // the guard only fires under toggles/hand rules — where the table set
    // is known. Non-fatal by design: a deliberately-global table is a
    // LEGITIMATE shape (see `scope_if_column_present`); the audit informs,
    // it does not block boot. An audit failure itself (PG unreachable for
    // the catalog read) also only warns — the real snapshotter will surface
    // connectivity loudly on its own.
    #[cfg(feature = "pg")]
    if let Some(col) = tenant_col {
        if cfg.replicator == "pg" && !rules_tables.is_empty() {
            match nostos_infra::snapshot_source::audit_tenant_column(
                &cfg.pg_url,
                col,
                &rules_tables,
            )
            .await
            {
                Ok(audit) => {
                    for table in &audit.columnless {
                        warn!(
                            table = %table, tenant_column = %col,
                            "synced table has NO tenant column: snapshots skip the \
                             tenant clause (safe), but the LIVE predicate references \
                             the column and will match NOTHING for tenant-scoped \
                             subscribers — post-seed changes to this table will not \
                             stream under a tenant deploy. If this table is meant to \
                             be global, this is fine (snapshot-first delivery); if \
                             not, add the column or fix NOSTOS_TENANT_COLUMN"
                        );
                    }
                    for table in &audit.missing {
                        warn!(
                            table = %table,
                            "ruleset syncs a table that does not exist in PG \
                             (public schema) — usually a typo in nostos_rules.toml"
                        );
                    }
                    if audit.columnless.is_empty() && audit.missing.is_empty() {
                        info!(
                            tenant_column = %col,
                            tables = rules_tables.len(),
                            "tenant-column audit: every synced table carries the column"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "tenant-column audit failed to read the catalog; continuing boot");
                }
            }
        }
    }

    // ---- op-log replay-on-reconnect adapter (ADR-0025 slice 4b) ----
    // Under `NOSTOS_REPLICATOR=pg` inject a `PgOpLogReader` so a reconnecting
    // client with a matching epoch + an in-window `resume_lsn` gets its offline
    // gap replayed from `cairn_oplog` instead of a full snapshot. Otherwise
    // `oplog_reader` stays `None` → reconnect always takes the snapshot path
    // (slice-1 reconcile remains the correctness floor either way).
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let reader: Arc<dyn nostos_application::ports::OpLogSource> =
            Arc::new(nostos_infra::PgOpLogReader::new(&cfg.pg_url));
        state_builder = state_builder.with_oplog_reader(reader);
        info!("op-log replay: PgOpLogReader (real source)");
    }

    // Guard (Fix A): NOSTOS_PG_URL set while replicator != "pg" is almost always
    // a misconfiguration — snapshot-on-subscribe (ADR-0014) stays OFF and a
    // freshly-subscribing client silently receives NONE of the table's
    // pre-existing rows (the "5 in Postgres, only 1 shows in the app" symptom).
    // The common cause is a fixture/.env that sets NOSTOS_PG_URL but omits
    // NOSTOS_REPLICATOR=pg. Fail loudly at startup instead of degrading silently.
    if cfg.replicator != "pg" && !cfg.pg_url.trim().is_empty() {
        // C10: BAIL (not warn) — a warn still let the server start degraded,
        // causing the silent "connected but lists empty" symptom (snapshot-
        // on-subscribe ADR-0014 stays OFF; clients receive no pre-existing
        // rows). Failing loudly at startup makes the misconfiguration
        // undiscoverable-by-accident. See docs/OPERATING.md §1.1(a).
        anyhow::bail!(
            "NOSTOS_PG_URL is set but NOSTOS_REPLICATOR={:?} is not 'pg' — \
             snapshot-on-subscribe (ADR-0014) is OFF, so clients would silently \
             receive none of the table's pre-existing rows on connect. \
             Set NOSTOS_REPLICATOR=pg, or unset NOSTOS_PG_URL.",
            cfg.replicator
        );
    }

    // ---- typed-schema endpoint adapter (WS1) ----
    // Under `NOSTOS_REPLICATOR=pg` inject a `PgSchemaSource` so `GET /schema`
    // can serve the publication's tables/columns/affinities for the Flutter
    // SDK's auto-schema (Option-C redesign). Otherwise
    // `schema_source` stays `None` and `GET /schema` returns 404.
    #[cfg(feature = "pg")]
    if cfg.replicator == "pg" {
        let schema_source: Arc<dyn SchemaSource> = Arc::new(nostos_infra::PgSchemaSource::new(
            &cfg.pg_url,
            &cfg.pg_publication,
        ));
        state_builder = state_builder.with_schema_source(schema_source);
        state_builder = state_builder.with_driver_dead(Arc::clone(&driver_dead));
        info!(publication = %cfg.pg_publication, "schema endpoint: PgSchemaSource");
    }
    let state = state_builder;
    // ADR-0041: cloned for the iroh accept loop (transport block below); the
    // router consumes the original via `.with_state`. Clone is Arcs — cheap.
    #[cfg(feature = "iroh")]
    let state_for_transport = state.clone();

    // CORS: explicit origins in production, permissive for local dev (the
    // empty-default case). Web clients need this to reach /sync from a browser.
    let cors = build_cors_layer(&cfg.cors_origins)?;

    let app = axum::Router::new()
        .route(&cfg.ws_path, get(sync_handler))
        .route("/healthz", get(healthz))
        // WS1: typed schema for client auto-schema. v2: add auth here (and to
        // /rules below) if a managed deploy wants to hide publication metadata.
        .route("/schema", get(schema))
        // ADR-0031: the active ruleset. GET stays on the same
        // v1-unauthenticated / v2-gated-together policy as /schema (see the
        // note above). PUT mutates it and is gated by NOSTOS_ADMIN_TOKEN
        // (Task 21): the route stays registered here on the same path (axum
        // has no per-method route table), but `put_rules_handler` returns a
        // literal 404 — before touching headers or body — whenever the
        // token is unset, so an unauthenticated caller observes exactly
        // what a genuinely unmounted route would look like.
        .route("/rules", get(rules_handler).put(put_rules_handler))
        .route(
            "/metrics",
            get({
                let m = Arc::clone(&metrics);
                let store_for_gauge = Arc::clone(&store);
                move || metrics_handler(m.clone(), store_for_gauge.clone())
            }),
        )
        .layer(cors.clone())
        .layer(TraceLayer::new_for_http().make_span_with(redacted_request_span))
        .with_state(state)
        // ADR-0037 §3 (plan 3.1): push-token registration, same JWT auth as
        // /sync, own state (registry + auth + tenant column) — merged after
        // `.with_state` so the two state types stay separate. The CORS +
        // trace layers are re-applied here because `.layer` only covers
        // routes registered on the router at call time; without this, a
        // browser SDK's cross-origin POST /push-tokens would be blocked.
        .merge(
            axum::Router::new()
                .route(
                    "/push-tokens",
                    axum::routing::post(push_api::post_push_token),
                )
                .route(
                    "/push-tokens/:token",
                    axum::routing::delete(push_api::delete_push_token),
                )
                .with_state(push_api::PushApiState {
                    auth: Arc::clone(&auth),
                    registry: Arc::clone(&push_registry),
                    tenant_column: tenant_col.map(str::to_string),
                })
                .layer(cors)
                .layer(TraceLayer::new_for_http().make_span_with(redacted_request_span)),
        );

    // B2 mirror (ADR-0042): the engine's write door. Mounted ONLY under
    // NOSTOS_REPLICATOR=mirror — no handle, no route (the mount-time shape of
    // PUT /rules's fail-closed unset-token 404).
    let app = if let Some(ing) = mirror_ingest {
        app.merge(
            axum::Router::new()
                .route("/ingest", axum::routing::post(ingest::post_ingest))
                .with_state(ing)
                .layer(build_cors_layer(&cfg.cors_origins)?)
                .layer(TraceLayer::new_for_http().make_span_with(redacted_request_span)),
        )
    } else {
        app
    };

    // ---- transport selection (ADR-0041) ----
    // "ws" (default): sync sessions ride HTTP/WS on NOSTOS_BIND. "iroh": an
    // iroh endpoint serves the sync session core NATIVELY (D6 — the accept
    // loop runs the WebSocket handshake on each QUIC stream and hands it to
    // transport::run_session; no loopback hop). The printed QR-native
    // iroh:// URL is what clients dial. The HTTP surface (healthz/metrics/
    // schema/rules/push REST, plus the /sync upgrade route for any
    // direct-dialing ws client) binds NOSTOS_BIND in BOTH modes.
    #[cfg(feature = "iroh")]
    if cfg.transport == "iroh" {
        // ADR-0041 D8: env-only operator knob — read only in iroh builds
        // under NOSTOS_TRANSPORT=iroh; kept out of clap so non-iroh binaries
        // don't advertise a knob they can't use (docs/OPERATING.md §1/§9).
        // main.rs reads the env, infra stays env-free. Some(url) =
        // self-hosted relay replaces the n0 default fleet.
        let relay_url = nostos_infra::iroh_sync::parse_relay_url(
            nostos_infra::env::var("NOSTOS_IROH_RELAY_URL")
                .ok()
                .as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(url) = &relay_url {
            info!(relay = %url, "iroh sync: NOSTOS_IROH_RELAY_URL set — self-hosted relay replaces the n0 default fleet");
        }
        let endpoint = nostos_infra::iroh_sync::bind_sync_endpoint(relay_url)
            .await
            .context("iroh sync endpoint bind failed")?;
        let url = endpoint.url(&cfg.ws_path);
        info!(
            transport = "iroh",
            http_bind = %cfg.bind,
            dial_url = %url,
            "Nostos sync server listening — clients dial the iroh:// URL"
        );
        tokio::spawn(async move {
            endpoint.serve_sessions(state_for_transport).await;
        });
    }
    #[cfg(not(feature = "iroh"))]
    if cfg.transport == "iroh" {
        anyhow::bail!(
            "NOSTOS_TRANSPORT=iroh but this server was built without the iroh feature \
             (cargo build -p nostos-server --features iroh)"
        );
    }

    let addr: SocketAddr = cfg
        .bind
        .parse()
        .with_context(|| format!("invalid bind address: {}", cfg.bind))?;
    info!(%addr, ws_path = %cfg.ws_path, transport = %cfg.transport, "Nostos HTTP surface listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    // ---- sync-rules hot reload (ADR-0031 D3, Task 14) ----
    // No engine restart: the watcher polls the same file loaded at boot and,
    // on an actual checksum change, swaps the shared ruleset + notifies every
    // live socket's cheap `watch::Receiver::changed()` arm (transport.rs).
    let (rules_shutdown_tx, rules_shutdown_rx) = tokio::sync::watch::channel(false);
    let rules_watch_handle = tokio::spawn(watch_rules(
        std::path::PathBuf::from(&cfg.rules_file),
        rules_shared,
        std::time::Duration::from_secs(5),
        rules_shutdown_rx,
        rules_tx,
    ));

    // Graceful drain: on SIGTERM/Ctrl-C, axum stops accepting new connections
    // and waits for in-flight ones. The ack-driven slot model (ADR-0009) means
    // the last confirmed LSN is already what every live client acked — no
    // unflushed progress to lose. The replicator's keepalive loop will advance
    // the slot one final time on the next status interval before the task ends.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    // ADR-0026 fix A (C6 late-append P1): stop ingesting new replication
    // changes BEFORE draining the op-log, so no append can race into the final
    // flush and be silently lost (ghost row on matching-epoch reconnect). The
    // detached replicator is the producer; the op-log drain is the consumer —
    // stop the producer first.
    #[cfg(feature = "pg")]
    if let Some(h) = repl_handle.take() {
        h.abort();
    }
    // ADR-0025 slice-6 follow-up: drain the op-log writer's in-flight batch so
    // a SIGTERM doesn't drop the last ≤BATCH_MAX entries mid-INSERT (those
    // clients would otherwise fall back to snapshot-reconcile on reconnect).
    #[cfg(feature = "pg")]
    {
        if let Some(w) = op_log_shutdown {
            // Bound the drain (audit 2026-08-17 M8): the flush task may be
            // inside a PG connect/execute, and a partitioned PG during
            // SIGTERM must not hang graceful shutdown forever. The
            // statement_timeout (30s) already bounds the statements; 35s
            // here is the last backstop — on expiry we log and exit, and
            // the dropped tail is recovered by snapshot-reconcile.
            if tokio::time::timeout(std::time::Duration::from_secs(35), w.shutdown())
                .await
                .is_err()
            {
                tracing::warn!(
                    "op-log drain exceeded 35s during shutdown; exiting — unflushed tail \
                     rows reconcile via snapshot on reconnect"
                );
            }
        }
    }
    let _ = rules_shutdown_tx.send(true);
    let _ = rules_watch_handle.await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received Ctrl-C, shutting down"),
        () = terminate => info!("received SIGTERM, shutting down"),
    }
}
