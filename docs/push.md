# Push with Nostos

> *Three ways to send push through Nostos: embedded in the sync server, the standalone daemon, or delegation between them. One credential contract everywhere. What these tools verify — and what they deliberately don't.*

Push is a **wake-up trigger, not a data channel** (ADR-0037 §2). A push payload carries at most `{table, lsn}` — the client's durable LSN checkpoint is the correctness mechanism, so a missed or stale push loses nothing. Row data never transits Apple, Google, or any push vendor.

---

## 1. The credential contract (all three recipes)

Every push surface in Nostos reads the same environment variables — there is one contract, not one per product:

| rail | env vars | shape |
|---|---|---|
| APNs | `NOSTOS_APNS_KEY_P8` | .p8 PEM inline **or** a filesystem path |
| | `NOSTOS_APNS_KEY_ID` | exactly 10 chars (Apple developer console → Keys) |
| | `NOSTOS_APNS_TEAM_ID` | 10-char team id (Membership) |
| | `NOSTOS_APNS_BUNDLE_ID` | the app's bundle id (becomes `apns-topic`) |
| | `NOSTOS_APNS_SANDBOX` | optional `=1` → sandbox endpoint |
| FCM | `NOSTOS_FCM_CREDENTIALS_JSON` | the service-account JSON itself (inlined by `nostos push init` — this rail does not read paths, unlike the APNs p8) |
| Web Push | `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY` | base64url of the 32-byte P-256 scalar |
| | `NOSTOS_WEBPUSH_VAPID_SUBJECT` | `mailto:` contact for providers to reach you |

An unset rail is an **off** rail — each one is independently optional, and a partially-set rail is a boot error, never a silent skip.

Secrets live in `.env` (gitignored) or your platform's secret store. `nostos.toml` is secret-free by design and stays that way: the push commands write **only** to `.env`, never to config.

## 2. `nostos push init` and `nostos push check`

The CLI owns the ergonomics of that contract (ADR-0038 Wave 3). Both are flag-driven and non-interactive — scripting-friendly, no TTY prompts.

```shell
# APNs: path to the downloaded .p8 (stored as the path) or paste the PEM inline
nostos push init --apns \
  --apns-key-p8 ./AuthKey_ABCDEFGHIJ.p8 \
  --apns-key-id ABCDEFGHIJ \
  --apns-team-id TEAM456789 \
  --apns-bundle-id run.nostos.app

# FCM: path to the service-account JSON (Firebase console → Project settings
# → Service accounts → Generate new private key) or the JSON inline.
# A path is read now and stored as minified JSON — the FCM rail parses the
# env var directly and does not read paths.
nostos push init --fcm --fcm-credentials-json ./nostos-fcm.json

# Web Push: mints a fresh VAPID P-256 keypair and PRINTS the public key —
# the client side needs it to subscribe
nostos push init --webpush --vapid-subject mailto:ops@example.com
```

What `init` validates before writing anything:

- **APNs**: the p8 contains a private-key PEM block and parses as a P-256 EC key (the exact parse the rail performs); key id is exactly 10 characters.
- **FCM**: the JSON parses and carries `client_email`, `private_key` (a PEM RSA key), and `project_id`.
- **Web Push**: the subject starts with `mailto:`; the keypair is minted with the same recipe the rail's tests use.

`.env` handling is conservative: existing keys are updated in place (never duplicated), a non-blank existing value is **skipped** unless `--force` (the report names what was skipped), and `--env-file` redirects the target. A rail that fails validation leaves the file untouched.

`nostos push check` then dry-runs every configured rail:

```shell
nostos push check          # shape checks; reads .env, process env overrides
nostos push check --probe  # APNs only: + TLS handshake to Apple's development gateway
```

- **APNs**: builds the ES256 provider JWT exactly as the rail does and validates its claims (`iss` = team id, fresh `iat`, `kid` = key id). With `--probe`, additionally performs a TLS handshake to `api.development.push.apple.com:443` — **no notification is sent**.
- **FCM**: exchanges the service-account key for a real OAuth2 access token (JWT-bearer grant, `firebase.messaging` scope) and reports obtained/failed.
- **Web Push**: offline shape check — the key decodes to a 32-byte P-256 scalar, the subject is `mailto:`.

Exit code is 0 only when every configured rail passes. Unset rails report as off, not failed.

**Honest limits:** these checks verify credential *shape* and provider *reachability* — never end-to-end delivery. A green `nostos push check` says your key parses and Apple will talk to you; it does not say a notification will land on a device. Delivery stays best-effort (ADR-0037 §4) — the client's LSN ack is the proof that data arrived.

## 3. Recipe 1 — embedded push in nostos-server

Push ships inside the sync server (ADR-0037): the router rides the same predicate pass that feeds WebSocket fan-out, so push candidates are derived from sync state — no second subscription registry to drift.

```shell
nostos push init --apns ... --fcm ... --webpush ...
nostos push check          # green before the server ever boots
nostos dev                 # or your deployment: rails activate per env var
```

Tokens register through the server's own authenticated REST surface (`POST /push-tokens`, same JWT path as `/sync`); tenant and account are stamped server-side, never client-attested. See `docs/api/push.md` for the client-side surface and ADR-0037 for the router/coalescer design (digest window, presence re-check at send time, prune-on-410).

## 4. Recipe 2 — the nostos-pushd daemon quickstart

The standalone daemon (ADR-0038) sends push **without sync**: token-addressed sends, its own registry, the same three rails. Useful on its own, and it is the only push daemon with a sync-aware upgrade path — start here, adopt sync later, nothing re-configures.

```shell
# 1. Same credential contract — reuse the .env you already validated:
nostos push check

# 2. Daemon auth: tenant API keys (tenant is force-stamped from the key)
echo 'NOSTOS_PUSHD_API_KEYS=acme:secret-word,hq:another-secret' >> .env

# 3. Run (SQLite registry at ./cairn-pushd.db by default; NOSTOS_PUSHD_DB to move it)
nostos-pushd   # binds 127.0.0.1:8090; NOSTOS_PUSHD_BIND to expose it
```

Then drive it per `docs/api/nostos-pushd.yaml` (bearer auth on every `/v1` route but `/v1/healthz`):

```shell
API=acme:secret-word

# Register a device token for this tenant (upsert; idempotent)
curl -s -X POST localhost:8090/v1/tokens \
  -H "Authorization: Bearer $API" -H 'Content-Type: application/json' \
  -d '{"token":"a1b2...","platform":"apns","account_tag":"user-42"}'

# Silent doorbell (content-free wake — the client syncs on receipt)
curl -s -X POST localhost:8090/v1/send \
  -H "Authorization: Bearer $API" -H 'Content-Type: application/json' \
  -d '{"token":"a1b2...","payload":{"silent":{"table":"tasks","lsn":"1234"}}}'

# Visible notification (operator template, already interpolated)
curl -s -X POST localhost:8090/v1/send \
  -H "Authorization: Bearer $API" -H 'Content-Type: application/json' \
  -d '{"token":"a1b2...","payload":{"visible":{"title":"Tasks changed","body":"You have new tasks"}}}'

# Poll the append-only receipt log (outcome + echoed metadata per push)
curl -s "localhost:8090/v1/receipts?since=0" -H "Authorization: Bearer $API"
```

Sends are coalesced per (tenant, token) inside the debounce window; outcomes land in the receipt log as `delivered` / `unregistered` / `transient` / `fatal`, and `unregistered` prunes the token row. NOT a marketing platform: no topics, scheduling, segments, or A/B (ADR-0037 boundary, reaffirmed in ADR-0038 §2).

## 5. Recipe 3 — delegation via RemoteNotifier

Shipped (Wave 2, ADR-0038 §3). A nostos-server pointed at a daemon stops sending through its own rails and delegates instead — set both variables in the **nostos-server** environment:

```shell
NOSTOS_PUSH_REMOTE_URL=https://push.internal:8090
NOSTOS_PUSH_REMOTE_KEY=secret-word      # the SECRET only — no suffix
```

On the **daemon** side, the matching `NOSTOS_PUSHD_API_KEYS` entry for that key MUST carry the `:rail` role suffix — delegation sends are rail-mode sends (unregistered token + `platform` field), and since the 2026-08-17 security closeout a Standard key gets `403` on them:

```shell
NOSTOS_PUSHD_API_KEYS="acme:s3cr3t,hq:secret-word:rail"
```

Precedence: both set → delegate to the daemon; unset → embedded router (Recipe 1); neither credential contract present → `NoopNotifier`. The daemon's receipt log flows back through the RemoteNotifier's poll, so push-LSN → client-ack correlation survives the network hop: `delivered` receipts advance the correlation map, `unregistered` receipts prune the server-side registry row, coalesced receipts (detail `coalesced:<winner>`) feed only correlation — never the sent/failed counters. No token registry is ever shared between server and daemon — delegation sends carry `(token, platform, payload)`, nothing else.

### Security behavior (2026-08-17 audit closeout, contract 0.3.0)

- **Rate limits**: `POST /v1/send` is token-bucket limited per tenant — `NOSTOS_PUSHD_SEND_RATE_PER_SEC` (default 10) sustained, `NOSTOS_PUSHD_SEND_BURST` (default 50) instantaneous; exhaustion is `429`. The coalescer also caps open debounce windows (`NOSTOS_PUSHD_PENDING_KEYS_MAX`, default 10 000) — a send for a NEW key past the ceiling is `429`.
- **Field caps** → `400`: title 256, body 1024, token 2048 (same bound as the registry, so a registered Web Push subscription token always sends), collapse_key 256, category 128, serialized metadata ≤ 4096 bytes.
- **Role gating** → `403`: rail mode (unregistered token + `platform`) requires a `:rail`-role key; registered-token sends accept either role.
- **Ownership** → `409`: registering a token held by another tenant is refused (never silently reassigned) — the old owner DELETEs first. `DELETE /v1/tokens/{token}` is `204` for every not-yours case (no token-existence oracle).
- **Healthz** → `{"status":"ok"}` only; the rails booleans live behind auth on `GET /v1/status`.

---

## References

- ADR-0037 — sync-aware push notifications (embedded router, doorbell semantics, honest limits)
- ADR-0038 — standalone push daemon + RemoteNotifier delegation
- `docs/api/push.md` — client-side token registration (embedded)
- `docs/api/nostos-pushd.yaml` — daemon REST contract
- `crates/nostos-infra/src/push/mod.rs` — the env-var contract's source of truth
