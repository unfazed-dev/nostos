# ADR-0038: Standalone push daemon (nostos-pushd) + optional RemoteNotifier delegation

- **Status:** Accepted (decision ratified via grill session 2026-08-17, six rounds).
- **Date:** 2026-08-17
- **Amends:** ADR-0037 Decision-2 boundary ("no rules engine, no scheduling, no A/B — that is the marketing-platform layer nostos explicitly does not build") — amended ONLY to permit a token-addressed send API and token registry outside the sync path. The platform-layer rejection stands.
- **References:** ADR-0007 (names `PushSink` as the extension point), ADR-0033 (experimental-behind-flag precedent), ADR-0037 (sync-aware push), `docs/research/push-server-brief.md` (2026-08-17: APNs/FCM rail requirements, competitor sweep, Rust crate landscape), `docs/STRATEGY.md:214` (push is a wake-up trigger, not a data channel), plan `docs/plans/nostos-push-daemon-implementation.md`.

## Context

ADR-0037 shipped sync-aware push inside nostos-server (plan 24/24, piloted on real APNs/FCM rails in atlet). The operator then asked whether push should become a dedicated crate so developers "just update config (p8, Google token)". Evidence gathered 2026-08-17:

- The credential story already exists and is env-only (`crates/nostos-infra/src/push/mod.rs:22-28`): `NOSTOS_FCM_CREDENTIALS_JSON`, `NOSTOS_APNS_KEY_P8/KEY_ID/TEAM_ID/BUNDLE_ID`, `NOSTOS_WEBPUSH_VAPID_*`; each rail is `from_env()` → unset = off.
- No sync competitor ships push (PowerSync, ElectricSQL, Supabase document it as DIY userland; re-verified 2026-08-17).
- The public Rust push-crate landscape is stale (`fcm` 0.9.2 predates the legacy-API shutdown; `a2` quiet; `google-fcm1` auto-generated).
- Self-hosted push demand is proven (ntfy ≈33k stars).

Grill outcome: the goal is a **standalone product** (usable without nostos sync) whose existential thesis is **land-and-expand** — the only push daemon with a sync-aware upgrade path.

## Decision

### 1. nostos-pushd — composition-root binary crate; NO feature-crate extraction

A new workspace member `nostos-push` (binary `nostos-pushd` + its lib), a composition root on the `nostos-bench`/`nostos-cloud` precedent, depending on `nostos-infra` for the rails. **Rejected alternative (recorded so it is not re-litigated):** extracting rails+router into a feature-shaped `nostos-push` library crate. Crates here are layers, not features; the rails are adapters and belong to nostos-infra; the router's value is its deliberate coupling to the fan-out predicate pass, `SessionStore` presence and `PgTokenStore`; and a crate boundary does nothing for config simplicity — the credential contract is env vars regardless of directory layout. The rails' `webpush` feature gate (iOS-staticlib/openssl-sys) survives unchanged.

### 2. v1 scope — token-addressed sends + templates + debounce coalescing

REST send API addressed to registered device tokens: silent doorbells and visible templates with interpolation; daemon-owned token registry with prune-on-410/`UNREGISTERED`; API-key auth; all three rails; the same env-var credential contract as embedded push. **Debounce coalescing:** a per-target time-window debounce plus the rail-native supersede keys already implemented (FCM `collapse_key`, APNs `apns-collapse-id`, Web Push `Topic`). **Explicitly not v1 (the 0037 boundary stands):** topics, scheduling, segments, A/B, marketing analytics. **Presence-aware coalescing stays sync-side** — "don't doorbell an online device" needs `SessionStore`; its absence standalone is upgrade-path value, not a gap.

### 3. RemoteNotifier — optional delegation over the existing port

`PushNotifier` is already a port (`crates/nostos-application/src/ports.rs:220`) with `NoopNotifier` default. A `RemoteNotifier` adapter points nostos-server at a remote nostos-pushd (`NOSTOS_PUSH_REMOTE_URL` + API key). Embedded `PushRouter` stays the default; delegation is opt-in. Delivery receipts flow back so the push-LSN → client-ack correlation (ADR-0037 §5 headline) survives the network hop. No token registry is ever shared or synced between server and daemon — delegation sends carry `(token, payload)`.

### 4. Daemon-owned registry; SQLite default

Daemon registry: SQLite default (single-binary ethos), Postgres option following the pool-of-one pattern (`PgTokenStore`). Tenant-scoped API keys; tenant force-stamping server-side per ADR-0018 discipline.

### 5. Timing — launch blocker (operator decision, risk on record)

nostos-pushd ships AS PART of the Phase-3 OSS launch. **Recorded risk:** Phase 3 slips by weeks and the sync engine's debut is staked on an unproven commodity daemon landing cleanly. Accepted by the operator 2026-08-17 with mitigations: the daemon clears the same launch bar as the sync engine (security pass, bench gate, compose leg, docs), and the pre-launch `nostos push init|check` ergonomics ship first because they serve the embedded story that is already launch-scoped.

## Consequences

- **Positive:** the only push daemon with a sync-aware upgrade path (land-and-expand top-of-funnel); one env-var credential contract across embedded, daemon, and delegation; rails get battle-tested by two products; honest Apache-2.0 answer to proven demand.
- **Negative:** Phase-3 launch delay; commodity competition (ntfy, Novu self-hosted) with no standalone moat; daemon inherits rail churn (ADR-0037 honest-limits posture applies — marketing never promises push delivery guarantees); two public APIs to version.
- **Reversal trigger:** if daemon adoption fails to convert to sync interest, demote nostos-pushd to maintenance mode; embedded push is unaffected (decision §3 keeps it the default and independent).

## The test that matters

Standalone: register a token via REST, send visible + silent through each rail against the provider mock (`push/mod.rs` `test_support` idiom); a burst of 20 sends to one target ⇒ exactly 1 coalesced push; an APNs 410 prunes the row. Delegation: with `NOSTOS_PUSH_REMOTE_URL` set, ADR-0037's test-that-matters passes unchanged (two devices share an account, one offline ⇒ exactly one coalesced push ⇒ LSN caught up on open) with sends transiting nostos-pushd and the receipt landing in push-LSN correlation. Bench gate: fan-out hot-loop latency unchanged with RemoteNotifier enabled.
