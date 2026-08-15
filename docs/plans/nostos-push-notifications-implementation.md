# Push notifications — implementation plan (ADR-0037)

Execution conventions per `docs/plans/HANDOFF.md`: tick checkboxes per task,
commit per task (single line, conventional prefix), `make ci` gate after every
task. Research basis: `nostos-push-notifications-research-2026-08-14.md`.

## Wave 1 — server core + flutter + node

### P1 Foundations (application/infra)

- [x] 1.1 Thread `Principal` through `StoredSession`/`SessionCandidate`
  (`crates/nostos-infra/src/store.rs:48-52`, `crates/nostos-application/src/ports.rs:206-210`)
  — `SyncSession` already carries it; the store discards it. Add an
  account→sessions index. Presence API: `SessionStore::account_online(account_id)`.
  Tests: index updated on register/unregister AND on eviction
  (`fanout.rs:309-318` — the leak path); zombie socket counts as offline.
- [x] 1.2 `PushNotifier` port (`ports.rs`, mirrors `EventSink` shape) +
  `NoopNotifier` default; composition-root wiring (`nostos-server/src/main.rs:400-412`).
- [ ] 1.3 Off-hot-loop enqueue: after the matched-set drain in `fan_out`
  (`fanout.rs:233-238`), fire-and-forget `(table, tenant, account, lsn)` into a
  bounded channel — copy the `OpLogWriter` non-blocking contract
  (`ports.rs:431-459`) verbatim, drop-on-full with a counter. Bench gate: fan-out
  latency unchanged with push enabled (extend `nostos-bench`).
- [x] 1.4 `cairn_push_tokens` migration + `PgTokenStore` (pool-of-one per
  `PgWriteBack`, `write_back.rs:231-246`): upsert/prune/list-by-account; prune
  on 410/`UNREGISTERED`; ADR-0013 identifier-regex discipline on table/column
  names. Column extractor typing note: predicates over numeric columns match
  wide (`main.rs:478-485`) — either fix typed extraction here or document the
  inherited hole in the ADR's template section (decision: fix in 1.4, it's
  ~20 lines and push makes it user-visible).

### P2 Rails (infra)

- [ ] 2.1 FCM HTTP v1 adapter (reqwest, OAuth2 service-account JWT): data-only
  + visible payloads, `collapse_key`, `ttl`, priority, 500-msg batch endpoint.
  Accept both `token` and `fid` (deprecation). Config: `NOSTOS_FCM_CREDENTIALS_JSON`.
- [ ] 2.2 APNs adapter (`a2` crate, token-based auth): `background` push-type
  + priority 5 for silent; `alert` + templated payload for visible;
  `apns-collapse-id`, `apns-expiration: 0` silent / short otherwise.
  Config: `NOSTOS_APNS_KEY_P8`, `NOSTOS_APNS_KEY_ID`, `NOSTOS_APNS_TEAM_ID`, bundle id.
- [ ] 2.3 Web Push adapter (direct VAPID): `web-push` crate — verify a version
  with CVE-2025-53604 fixed before depending; if unfixed, minimal RFC-8030
  client on reqwest + the crate's crypto only. `Topic`/`Urgency`/`TTL` headers.
  Config: `NOSTOS_WEBPUSH_VAPID_PRIVATE_KEY`, subject.
- [ ] 2.4 Coalescer task: bounded channel consumer; per-account look-back
  window (default ~2s, env `NOSTOS_PUSH_DEBOUNCE_MS`); per (device,
  subscription) collapse key; priority/template from per-table config
  (`NOSTOS_PUSH_TABLES` env or rules file section — match `nostos_rules.toml`
  style). Visible template: `{title, body, column?}` static interpolation only.

### P3 Server surface

- [ ] 3.1 REST `POST /push-tokens` + `DELETE /push-tokens/{token}` on the axum
  router (`main.rs:721-735` precedents) — same JWT auth as `/sync`; server-side
  tenant/account stamp from `Principal::tenant_scope` (`principal.rs:160-166`);
  reject client-attested tenant fields.
- [ ] 3.2 Metrics: `push_sent`/`push_failed`/`push_pruned` counters
  (`ports.rs:687-735` pattern) + push-LSN→client-ack correlation surface
  (expose "last pushed lsn per account" alongside session acked-lsn).
- [ ] 3.3 E2E test: fake rails (in-memory `PushNotifier`) — 100-event burst to
  an offline account ⇒ 1 push; online account ⇒ 0 pushes (no double-signal);
  `Dropped`-but-online ⇒ 0 pushes; sign-out ⇒ token pruned. Real-rail smoke
  behind env vars (skip cleanly, like `NOSTOS_E2E_PG`).

### P4 Flutter + node SDKs

- [ ] 4.1 flutter: `NostosDatabase.registerPushToken(platform, token)` → REST;
  deregister in `_signOutHooks` (`nostos_database.dart:645` pattern); wake
  entry: bg-isolate re-`connect`+`subscribe` from durable checkpoint (FRB
  handle can't cross isolates — re-init, `resume()` exists at engine.dart:161);
  app-side FCM/APNs handler doc + example wiring.
- [ ] 4.2 node: `registerPushToken` symmetry (no OS push; for completeness).

## Wave 2 — UniFFI wake API + mobile SDKs

- [x] 5.1 `SyncClient::disconnect()`/`resume()` non-destructive siblings in
  `nostos-client` (node's `close()` at `src/lib.rs:467` is the model); expose via
  UniFFI on kotlin/swift/dotnet (`disconnect` must NOT wipe — contrast
  `sign_out`). This is the prerequisite gap; test: disconnect→resume→delta
  applies from checkpoint, no data loss.
- [ ] 5.2 kotlin/swift: token registration + FCM/APNs handler wiring + poke
  (resume from killed app: `connect`+`subscribe` cold path is safe — verify
  durable checkpoint on both).
- [ ] 5.3 react_native: TurboModule spec methods bridging 5.1/5.2.
- [ ] 5.4 dotnet: registration + host-app wake doc (MAUI push is host-dependent).

## Wave 3 — experimental: web + capacitor + Live Activities

- [ ] 6.1 Storage seam: replace `Window::localStorage` dependency in
  `nostos-ffi-wasm` with an injected key-value store (SW-compatible).
- [ ] 6.2 Real Service Worker: `push` event → postMessage wake to
  `nostos.worker.js`; VAPID pubkey config; permission UX in index.js. Behind a
  flag until proven (ADR-0033 discipline).
- [ ] 6.3 Capacitor native plugin (new package): APNs/FCM registration +
  foreground bridge; beta-labeled.
- [ ] 6.4 Live Activities: ActivityKit token registration (P3 route),
  priority-5 state updates from coalescer; template maps sync row → activity
  update; experimental.

## Closeout

- [ ] 7.1 Honest bench: push coalescing factor + fan-out latency delta with
  push on/off (RESULTS.md, same-stage comparison rules).
- [ ] 7.2 Docs: READMEs (all SDKs), `docs/api/*.md` push sections, footgun
  callout (token/credential config = the new `NOSTOS_WRITE_TABLES`-style step).
- [ ] 7.3 Security pass: token trust boundary, template tenant isolation,
  sign-out deregistration on every SDK (verification-before-completion gate).
