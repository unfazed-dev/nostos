# nostos-push — nostos-pushd, the standalone push daemon

Token-addressed APNs / FCM HTTP v1 / Web Push sends with debounce coalescing,
behind one REST API and one env-var credential contract. ADR-0038.

## What it is

- A self-hosted push server you run next to any stack: register device
  tokens, POST sends, poll delivery receipts. No nostos sync engine required.
- Three rails, configured by env vars — unset = rail off:
  | rail | env |
  |---|---|
  | FCM HTTP v1 | `NOSTOS_FCM_CREDENTIALS_JSON` (service-account JSON, inline) |
  | APNs | `NOSTOS_APNS_KEY_P8` (p8 PEM or path), `NOSTOS_APNS_KEY_ID`, `NOSTOS_APNS_TEAM_ID`, `NOSTOS_APNS_BUNDLE_ID`, optional `NOSTOS_APNS_SANDBOX=1` |
  | Web Push | `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY`, `NOSTOS_WEBPUSH_VAPID_SUBJECT` (mailto:) |
- Tenant-scoped API keys (`NOSTOS_PUSHD_API_KEYS="tenant:secret,…"`), a
  SQLite token registry with prune-on-410/UNREGISTERED, and a per-target
  debounce coalescer using rail-native supersede keys.
- The API contract is versioned and pinned: `docs/api/nostos-pushd.yaml`.

## What it is NOT

- **Not a marketing platform.** No topics, scheduling, segments, A/B tests,
  or campaign analytics — that boundary is ratified in ADR-0037 and stands.
- **Not a delivery guarantee.** APNs/FCM/Web Push last-mile is best-effort;
  outcomes are reported via the receipt log, never promised. Push is a
  nudge — reconcile state another way (in nostos, that way is sync).
- **Not presence-aware.** "Don't doorbell an online device" needs a session
  store, which only the sync engine has. Standalone coalescing is
  time-window debounce only.

## Quickstart

```sh
# 1. Credentials — validates and writes gitignored .env (never nostos.toml):
nostos push init --fcm --fcm-credentials-json ./service-account.json
nostos push init --webpush --vapid-subject mailto:ops@example.com

# 2. Sanity — credential shape/reachability, never end-to-end delivery:
nostos push check

# 3. Run:
NOSTOS_PUSHD_API_KEYS="acme:s3cr3t" nostos-pushd

# 4. Register + send (see docs/api/nostos-pushd.yaml):
#    POST /v1/tokens  {"token": "…", "platform": "fcm"}
#    POST /v1/send    {"token": "…", "payload": {"visible": {"title": "Hi", "body": "…"}}}
#    GET  /v1/receipts?since=0
```

Docker: the `nostos-pushd` service in `docker/docker-compose.stack.yml`
(ADR-0038; builds from the root Dockerfile).

## The upgrade path (why this daemon exists)

nostos-pushd is the only push server with a sync-aware upgrade path. Adopt
nostos sync later, point `NOSTOS_PUSH_REMOTE_URL`/`NOSTOS_PUSH_REMOTE_KEY` at
this daemon, and your pushes become predicate-routed doorbells derived from
the same fan-out pass as sync — with presence-aware coalescing and
push-LSN → client-ack delivery proof no push vendor can offer
(ADR-0037 §5, ADR-0038 §3).

Full recipes: `docs/push.md`. Contract: `docs/api/nostos-pushd.yaml`.
Decision record: `docs/adr/0038-standalone-push-daemon.md`.
