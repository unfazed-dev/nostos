# Nostos integration: sdk_tauri + sdk_flutter + nostos-push → arxa / arxa studio

**Status:** ready to delegate. Non-blocking for the mobile track — integration lands when each track hits its gate.
**Nostos repo:** `/Volumes/developer_ssd/Developer/nostos` (read as source of truth; do not fork behavior — upstream fixes go in nostos itself).
**Priority order:** Track A (nostos_tauri) → Track B (nostos-push production consume) → Track C (nostos_flutter for client apps) → Track D (iroh transport ADR upstream — required, not optional).

## Verified ground truth (2026-08-26 audit)

- `sdk/nostos_tauri` — feasibility scaffold, NOT a polished SDK. One Rust file `sdk/nostos_tauri/src/lib.rs` (~1,156 lines), standard Tauri 2 plugin (`.plugin(tauri_plugin_cairn::init())`). Commands: connect, subscribe, write, query, checkpoint, watch, set_token, sign_out. No `.d.ts`, no JS/TS package, no docs beyond README invoke examples. **Zero push code** (no matches in `src/lib.rs`).
- `sdk/nostos_flutter` — flagship. ~14.2k LOC, CHANGELOG, 9 test files + integration_test, iOS/Android/macOS/web.
- `nostos-push` — server side is real and tested: 36 integration tests against a spawned axum listener (`crates/nostos-push/tests/daemon.rs`), APNs ES256 / FCM OAuth2 / Web Push VAPID hand-rolled in Rust (`crates/nostos-infra/src/push/`). Three modes: embedded PushRouter, standalone `nostos-pushd`, delegation via `RemoteNotifier`.
- **Real-device push: CONFIRMED.** `apps/atlet/flutter/tool/push_smoke.sh` + `PUSH_SMOKE.md` drive the full real rail (device token → nostos-server → FCM HTTP v1 → device), no mocks. Operator reality checks "all bit live on 2026-08-16" (paid Apple team provisioning, PLA acceptance, .p8 upload, iOS 14+ JIT/profile-build behavior — these errors only occur on a physical device path). Local (uncommitted, operator-owned) `GoogleService-Info.plist` and `google-services.json` exist in the atlet tree, and `build/ios/{Debug,Profile,Release}-iphoneos/Runner.app` device builds are present. Lock-screen action banners with app killed are documented from observed behavior.
- Caveats to carry into production use: the plan-checklist E2E (item 3.3) runs on **fake rails**; real-rail smoke is env-gated. iOS **simulator** cannot receive FCM; Android emulator leg is fully real FCM. `firebase_messaging` has **no macOS implementation** — desktop studio gets push via nostos WS session, not APNs.
- nostos-push production posture (**updated 2026-09-01** — the 2026-08-26 gap list is history, verified against source + 71 green tests): transient-5xx retry **CLOSED** (B2 2026-08-27: `RetryPolicy` re-debounce, receipts written only at the terminal attempt); per-tenant rate limits **CLOSED** (per-tenant token buckets with per-key `(rate_per_sec, burst)` overrides + `Retry-After`); API-key hashing **CLOSED** (`nostos push key add/list/revoke` — SHA-256 at rest, raw secret printed once; store keys win over env). Still true today: Web Push killed-tab shows the notification but no data wake / no re-subscribe on `pushsubscriptionchange` (documented limitation); Live Activities deferred (0.2.0 rail-delegation ponytail, `remote.rs`); Capacitor native bridge beta.

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

### Track A status (2026-09-01 — verified against the tree, the 2026-08-26 audit trail above is history)

- **A1 DONE** (code): `guest-js/` ships the typed `@nostos-sync/tauri` package (ESM + `.d.ts` + README, no build step) with the raw tier AND the unified-verb sugar tier. The dx-audit verb set is now COMPLETE end-to-end: `orSetAdd`/`orSetRemove`/`counterIncrement`/`counterDecrement`/`deadLetters`/`connectionState` landed as Rust commands + JS wrappers (`187ac9f`), alongside the earlier `upsert`/`patch`/`deleteRow`/`writeBatch`/`watchRows`/`fetchAll` sugar. Remaining: npm PUBLISH (operator). The one-table ceiling was lifted 2026-09-21 (`plugins.cairn.tables`).
- **A2 DONE**: `NostosPluginConfig` (the `plugins.cairn` block, camelCase, deny-unknown) + `example.tauri.conf.json` + full Tauri 2 ACL — 16 autogenerated `allow-*` permissions in `permissions/default.toml`, example capability in `guest-js/`. The 2026-08-26 'no tauri.conf.json anywhere' claim predates this.
- **A3 DONE**: `register_push_token`/`deregister_push_token` commands + sign-out auto-deregistration (ADR-0037 §3), tested.
- **A4 DONE (2026-09-21)**: rail is `make sdk-e2e tauri` — 19 unit tests, the ported adapter-conformance suite (`tests/conformance.rs`, items 1–5 + item 6 multi-table), and the fixture app `sdk/nostos_tauri/fixture/` (`cargo run` = two-table window; `tests/ipc.rs` drives the frontend's exact `InvokeRequest`s through `tauri::test::get_ipc_response` + the capabilities ACL against a live spine). One-table ceiling lifted: `plugins.cairn.tables[]` → `SyncClient.extra_tables`; unlisted tables refused naming the set. Per-table `where_sql` landed later that day (`plugins.cairn.whereSql`); per-table `resume_lsn` is not a thing (LSN is stream-global, ADR-0022). Click-level WebDriver rail (WebdriverIO `@wdio/tauri-service`, embedded driver — v2.tauri.app/develop/tests/webdriver confirms macOS support) deliberately NOT built: the `tauri::test` IPC fixture already pins the command boundary, and the rail would add a node toolchain + two Tauri plugins + a `tauri build` to a Rust fixture for click coverage of a two-table demo window. Revisit when `arxa/desktop` has UI worth clicking.
- **A5 OPEN (owner/device)**: the iOS-device + Android-emulator gate with a real doorbell — needs a fixture app built on the published plugin.


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

## Status addendum — 2026-08-28 gap-closure session

- **Track B3 token-minting seam — CLOSED** (arxa `7053c607`): `tauri-plugin-mobile-push` 0.1.4 mints APNs/FCM tokens in the Tauri mobile shell; `set_push_token` is now actually called; both mobile targets `cargo check` green (iOS at the project's 14.0 deployment floor, Android with NDK). Operator seam remaining: `google-services.json` into `gen/android/app/`, iOS Push Notifications provisioning.
- **Engine-side `/v1/send` — EXISTS, no build needed.** nostos-side sender is `RemoteNotifier` (`crates/nostos-infra/src/push/remote.rs`), wired in `nostos-server` when `NOSTOS_PUSH_REMOTE_URL` + `NOSTOS_PUSH_REMOTE_KEY` are both set; delegation e2e green in `nostos-push/tests/delegation.rs`. Arxa's engine (Node sidecar) has no doorbell trigger yet by design — "a future notifications surface" (`desktop/src-tauri/src/pushd.rs:23`) riding the kit-plan review (owner decision). Rail credentials (APNs .p8 / FCM service-account JSON) go in `<app-local-data-dir>/pushd.env`; `nostos push check` validates the mint live.
- **Track D — ADR-0041 ACCEPTED 2026-08-29** (merge `2e6cb9c`; decision memo `docs/plans/adr-0041-decision-memo.md`). Spike green: ws/iroh conformance parity re-run at `680852f`, iroh off-default everywhere, iOS/Android build viability verified. **Accept-gated items (now tracked work, conditions in ADR-0041 §Acceptance):**
  - D5. **Field leg** — phone on cellular pairs via QR and completes an offline→online resume through the relay path. Blocks any consumer defaulting to `iroh://`.
  - D6. ~~**Native `run_session` refactor**~~ — **CLOSED 2026-08-29** (`da772aa`): session core generic over the frame `Stream`/`Sink`; iroh accept loop runs the WS handshake natively per QUIC stream and drives it directly — bridge and its loopback hop deleted; auth parity pinned by `iroh_auth_rejects_bad_token`; HTTP binds NOSTOS_BIND in both modes.
  - D7. ~~**Flutter/tauri SDK wiring**~~ — **CLOSED 2026-08-29** (`4c7de71` + `599cbf1`): `nostos_flutter_rust` gains an `iroh` cargo feature forwarding to `nostos-client/iroh` (default OFF); no FRB signature change — `connect(url, …)` was already scheme-agnostic, regen diff is doc-comments only. Without the feature an `iroh://` URL fails loudly (`reject_iroh_scheme`, unit-tested); Dart `NostosConfig` accepts the `iroh` scheme; the build hook's source-build path opts in via `NOSTOS_FLUTTER_CARGO_FEATURES=iroh` (prebuilt binaries never carry it). Off-default posture kept: prebuilt = iroh-less; only D5 (field leg) now gates consumer defaulting. Arxa-side enablement is a build-env choice, no code change needed there.
  - D8. ~~**Self-hosted relay guidance + n0-fleet privacy note** in operator docs~~ — **CLOSED 2026-08-29** (`c060bee`): `NOSTOS_IROH_RELAY_URL` seam (`parse_relay_url` unit-tested; conformance 3/3 + clippy green); OPERATING.md §9 carries the self-host how-to (`iroh-relay --dev` :3340 / TLS config), the n0-fleet privacy note (relay sees metadata only, payloads are device-keyed E2E QUIC; `iroh.link` discovery publishes endpoint-id↔addr), and the honest limitations (discovery + client-side home relay stay n0 until D7's SDK knobs).
- **ci gate:** main was fmt/clippy-red from earlier commits; fixed and green as of `489ef7c` (ADR-0040 regression tests `18df3a2` included).

## Grilled decisions — 2026-09-21 (readiness for Flutter / Tauri / Web adoption)

Session: `/grill-with-docs` on "is nostos usable in flutter/dart/tauri/web projects already".
Ground truth re-verified against the tree that day: nothing is published (pub.dev, npm,
crates.io); `hook/prebuilt.json` is all placeholders (every consumer cargo-builds);
`arxa/kit/nostos` pins `nostos_flutter` by git SHA `fabc1a1` (on origin); `arxa/desktop` is
the Tauri consumer; no JS/TS app in arxa imports `@nostos-sync/web` yet.

1. **Readiness bar = internal (arxa) only.** Git-SHA pin + Rust toolchain on every dev/CI
   box + cargo-build fallback is the accepted cost. Publication (push, tag, pub.dev/npm/crates.io,
   prebuilt manifest, stranger test) stays the v0.2 operator gate — unchanged.
2. **Flutter native: adopt as-is** via the kit's SHA pin. iOS/Android build-hook firing is
   verified in `arxa/mobile_flutter`'s device runs, not in this repo. Housekeeping when
   convenient: `flutter_rust_bridge` `=2.13.0-beta.5` → `2.13.0` — DONE 2026-09-21 (regen with codegen 2.13.0);
   README line "native assets are still an opt-in flag" is 3.12-era — dart.dev/tools/hooks
   (Dart 3.13) documents build hooks as first-class; re-verify on Flutter 3.47.
3. **Tauri: fixture app first, multi-table lifted inside it.** `arxa/desktop` needs many
   tables, so the v1 one-table `NostosState` assert (`sdk/nostos_tauri/src/lib.rs:104`) is a
   real blocker. The lift is plumbing only: `plugins.cairn.tables[]` config → `SyncClient`'s
   existing `extra_tables` (ADR-0022, server cap 32). DONE 2026-09-21 — see Track A4 status. The provider-dashboard multi-table demo plan is NOT
   required for this and stays "proposed".
4. **Web = both, neither deferred.** (A) Flutter-web via the ADR-0036 engine — needs
   `kit/nostos` web wiring and a fix to the `nostos_flutter` README platform row that still
   says "Web: punted". (B) `@nostos-sync/web` for standalone JS/TS clients. The additive wasm
   `subscribe()` already gives both paths multi-table; the first-table-only checkpoint
   (`crates/nostos-ffi-wasm/src/lib.rs:1320`) is correct because LSN is stream-global — leave,
   document. Ceilings to close before a real consumer are listed in
   `docs/research/web-sdk-best-practices-2026-09-21.md`.
