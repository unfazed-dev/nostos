# Nostos integration: sdk_tauri + sdk_flutter + nostos-push → arxa / arxa studio

**Status:** ready to delegate. Non-blocking for the mobile track — integration lands when each track hits its gate.
**Nostos repo:** `/Volumes/developer_ssd/Developer/nostos` (read as source of truth; do not fork behavior — upstream fixes go in nostos itself).
**Priority order:** Track A (nostos_tauri) → Track B (nostos-push production consume) → Track C (nostos_flutter for client apps) → Track D (iroh transport ADR upstream — required, not optional).

## Verified ground truth (2026-08-26 audit)

- `sdk/nostos_tauri` — feasibility scaffold, NOT a polished SDK. One Rust file `sdk/nostos_tauri/src/lib.rs` (~1,156 lines), standard Tauri 2 plugin (`.plugin(nostos_tauri::init())`). Commands: connect, subscribe, write, query, checkpoint, watch, set_token, sign_out. No `.d.ts`, no JS/TS package, no docs beyond README invoke examples. **Zero push code** (no matches in `src/lib.rs`).
- `sdk/nostos_flutter` — flagship. ~14.2k LOC, CHANGELOG, 9 test files + integration_test, iOS/Android/macOS/web.
- `nostos-push` — server side is real and tested: 36 integration tests against a spawned axum listener (`crates/nostos-push/tests/daemon.rs`), APNs ES256 / FCM OAuth2 / Web Push VAPID hand-rolled in Rust (`crates/nostos-infra/src/push/`). Three modes: embedded PushRouter, standalone `nostos-pushd`, delegation via `RemoteNotifier`.
- **Real-device push: CONFIRMED.** `apps/atlet/flutter/tool/push_smoke.sh` + `PUSH_SMOKE.md` drive the full real rail (device token → nostos-server → FCM HTTP v1 → device), no mocks. Operator reality checks "all bit live on 2026-08-16" (paid Apple team provisioning, PLA acceptance, .p8 upload, iOS 14+ JIT/profile-build behavior — these errors only occur on a physical device path). Local (uncommitted, operator-owned) `GoogleService-Info.plist` and `google-services.json` exist in the atlet tree, and `build/ios/{Debug,Profile,Release}-iphoneos/Runner.app` device builds are present. Lock-screen action banners with app killed are documented from observed behavior.
- Caveats to carry into production use: the plan-checklist E2E (item 3.3) runs on **fake rails**; real-rail smoke is env-gated. iOS **simulator** cannot receive FCM; Android emulator leg is fully real FCM. `firebase_messaging` has **no macOS implementation** — desktop studio gets push via nostos WS session, not APNs.
- Known nostos-push production gaps (from nostos source): daemon has no retries in v1 (process-wide, not per-tenant, rate limits — `coalescer.rs:31`, `limit.rs:19`); API keys plaintext env, hashing deferred (`auth.rs:23`); Web Push killed tab shows notification but no data wake / no re-subscribe on `pushsubscriptionchange`; Live Activities experimental/unsupported by daemon; Capacitor native bridge marked beta.

## Constraints (do not violate)

1. **Supabase decoupling.** The Supabase database belongs to arxa digital solutions (parent company), not to arxa studio users. Free users have no database. Every nostos feature must work identically with local-only storage; server sync is an optional upgrade. No hard dependency on `supabase/` anywhere in the integration.
2. **Mobile transport decided:** iroh embedded, QR pairing minted by the desktop app, any number of phones, full studio UI, TestFlight + APK distribution. Nostos rides inside that tunnel; it does not replace it.
3. Nostos fixes are made **in the nostos repo** (its owners' conventions, ADRs 0032/0037/0038 govern); arxa-studio only consumes tagged versions. No vendoring.

## Track A — nostos_tauri: scaffold → production SDK (priority)

- A1. JS/TS guest bindings: typed `@nostos-sync/tauri` npm package wrapping `invoke("plugin:cairn|…")` — mirror `nostos_client::SyncClient(SqliteStorage)` surface; ship `.d.ts`, ESM, README.
- A2. Config story: today no `tauri.conf.json` exists anywhere in the nostos repo — add plugin config (sync URL, token, tables) + capabilities/permissions entries for Tauri 2 ACL.
- A3. Push registration parity: add `register_push_token` / `deregister on sign_out` commands calling the same REST as flutter (`NostosDatabase.registerPushToken(platform, token)` pattern, `nostos_database.dart:645`). On iOS/Android the token comes from the Tauri mobile shell (APNs/FCM native hooks); desktop registers a WS-session "push" (no OS rail).
- A4. Test rail: port the flutter conformance shape (`apps/atlet/spec/adapter.md`, `test/adapter_conformance_test.dart`) to a Tauri fixture app; wire into nostos CI next to the 7 existing Rust tests.
- A5. Gate: fixture app on iOS device + Android emulator syncs a table offline→online round-trip and receives a real doorbell via the push_smoke harness (`PUSH_SMOKE_DEVICE=ios` leg).

## Track B — nostos-push: consume as production service

- B1. Stand up `nostos-pushd` (one binary, SQLite registry, no Postgres needed) as the arxa push service; config via `nostos push init/check` (validates rails; APNs .p8 live-mint + FCM live token mint already proven).
- B2. Close the gaps that matter for an agency tool: per-tenant rate limits (upstream PR), key hashing (`auth.rs:23`), retry policy for transient APNs/FCM 5xx (upstream); document Web Push limitation instead of fixing it now.
- B3. Arxa studio wiring: desktop engine (Mac) runs embedded PushRouter or points at `nostos-pushd`; phones register tokens over the iroh tunnel at QR-pair time; doorbells target offline devices only (nostos semantics — online sessions get WS, no double-signal).
- B4. Credentials are operator-owned: APNs .p8 + team/bundle ids, FCM service-account JSON, stored in studio's local keystore — never in the repo, never in Supabase. Free users: push still works — `nostos-pushd` runs on the user's Mac beside the engine; no cloud required.
- B5. Gate: `push_smoke.sh` green on Android emulator (automated) + one physical iPhone run (manual, mirrors atlet's proven procedure).

## Track C — nostos_flutter: for arxa client apps (when clients need offline-first)

- C1. Pin `nostos_flutter 0.1.0` + `flutter_rust_bridge 2.13.0-beta.5` compatibility; track nostos's v0.1.0 tag.
- C2. Reuse atlet as the reference: `lib/adapters/nostos_adapter.dart` (side-by-side with `powersync_adapter.dart`) and `lib/push/push_pilot.dart` (FCM mobile + VAPID web, `--dart-define=ATLET_PUSH_PILOT=true`, `nostosDoorbellBackgroundHandler` background isolate).
- C3. Note for client work: adapter-conformance pilot ran without a live backend (only checklist item 5 genuinely passed for either adapter) — a live-backend conformance pass is the first task when a client project actually adopts it.
- C4. Gate: a minimal arxa-branded flutter fixture passes the conformance suite against a live nostos-server, offline→online, with one real push received.

## DX ergonomics ground truth (nostos-dx-audit, 2026-08-27)

- **Watch is already push-invalidation, not polling** — `write_notify: Notify` (nostos-client/src/client.rs:490, fired :746/:789/:952) + `tokio::sync::watch` channels; comment :481 "re-emit after every applied batch instead of polling". The livestore-style reactive core exists; the redesign is surface work, not engine work.
- **CRDT verbs live in core but only Flutter + Web expose them** — `or_set_add`:799, `or_set_remove`:810, `counter_increment`:867, `counter_decrement`:878 in nostos-client; zero hits in node/capacitor/RN/tauri/kotlin/dotnet surfaces. Opt-in tables must match server `NOSTOS_OR_SET_COLUMNS`/`NOSTOS_COUNTER_COLUMNS` (client.rs:224-233).
- **Leverage point:** Swift/Kotlin/.NET are UniFFI-generated; node is napi; tauri wraps core — one core change (typed upsert/patch/delete/writeBatch, structured predicate per ADR-0032 Waves 2+, CRDT verbs) propagates to 5 SDKs automatically. Exception: `nostos-ffi-wasm/src/lib.rs:768` has a **duplicated** `or_set_add` (":765 ponytail — rewire") that must be rewired by hand in any core CRDT DX change.
- **No query builder in core** — each SDK hand-rolls where/order strings; the uniform watch-verb design should land the predicate in core once.
- **Naming to standardize on (majority)**: `connect, subscribe/subscribeTables, watch/watchSql, upsert, patch, delete, writeBatch, orSetAdd/orSetRemove/counterIncrement/counterDecrement, setToken, signOut, connectionState, deadLetters`. Raw-tier byte-level `write` goes internal-only.
- **Impact on tracks:** A1's `@nostos-sync/tauri` surface should target the unified verb set (not today's raw tier); C-track flutter is already the rich-tier reference.

## Track D — iroh transport ADR (upstream proposal, TO DO)

- D1. Write and submit a nostos ADR proposing a first-class transport abstraction: `--transport ws|iroh` on nostos-server and a matching client dial layer (`ws://` vs `iroh://`). The sync protocol is transport-agnostic (WebSocket framing at `/sync`, `crates/nostos-server/src/main.rs:909`); iroh replaces TCP/TLS only — NAT traversal, hole-punching, and device-keyed encryption for free. Topology stays hub-and-spoke; no change to server-authoritative LWW, LSN ordering, or the CRDT tier.
- D2. Scope guard in the ADR: explicitly out-of-scope — serverless mesh/P2P sync (rejected in nostos's CRDT ADR rationale; would require CRDT semantics on every table and a causal-ordering protocol). The ADR is transport-only.
- D3. Arxa payoff: removes our tunnel-wrapping glue (today nostos's WS rides inside the arxa iroh tunnel); once upstream, studio dials `iroh://` directly at QR-pair time.
- D4. Gate: ADR accepted upstream (or explicitly rejected with rationale recorded here); if accepted, a spike branch shows the fixture app from A5 syncing over an iroh endpoint.

## Sequencing / delegation

- Tracks are independent; A is priority. Each track is one agent, working in the nostos repo (A, B upstream parts, C) with a thin consume-side commit in arxa-studio per gate.
- Nothing here blocks the mobile shell work (iroh + QR + Tauri iOS/Android); integration point is a single `@nostos-sync/tauri` dependency + push-token registration call once Track A gate passes.
