# nostos-pushd — standalone push daemon: launch-blocker implementation plan

Decision record: ADR-0038 (grill-ratified 2026-08-17). Amends ADR-0037 scope.
Research: `docs/research/push-server-brief.md`. Existing embedded push:
ADR-0037 + `docs/plans/nostos-push-notifications-implementation.md` (24/24, done).

**Sequence note:** the daemon is a launch blocker (ADR-0038 §5) — every wave is on the Phase-3 critical path. Wave 3 serves the embedded story too and can land first.

## Wave 0 — design pins (decide before code; record answers here)

- [x] 0.1 REST API surface — PINNED 2026-08-17, contract at `docs/api/nostos-pushd.yaml`: `POST /v1/tokens` (upsert), `DELETE /v1/tokens/{token}` (204 idempotent, owner-scoped, foreign → 404), `POST /v1/send` (one token per request; silent | visible{title,body,category?}; optional collapse_key/priority/metadata; 202 + push_id), `GET /v1/receipts?since=<seq>&limit=<n>` (polling receipt log — boring and testable, no webhook push-back), `GET /v1/healthz` (unauthenticated). Bearer auth on all other /v1. Batch send deferred to v1.1 (ponytail).
- [x] 0.2 API-key model — PINNED 2026-08-17: env-seeded `NOSTOS_PUSHD_API_KEYS="tenant:secret,tenant2:secret2"`, fail-fast at boot if empty/malformed; constant-time compare via `subtle` (already a workspace dep); tenant force-stamped from the matched key (ADR-0018 — never client-attested). ponytail: CLI key CRUD + hashed-at-rest storage deferred to v1.1 — keys live in .env under the same threat model as the rail secrets.
- [x] 0.3 Registry schema — PINNED 2026-08-17: `push_tokens(token TEXT PK, platform TEXT CHECK(platform IN ('apns','fcm','webpush')), tenant_id TEXT NOT NULL, account_tag TEXT, created_at TEXT, updated_at TEXT)` + idx(tenant_id, account_tag); `receipts(seq INTEGER PK AUTOINCREMENT, push_id, token, outcome, detail, provider_ts)` with retention sweep. Store trait + SQLite impl (rusqlite bundled, `Arc<Mutex<Connection>>` mirroring `nostos-cloud/src/store.rs:90`); `NOSTOS_PUSHD_DB` default `./cairn-pushd.db`. PG impl behind nostos-push `pg` feature (dep:tokio-postgres) — attempted time-boxed after SQLite green, ponytail-deferred to v1.1 if it bloats.
- [x] 0.4 Receipt format — PINNED 2026-08-17: `{seq, push_id, token, outcome: delivered|unregistered|transient|fatal, detail?, provider_ts}`; append-only log, monotonic seq cursor; RemoteNotifier polls `GET /v1/receipts?since=`. push_id = uuid v4 if uuid is a workspace dep, else time-nanos + atomic counter (implementer reports the choice). `POST /v1/send` accepts an optional `metadata` object echoed into the receipt — that is the push-LSN correlation channel.
- [x] 0.5 Crate boundary — PINNED 2026-08-17: `crates/nostos-push` (lib + `nostos-pushd` bin), depends on nostos-domain + nostos-infra ONLY (application ports not needed daemon-side — PushNotifier is the sync side's port). Zero new external deps: axum/tokio/rusqlite/subtle/serde/tracing all already workspace deps. Root Cargo.toml members + AGENTS.md crate-map row added by the implementer. Config: clap + `NOSTOS_PUSHD_*` env (server pattern); `NOSTOS_PUSHD_BIND` default `127.0.0.1:8090`, `NOSTOS_PUSHD_DEBOUNCE_MS` default 2000 (mirrors router.rs's 2s window); rails keep their existing `NOSTOS_FCM/APNS/WEBPUSH` envs via from_env().

## Wave 1 — daemon core

- [ ] 1.1 Crate skeleton: composition-root main (clap + NOSTOS_* env, same pattern as nostos-server `main.rs:34-41`), lib with send/registry modules.
- [ ] 1.2 Registry: SQLite store + migrations; PG option (pool-of-one, `PgTokenStore` pattern). Prune on `RailOutcome::Unregistered`.
- [ ] 1.3 API-key auth middleware (constant-time compare; per-tenant scope; stamped tenant, never client-attested).
- [ ] 1.4 Token routes: register / deregister (idempotent delete, owner-scoped — mirror `crates/nostos-server/src/push_api.rs` discipline).
- [ ] 1.5 Send route: build `PushPayload` (silent | visible w/ single-column interpolation, same template syntax as embedded), dispatch to rail by token.platform. Reuse `nostos_infra::push::{apns,fcm,webpush}` AS-IS — no rail forks.
- [ ] 1.6 Debounce coalescer: per-(tenant,target) time-window buffer (bounded channel off the request path — OpLogWriter-contract discipline); flush with rail-native supersede keys (collapse_key / apns-collapse-id / Topic). Config: `NOSTOS_PUSHD_DIGEST_MS` (default pinned in 0.x).
- [ ] 1.7 Rail config: identical env contract (NOSTOS_FCM_CREDENTIALS_JSON, NOSTOS_APNS_*, NOSTOS_WEBPUSH_VAPID_*) + `from_env()` unset = rail off.
- [ ] 1.8 Tests: provider mock (`test_support.ProviderMock` idiom — promote it from #[cfg(test)] to a shared testkit or duplicate); burst-coalesce test; 410/UNREGISTERED prune test; auth rejection tests.

## Wave 2 — RemoteNotifier delegation

- [x] 2.0 Delegation-registry amendment — PINNED 2026-08-17 (pre-build design review): ADR-0038 §3 says delegation sends carry `(token, payload)` and registries are never shared/synced — but `POST /v1/send` 404s on unregistered tokens, which breaks registry-free delegation. Resolution: the contract gains an optional `platform` field on SendRequest — when present AND the token is not in the daemon registry, the send dispatches directly (registry-free rail mode for trusted delegators); when absent, standalone registry lookup + 404 semantics apply unchanged. Contract bumps to 0.2.0 in Wave 2 (Wave 1 builds 0.1.0 as pinned). Rationale: dual-registering nostos-server's tokens with the daemon would re-create the drift-prone second registry ADR-0037 §1 rejects, now across a network. Account→token resolution, presence checks (ADR-0037 §4), and template interpolation stay in nostos-server; the daemon in delegation mode is the coalescing rail + receipt log.
- [ ] 2.1 `RemoteNotifier` in nostos-infra implementing the `PushNotifier` port (`ports.rs:220`): non-blocking contract respected (enqueue-only, bounded channel; NEVER a PG/HTTP round-trip on the fan-out hot loop — ADR-0037 §4).
- [ ] 2.2 Sends carry push-LSN metadata; daemon returns receipts (0.4); receipt consumer feeds push-LSN → client-ack correlation.
- [ ] 2.3 Composition-root wiring: `NOSTOS_PUSH_REMOTE_URL` + `NOSTOS_PUSH_REMOTE_KEY` → RemoteNotifier; unset → embedded PushRouter; neither → NoopNotifier. Precedence documented.
- [ ] 2.4 E2E: ADR-0038 test-that-matters (delegation leg) green.

## Wave 3 — developer ergonomics (serves embedded AND daemon; can land first)

- [ ] 3.1 `nostos push init`: interactive — which rails; writes gitignored .env entries (NEVER nostos.toml — secret-free convention); validates p8 PEM shape, service-account JSON fields; mints VAPID keypair on request.
- [ ] 3.2 `nostos push check`: credential reachability dry-run per configured rail (APNs JWT mint + sandbox probe, FCM OAuth token mint, VAPID signature self-check). Reports reachability, not end-to-end delivery (honest limits).
- [ ] 3.3 Docs page: push with nostos — embedded (env vars), daemon (standalone), delegation (RemoteNotifier) — one page, three recipes.

## Wave 4 — launch gate (the blocker checklist)

- [ ] 4.1 Security review: token registry is PII-adjacent; API-key storage (hashed at rest?); tenant isolation; rate limits on /v1/send; no row data in payloads (doorbell discipline, ADR-0037 §2 — daemon visible templates are operator-configured, never row-derived beyond interpolation).
- [ ] 4.2 Bench gate: `make bench` with RemoteNotifier enabled — hot-loop latency unchanged; recorded in benches/results/RESULTS.md per docs/BENCHMARK-METHODOLOGY.md.
- [ ] 4.3 docker compose leg: nostos-pushd service + env template.
- [ ] 4.4 README + docs: what it is / isn't (NOT a marketing platform — ADR-0037 boundary; no delivery guarantees — honest limits).
- [ ] 4.5 CI: nostos-push in fmt/clippy (-D warnings)/test matrix; unsafe-forbidden lint covers the new crate.
- [ ] 4.6 Launch narrative: daemon as land-and-expand top-of-funnel — "the only push server with a sync-aware upgrade path."

## Explicitly out of scope (ADR-0038 §2 — do not re-litigate without an ADR)

Topics/pub-sub channels, scheduling, segments, A/B testing, marketing analytics, presence-aware coalescing in the daemon (sync-side only), feature-crate extraction of the rails.

## Open risks (from the grill session, on record)

- Phase-3 slips by weeks; sync debut staked on commodity daemon landing cleanly (operator-accepted 2026-08-17).
- No pilot-developer field data on config stumbling points — the launch is the probe (3.1/3.2 are the cheapest mitigation).
- ntfy/Novu copy DX faster than nostos copies their ecosystem; the durable differentiator is the RemoteNotifier upgrade path, not feature count.
