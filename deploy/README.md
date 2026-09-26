# Deploying nostos

## Atlet Appwrite gateway

The [Atlet gateway config](atlet-appwrite.fly.toml) deploys `nostos-server`
near the ADS project's Appwrite `fra` region. It uses the
[gateway-only image](Dockerfile.atlet-appwrite), a fixed project and Function,
two sync routes, and no Appwrite API key or Postgres slot. Direct and server
mode therefore share the same TablesDB journal and Function authorization.
This is a manual deployment; the repository's release workflow does not
deploy on merge. Fly's [deploy documentation](https://fly.io/docs/launch/deploy/)
describes the app creation and config behavior.

From the repository root, after selecting an app name in a Fly account:

```sh
fly apps create <app-name>
fly deploy --config deploy/atlet-appwrite.fly.toml --app <app-name>
```

Check `https://<app-name>.fly.dev/healthz`, then set
`NOSTOS_APPWRITE_GATEWAY_URL=https://<app-name>.fly.dev` in the ignored
`apps/atlet/.env.cloud` and run the `--mode server` Rust launchers documented
in [Atlet](../apps/atlet/README.md). `/healthz` proves only the gateway is
listening; an authenticated pull proves the Appwrite path. The browser test
uses origin `http://127.0.0.1:8765`, the explicit origin in the Fly config.
Add any deployed Atlet web origin to `NOSTOS_CORS_ORIGINS` before serving it.

---

Two binaries, two deploy targets:

| Binary | Role | Port | Deploys as |
|---|---|---|---|
| `nostos-server` | the sync engine (Postgres logical-replication → clients) | 8800 | one Fly app **per managed project** (template: [`../fly.toml`](../fly.toml)) |
| `nostos-cloud` | the control plane (accounts, projects, API keys, billing, admin) | 9090 | one Fly app for the whole cloud |

Both build from the repo-root [`Dockerfile`](../Dockerfile) (multi-stage,
`nostos-server --features pg` + `nostos-cloud`, slim runtime).

---

## Managed-cloud architecture (the groundwork slice)

```
   customer                 nostos-cloud (1 app)              one Fly app per project
   ┌────────┐  POST /v1/    ┌──────────────────────┐   provision   ┌─────────────────────┐
   │ admin  │──projects/──► │ control plane        │ ────────────► │ nostos-server (sync)  │
   │ SPA    │   provision   │ • store (acct/proj/  │   (Fly API)   │  • NOSTOS_PG_URL=...  │
   └────────┘               │   api_keys/subs)     │               │  • its own slot      │
        │                   │ • Stripe billing     │               │  • isolated from     │
        │ Stripe            │ • Provisioner trait  │               │    every other proj  │
        ▼                   │   (Manual / Fly)     │               └──────────┬──────────┘
   ┌────────┐               └──────────┬───────────┘                          │ ws://<app>.fly.dev
   │ Stripe │  webhook HMAC            │ stores sync_url                      │ /sync
   └────────┘                          ▼                                      ▼
                                   ┌──────────────┐   each project's   ┌──────────────┐
                                   │  sqlite      │   Postgres source  │  Postgres    │
                                   │  (control)   │ ◄───────────────── │  (customer)  │
                                   └──────────────┘                     └──────────────┘
```

**Multi-tenant isolation = one nostos-server Fly app per project.** Each app
binds exactly one project's `NOSTOS_PG_URL` (a Fly secret, never logged), owns
its own `nostos_slot`/`nostos_pub`, and shares no state with other projects.
Isolation is at the process boundary, not a multi-tenant in-process split — the
simplest correct model (no cross-tenant leakage path through shared memory,
connection pools, or a shared SQLite file).

### The Provisioner seam (next slice — not yet wired)

The control plane stores `Project.sync_url` but does **not** yet provision the
sync-server instance. The next managed-cloud slice adds a `Provisioner` trait
(`crates/nostos-cloud/src/provision.rs`):

```rust
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Deploy an isolated nostos-server for `project` bound to `pg_source_url`.
    /// Returns the public sync_url the control plane records on the project.
    async fn provision(&self, project: &Project, pg_source_url: &str)
        -> Result<ProvisionedSync>;
}

pub struct ProvisionedSync {
    pub sync_url: String,    // wss://nostos-sync-<project>.fly.dev/sync
    pub region: String,
    pub instance_id: String, // the Fly app name
}
```

Two impls:
- **`ManualProvisioner`** — ops deploys the app by hand (the steps below) and
  registers the sync_url via `POST /v1/projects/{id}/sync-url`. For MVP / local
  / pre-Fly-account.
- **`FlyProvisioner`** — calls the Fly API (machines / apps) to deploy
  [`fly.toml`](../fly.toml) as `nostos-sync-<project-id>`, injects
  `NOSTOS_PG_URL` + `NOSTOS_WRITE_TABLES` as Fly secrets, returns the sync_url.
  Requires `FLY_API_TOKEN`; returns a clear `not-configured` error (never a
  silent stub) when absent.

A `POST /v1/projects/{id}/provision` route (admin-authed) calls the provisioner
and stores the resulting `sync_url`.

---

## Deploy a sync server by hand (ManualProvisioner path)

Prereqs: [`flyctl`](https://fly.io/docs/hands-on/install-flyctl/) + `fly auth login`.

```sh
APP=nostos-sync-$PROJECT_ID          # one app per project
fly launch --no-deploy --name "$APP" --image-label "$APP" --dockerfile Dockerfile
# Secrets (never commit) — the customer's Postgres source + the write allowlist:
fly secrets set --app "$APP" \
  NOSTOS_PG_URL="postgresql://...@<customer-pg>:5432/...?sslmode=require" \
  NOSTOS_WRITE_TABLES="tasks,providers,..." \
  NOSTOS_JWT_AUD="<your-aud>" \
  NOSTOS_JWKS_URL="<your-jwks>"   # or NOSTOS_JWT_SECRET for HS256
fly deploy --app "$APP"
# Register the sync_url with the control plane:
curl -X POST https://cloud.<your-domain>/v1/projects/$PROJECT_ID/sync-url \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -d "{\"sync_url\":\"wss://$APP.fly.dev/sync\"}"
```

Point the customer's client at `wss://$APP.fly.dev/sync`. Done.

### Deploy the control plane (nostos-cloud)

One app for the whole cloud (separate from the per-project sync apps):

```sh
fly launch --no-deploy --name nostos-cloud --dockerfile Dockerfile
fly secrets set --app nostos-cloud \
  STRIPE_SECRET_KEY=... STRIPE_WEBHOOK_SECRET=... NOSTOS_CLOUD_ADMIN_KEY=...
# The image's default command is nostos-server; the cloud app overrides it:
fly deploy --app nostos-cloud --strategy rolling
```

(The cloud app sets `[processes]`/CMD to `nostos-cloud`; the per-project sync
apps use the default `nostos-server` command.)

---

## Observability on Fly

- **Logs**: `NOSTOS_LOG_FORMAT=json` is set in [`fly.toml`](../fly.toml) →
  `fly logs` shows structured JSON; pipe to an OTel collector / Logtail /
  Datadog via Fly's [log shippers](https://fly.io/docs/reference/logs).
- **Metrics**: `GET /metrics` (Prometheus text) on the sync app — scrape with
  [`fly metrics`](https://fly.io/docs/reference/metrics/) or a Prometheus
  instance. Watch `nostos_slot_wal_status` (the P0-1 slot-health gauge) and
  `nostos_replication_lag_bytes` — alert if `wal_status` leaves `Healthy`.

---

## Why a sync server must not auto-stop

`fly.toml` sets `auto_stop_machines = false` + `min_machines_running = 1`.
A "sleeping" nostos-server stops consuming its Postgres replication slot; once
`max_slot_wal_keep_size` fires, `wal_status` flips to `lost` and offline changes
are silently skipped (the P0-1 data-loss class). **A stopped sync server is a
data-loss server.** Keep it running. (See
`docs/plans/nostos-soundness-audit-2026-07-19.md` §P0-1.)
