# nostos-pushd — standalone push daemon: launch-blocker implementation plan

Decision record: ADR-0038 (grill-ratified 2026-08-17). Amends ADR-0037 scope.
Research: `docs/research/push-server-brief.md`. Existing embedded push:
ADR-0037 + `docs/plans/nostos-push-notifications-implementation.md` (24/24, done).

**Sequence note:** the daemon is a launch blocker (ADR-0038 §5) — every wave is on the Phase-3 critical path. Wave 3 serves the embedded story too and can land first.

## Wave 0 — design pins (decide before code; record answers here)

- [ ] 0.1 REST API surface (OpenAPI yaml in `docs/api/nostos-pushd.yaml`): `POST /v1/tokens`, `DELETE /v1/tokens/{token}`, `POST /v1/send` (token-addressed; silent | visible{title,body,category?}), `GET /v1/healthz`. Versioned /v1 from day one.
- [ ] 0.2 API-key model: per-tenant scoped bearer keys, created how (CLI subcommand vs env-seeded)? Tenant force-stamp discipline per ADR-0018.
- [ ] 0.3 Registry schema: `push_tokens(token PK, platform, tenant_id, account_tag?, created_at, updated_at)`; SQLite file default, PG URL option.
- [ ] 0.4 Receipt format for RemoteNotifier correlation: `{push_id, token, rail_outcome, provider_ts}` — push_id ties to push-LSN metadata passed at send time.
- [ ] 0.5 Crate boundary: `crates/nostos-push` (lib + `nostos-pushd` bin), depends on nostos-infra (rails), nostos-application (ports where reusable), nostos-domain. Workspace members + [workspace.dependencies] updated; AGENTS.md crate-map row added.

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
