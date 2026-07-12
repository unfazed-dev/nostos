# Nostos PowerSync-SDK parity plan

Date: 2026-07-12. Sources: PowerSync official docs (pub.dev `powersync` v2.3.1,
docs.powersync.com) fetched 2026-07-12; Nostos surface verified by file:line
inventory. Companion: `supabase-flutter-smoke-results.md` (engine proven 18/18
vs live Supabase), `launch-readiness-gap-list.md`.

## Goal

"Nostos must ship the equivalent of the PowerSync SDK." This plan defines
*equivalent* concretely, gap-analyzes the current Nostos SDK against it, and
sequences the work. It is a **plan**, not an implementation — per the operator's
standing scope. Implementation starts on explicit go.

PowerSync is the DX bar for Supabase+Flutter local-first. Its client SDKs are
Apache-2.0; its self-host service is FSL-1.1 (→ Apache-2.0 after 2 yrs). Nostos's
moat is Rust throughput (~200× PowerSync's ops/sec) + full Apache-2.0 +
server-enforced tenancy. Parity means: match the **client DX**, not the service
architecture.

## The 5 must-match features (from PowerSync research) + Nostos gap

| # | PowerSync feature | Nostos today (verified) | Gap | Severity |
|---|---|---|---|---|
| 1 | `watch(sql)→Stream<ResultSet>` — re-runs on table mutation, `triggerOnTables`, `throttle` | `watch(table)→Stream<List<Map>>` re-emits the **full table** per commit (`sdk/nostos_flutter/lib/src/nostos.dart:94`); `Storage` trait has only `checkpoint`+`apply_batch` (`nostos-core/src/storage.rs:50`); `rows_for` returns opaque bytes, documented "not SQL-queryable" (`storage.rs:20`, `sqlite.rs:139`) | No SQL watch, no differential, O(rows) re-emit per commit | **High for parity credibility** — but NOT v1-launch-blocking (table-watch suffices for a todo app) |
| 2 | Sync Streams — parameterized per-client queries, JOIN/CTE, lazy `syncStream(name,params).subscribe()` | server-side `where_sql` predicate + `tenant_column` (ADR-0011/0012); one where-clause per subscription | Less expressive; no on-demand lazy subscriptions | Medium |
| 3 | Durable retrying upload queue + developer-owned `uploadData(CrudTransaction)` backend | durable outbox (enqueue/pending/mark_done, in-order, survives restart — `offline_writes.rs`); **retries forever, head-of-queue blocking, no dead-letter** (`client.rs:31-32` ponytail); write path collapsed into nostos-server `PgWriteBack` (no dev backend) | No dead-letter; no dev-owned write backend | Medium (dead-letter = debt; dev-backend = architectural decision, see below) |
| 4 | Bucket dedup + 1,000–10,000 bucket budget per client | no "buckets"; shared publication + per-client predicate + sharded router (Phase 2) for scale | Different scalability model (router, not buckets) | Low — not a user-facing gap |
| 5 | Column-level PATCH LWW + op-types `PUT`/`PATCH`/`DELETE` + idempotency | `op ∈ {upsert, delete}` whole-row; `ON CONFLICT DO UPDATE` (last-row-image); idempotency at apply layer (`apply_idempotency_premise.rs`) | No PATCH (column-level); no op-type-aware semantics | Medium-High for collaborative-edit use cases |

## The load-bearing architectural decision (operator to ratify)

**PowerSync splits read and write paths:** reads via server fan-out (Sync
Streams); writes via a **developer-implemented** `uploadData(CrudTransaction)`
that replays the local queue through the dev's own backend (supabase-js + RLS).
Conflict resolution lives in *that* backend.

**Nostos collapses both** into one WS stream: the server applies client writes
directly via `PgWriteBack`, server-enforces the tenant (ADR-0018), and the
resulting mutation re-replicates back (idempotent — ADR-0013 addendum).

| | PowerSync (split) | Nostos (collapsed) |
|---|---|---|
| Dev writes a backend? | **Yes** — `uploadData` is required | **No** — server auto-applies |
| Tenant enforcement | client-trusted params + RLS | **server-enforced predicate** (stronger) |
| Custom conflict/validation | in dev backend (full control) | none today (whole-row LWW) |
| Plug-and-play DX | more moving parts | **fewer** (better for the ≤5-min bar) |

**Recommendation:** *Preserve* Nostos's collapsed model as the default — it is a
DX advantage for the stated launch bar ("better experience than PowerSync for
Supabase+Flutter") and pairs with the stronger server-enforced tenancy. *Add*
an **opt-in `UploadConnector` hook** (workstream P4) so advanced users can
intercept writes for custom validation/conflict — matching PowerSync's
flexibility without forcing every dev to write a backend. Do **not** fork to
PowerSync's split model.

## Workstreams (sequenced; P1–P3 are the parity credibility core)

- **P1 — SQL-level reactive `watch()` (the headline).** Make on-device data
  SQL-queryable: decode the opaque payload bytes into typed columns in a real
  SQLite schema, then add `watch(sql, {triggerOnTables, throttle})` that re-runs
  the query when a touched table mutates (hook the existing
  `subscribe_changes` broadcast). Kills the O(rows) full-table re-emit. Touches
  `nostos-core/storage.rs` (Storage trait gains a query surface), `SqliteStorage`
  schema, and the Flutter `watch()` API. Largest item; the biggest DX win.
- **P2 — Outbox dead-letter policy.** Replace retry-forever head-of-queue
  blocking (`client.rs:31-32`) with max-retries → DLQ + observable signal. Small,
  removes a known footgun. (Already on the gap-list C8.)
- **P3 — PATCH / column-level writes.** Extend `WriteOp` to `{Put, Patch,
  Delete}`; add a column-scoped wire frame; `PgWriteBack` does targeted
  `UPDATE SET (changed cols)`. Op-type-aware + idempotent (matches PowerSync's
  PUT/PATCH/DELETE contract).
- **P4 — Opt-in `UploadConnector` hook.** Let a dev register a write
  interceptor (the Nostos analog of `uploadData`) for custom validation/conflict,
  defaulting to the auto `PgWriteBack`. Preserves the collapsed model's DX while
  matching PowerSync's flexibility.
- **P5 — Declarative sync-rules expressiveness.** Move beyond a single
  `where_sql` per subscription toward Sync-Streams-style parameterized queries
  + on-demand `subscribe(name, params)` (lazy sync). Phase-2 flavored; defer
  until P1–P3 land.
- **P6 — Schema contract + schemaless views.** Adopt/derive the `id text
  primary key` convention; consider PowerSync's "sync schemaless data, apply
  client schema via SQLite views" model (it removes explicit client migrations
  — relevant to P1).

## Implementation status (2026-07-12)

- **P1 — SQL-level reactive `watch()`: ✅ SHIPPED + verified end-to-end.**
  Rust `SqliteStorage::query(sql)` (json_extract against `cairn_data`; JSON1
  in the bundled SQLite; on the **concrete type, not the `Storage` trait** —
  preserves WASM purity) → FFI `NostosHandle::query` (frb codegen, matching
  version) → Dart `Nostos.watchQuery(sql)` (re-runs on the change-tick, decodes
  JSON). 3 Rust tests + 2 Dart tests; workspace `cargo check` + `dart analyze`
  + `flutter test` all green. The #1 PowerSync differentiator is matched.
- **P2 — outbox dead-letter: ✅ SHIPPED + verified.** `dead_letter_max_attempts`
  (default 50) → **quarantine-not-delete** (head advances past a permanent
  rejection, write retained in `dead_letter_entries()`). 3 Rust tests;
  workspace-green. Resolves the old `ponytail:` retry-forever debt.
- **P3 — PATCH / column-level writes: ✅ SHIPPED + verified.** `WriteOp::Patch`
  + wire + `PgWriteBack` targeted UPDATE (only patched columns) with the tenant
  force-stamp + CTE-EXISTS guard (ADR-0018 — UPDATE's WHERE sees the pre-update
  row, so the tenant col is force-stamped; a cross-tenant patch → Forbidden, row
  unchanged). 3 new PG-gated e2e tests green vs **live Supabase**
  (`patch_updates_only_specified_columns`, `patch_on_absent_row_is_ok`,
  `cross_tenant_patch_is_rejected_row_unchanged`) + the 8 prior writeback tests
  = 11/11. No codegen needed (`write()` takes `op` as a string).
- **P4 — opt-in `UploadConnector`: deferred** (builds on P3's WriteOp model).
- **P5/P6: deferred** (Phase-2).

ADR-0013 v2 addendum records the P1+P2 decisions (quarantine-not-delete;
query-on-concrete-type). `supabase-flutter-smoke-results.md` is the engine
proof these changes build on (18/18 vs live Supabase).

## Preserve (moat — do NOT regress)

- Rust fan-out throughput (142k–833k ops/sec vs PowerSync 2k–4k) — `benches`.
- Apache-2.0 end-to-end (vs FSL-1.1 self-host).
- Server-enforced tenant isolation (ADR-0011/0018) — stronger than client-trusted.
- Zero-backend-write DX (better plug-and-play than PowerSync's required `uploadData`).
- Typed payloads server-side (ADR-0019) — proven live in the smoke campaign.
- Human-debuggable JSON wire (ADR-0012) — keep until a measurement says otherwise.

## Deliberately do NOT copy (YAGNI)

- Incremental/differential `watch` (PowerSync JS-only; basic `watch` parity is enough for v1).
- Non-Postgres sources (Mongo/MySQL/SQLServer) — Postgres-first.
- The "bucket" abstraction as a user-facing concept (Nostos's router handles scale; buckets are an implementation detail).
- PowerSync's Monaco config editor / hosted dashboard — `nostos` CLI + `nostos.toml` suffice.

## Sequencing vs the launch bar

P1 is **parity credibility**, not launch-critical: the ≤5-min todo-app launch
bar is met by today's table-level `watch()` + the proven engine (18/18). Launch
sequence is unchanged: stranger test + launch ops (gap-list §A). P1–P3 should
land early in the post-launch parity push so the broader "PowerSync equivalent"
claim is true, not aspirational. P4–P6 follow.

## Open questions for the operator

1. Ratify the **collapsed read/write model + opt-in UploadConnector** decision
   (vs forking to PowerSync's split). This shapes P3/P4.
2. Is PATCH/column-level writes (P3) in scope for the first parity release, or
   is whole-row upsert acceptable until a design partner needs it?
3. Priority of P1 (SQL-watch) vs P5 (sync-rules expressiveness) — both are
   "PowerSync-equivalent" claims; which matters more for the target wedge?

## Full-parity audit (2026-07-12, SDK-breadth sweep)

Prompted by: "does nostos cover all the PowerSync SDKs, and is nostos now at
parity or better?" Method: fable 5-gate; PowerSync surface from official docs
(docs.powersync.com, pub.dev, npm registry) + Nostos surface verified from
source (file:line). **This section supersedes the 5-feature frame above for the
parity question** — that frame was a Flutter-only lens and undercounted both
the SDK surface and the feature catalog.

> **Update (2026-07-12): all 7 shipped SDKs are now LIVE-replication-E2E-verified**
> (PUSH server→client + ECHO client→server→client, through each SDK's real public
> API, against the shared no-docker spine). Flutter (docker PG) + Rust (in-process)
> were already live; Node/Tauri/Swift/Kotlin/Web were wired + verified this session.
> Run them: `make sdk-e2e`. Full record: `docs/plans/sdk-live-e2e-consolidation.md`.

### SDK platform coverage (the dimension the original plan never scored)

PowerSync ships **10** client SDKs; Nostos ships **2** (3 counting the native
Rust `nostos-client`):

| Platform | PowerSync | Nostos today |
|---|---|---|
| Flutter (Dart) | ✅ GA `powersync` 2.3.1 | ✅ `sdk/nostos_flutter` (frb) |
| Web / WASM | ✅ GA `@powersync/web` 1.39.0 | ✅ `crates/nostos-ffi-wasm` + `sdk/nostos_web` (`@nostos-sync/web`, PowerSync-style API; node smoke `SMOKE_OK`; browser WS via web-sys — OPFS deferred) |
| React Native | ✅ GA `@powersync/react-native` 1.35.9 | 🟢 `sdk/nostos_react_native` — TurboModule over nostos_kotlin/nostos_swift UniFFI (ADR-0020; Hermes has no WASM so the JS core is facade-only); Android emu live PUSH+ECHO E2E verified (`xml_failures=0`); iOS TurboModule fast-follow |
| Node | 🟡 Beta `@powersync/node` 0.19.4 | 🟡 `sdk/nostos_node` (napi-rs) — **loads in node, async query round-trips `EXIT=0` (verified independently)**; offline-only (no live `subscribe`/replicator path yet) |
| Kotlin (KMP) | ✅ GA `com.powersync:core` 1.12.0 | 🟢 `sdk/nostos_kotlin` (UniFFI) — **device-verified**: `connectedDebugAndroidTest` PASS on Android 14 (API 34, arm64-v8a, **4KB pages**) — `NostosClient` construct + `connect` + `query("SELECT 1 AS one")` round-trip (`failures=0`, verified independently). JNA/Android-16 16KB-page wall confirmed as root cause; API 34 sidesteps it. `forbid(unsafe)` |
| Swift (iOS/macOS) | ✅ GA `powersync-swift` | 🟢 `sdk/nostos_swift` (UniFFI) — **sim-E2E proven**: `connect`+`query` ran on the **iPhone 17 sim** (iOS 26.5, captured stdout, independently reproduced); Rust compiles host + iOS + sim; `forbid(unsafe)`. SPM `.binaryTarget`/xcframework packaging is the remaining polish |
| Capacitor | 🟡 Beta | 🟢 `sdk/nostos_capacitor` — web-only v8 plugin over `@nostos-sync/web`'s browser live path (webview WASM+WS unmodified); Playwright PUSH+ECHO E2E |
| Tauri | 🔴 Alpha | 🟡 `sdk/nostos_tauri` plugin (tauri 2) — compiles + 2 integration tests green (offline connect/query round-trip; write-before-connect contract); `forbid(unsafe)`; `subscribe` run-loop + JS bindings deferred |
| .NET | 🟡 Beta | 🟢 `sdk/nostos_dotnet` — UniFFI-CS Nord `v0.9.2+v0.28.3` over the nostos-client UniFFI surface (reuses nostos_swift/kotlin's `#[derive(uniffi::Object)]`); iOS/iOS-sim/Android cross-compile verified; generated `nostos.cs` committed; C# runtime E2E **live** (`dotnet/smoke` PUSH+ECHO vs the spine, 2026-07-13) |
| Rust | 🔴 Alpha | ✅ `crates/nostos-client` — native Rust SDK (`SyncClient`/`SqliteStorage`, `forbid(unsafe)`, live-Supabase-tested, README). Not yet on crates.io |

Catch-up is **cheap, not foundational**: `nostos-core` is WASM-clean
(`forbid(unsafe_code)`, deps = `nostos-domain`+serde only) with trait seams
(`Storage`, `Outbox`). Each additional SDK is a thin FFI bridge (UniFFI /
cbindgen / napi-rs) over `nostos-core` or `nostos-client`, per ADR-0015 — no
engine rewrite. The Flutter SDK already proves the pattern (path-deps to
`nostos-client`+`nostos-core`+`nostos-domain`, embedded tokio runtime).

### Feature parity (full catalog)

| Feature | PowerSync | Nostos | Verdict |
|---|---|---|---|
| basic `watch(sql)` re-run | all SDKs | Flutter `watchQuery` (P1) | ✅ parity |
| `triggerOnTables` / `throttle` | all SDKs | re-run every tick, no throttle | ⚠️ refinement gap |
| incremental / differential watch | **JS-only** | none | ⚠️ behind (PS is JS-only too) |
| Sync Streams (param, lazy, CTE/JOIN) | all 5 main SDKs | single `where_sql` predicate | ❌ behind (P5 deferred) |
| durable upload queue + retry | `uploadData` (5s retry, stalled detection) | durable outbox + dead-letter (P2) + auto `PgWriteBack` | ✅ parity, **different model** (Nostos = zero-backend) |
| op-types PUT/PATCH/DELETE | all SDKs | core+wire+server ✅; **Flutter bridge rejects `"patch"`** | ❌ NOT at SDK parity — see gap below |
| column-level LWW conflict | server-side per-field + override hooks | whole-row LWW (ADR-0014a); column-level at engine for Patch | ⚠️ partial (engine-only for Patch; no override hook = P4) |
| attachments / files | Alpha across SDKs | none | ❌ behind (Alpha on PS) |
| ORM integrations | Drift/Drizzle/Room/GRDB | none | ❌ behind |
| encryption (SQLCipher) | Beta across SDKs | none | ❌ behind |
| multi-source DBs | PG/Mongo/SQLServer/MySQL | PG only | deliberate (Postgres-first YAGNI) |

### GAP discovered this audit: PATCH is unreachable from the Flutter SDK

P3 (marked "✅ SHIPPED" above) is shipped at the **engine + wire + server**
layer — `WriteOp::Patch` (`crates/nostos-core/src/outbox.rs:169`), the `"patch"`
wire arm, and `PgWriteBack::patch` are all live and **proven vs live Supabase**
(3 e2e green, 11/11 writeback). But the **Flutter frb bridge was never wired**:
`sdk/nostos_flutter/rust/src/api/nostos.rs:230-238` matches only `"upsert"` /
`"delete"` — `"patch"` returns `Err("unknown write op ...")`. Meanwhile the
Dart doc (`sdk/nostos_flutter/lib/src/nostos.dart:136-138`) advertises `"patch"`
as supported ("P3 PowerSync PATCH parity"). **The SDK documents a PATCH it
rejects at runtime.** ✅ **RESOLVED 2026-07-12**: wired `"patch" => WriteOp::Patch`
+ doc comment at `sdk/nostos_flutter/rust/src/api/nostos.rs:230`; Flutter crate
`cargo check` clean; root `make ci` GREEN (`MAKE_CI_EXIT=0`, every suite
`0 failed`). PATCH is now reachable from the Flutter SDK — the doc-vs-runtime
lie is closed, and the engine+server layer (already live-proven vs Supabase) is
reachable end-to-end.

### Honest verdict

1. **Overall parity: NO.** Nostos is ahead on engine / throughput / license /
   tenant / write-DX, behind on SDK breadth (2/10) and SDK-surface feature
   completeness.
2. **For the launch wedge (Flutter + Supabase plug-and-play): effectively YES,
   and better** on the must-haves (subscribe, reactive `watchQuery`, durable
   writes, typed payloads) plus the moat (throughput, Apache-2.0,
   server-enforced tenant, zero-backend writes). The "better than PowerSync for
   Flutter+Supabase" claim is defensible **for that wedge alone**.
3. **"Equivalent of the PowerSync SDK" is true ONLY for Flutter today.** To
   make the broad claim true, sequence: (a) wire PATCH through the frb bridge
   (trivial), (b) P5 Sync Streams, (c) P4 opt-in UploadConnector, (d) the
   platform SDKs (RN/Node/Kotlin/Swift via UniFFI/napi — ADR-0015 mapped).
   Attachments / ORM / encryption are Phase-2/3.

### Actionables — status (2026-07-12 session push)

1. ✅ **DONE** — `"patch"` wired through the Flutter frb bridge; `make ci` green.
2. ✅ **DONE** — `throttle` / `triggerOnTables` on `watchQuery` (Dart-side
   trailing-edge debounce before the `asyncMap`, so it bounds the query rate;
   16/16 tests, `dart analyze` clean, no FFI/wire/engine change).
3. ✅ **DONE** — **Node SDK scaffold** (`sdk/nostos_node`, napi-rs). Builds
   `--release`; `node smoke.cjs` loads the addon + round-trips an async query
   through rusqlite+serde_json (`EXIT=0`, reproduced independently); `cargo
   clippy --release -- -D warnings` clean; purely additive (untracked dir), no
   hand-written `unsafe`. **First new platform — proves the cheap-catch-up
   thesis** (the Flutter `Runtime::new()` + `SyncClient` FFI pattern ported
   straight to napi). Honest scope: offline-only (no live `subscribe`/replicator
   path verified yet) + a `u64→f64` id-precision `ponytail:`. Nostos is now
   **10/10 platforms** (Flutter + JS-Web + Node + Rust + Tauri + Swift + Kotlin + Capacitor + .NET + React Native; RN/Capacitor/.NET landed 2026-07-12 — see `docs/plans/sdk-parity-final-three.md` + ADR-0020). Note: `nostos-ffi-wasm` + `sdk/nostos_web` are ONE platform (JavaScript Web, matching PowerSync's single `@powersync/web`); the authoritative figure vs PowerSync's 10 is now **10/10** (10/10 with a live-E2E path — .NET C# smoke live via `dotnet/smoke` 2026-07-13; RN-iOS TurboModule pending, RN-Android emu-verified).
4. P5 Sync Streams — biggest remaining *feature* gap for the "equivalent SDK"
   claim.
5. ✅ **DONE** — `nostos-client` documented as the Rust SDK (README + public-API
   surface); 4/10. crates.io publish is a release-op, not a code gap.
6. ✅ **DONE** — **Web JS SDK** (`sdk/nostos_web`, `@nostos-sync/web`). `wasm-pack
   --target nodejs` builds; node smoke `SMOKE_OK` / `EXIT=0` — 11 checks
   (require + write + query + watch snapshot + rowCount) against the apply
   engine; API is PowerSync-shaped. Browser live-WS rides the wasm's existing
   `NostosSocket` (web-sys); an automated browser-test of that path + a node WS
   adapter are the next increments (`ponytail:`-marked). **5/10.**
7. ✅ **DONE** — **Tauri plugin** (`sdk/nostos_tauri`, tauri 2). Compiles +
   `cargo test` 2/2 green (offline `connect`+`query` round-trip through
   `NostosState` → `SyncClient` → `SqliteStorage`; write-before-connect
   contract); `cargo clippy -D warnings` clean; `forbid(unsafe)`. The agent's
   build never finished, so 5 defects were caught + fixed only by independent
   compile/test (build.rs `Builder::new` command-args, Cargo `links`, the
   nested-`Result` flatten in `query`, a dead owned-runtime that dropped-panicked
   inside the test runtime, a wrong `SELECT 1` assertion) — the PROCESS LESSON
   in action. `subscribe` run-loop + JS bindings deferred (`ponytail:`-marked).
   **6/10.**
8. ✅ **DONE** — **Swift SDK** (`sdk/nostos_swift`, UniFFI 0.28). Rust compiles
   for host **and** `cargo build --target aarch64-apple-ios`; `cargo test --lib`
   2/2 (offline connect+query; write-before-connect); `cargo clippy
   --all-targets -D warnings` clean; UniFFI emits Swift bindings; `swiftc
   -typecheck` passes against the FFI header. `forbid(unsafe)`. Unlike Tauri,
   the agent's build FINISHED and reported accurately — every gate reproduced
   on my independent re-run. SPM `.binaryTarget`/xcframework wrapping the `.a`
   is the next increment (`ponytail:`-marked). **7/10.**
9. 🟢 **VERIFIED — Kotlin/Android SDK** (`sdk/nostos_kotlin`, UniFFI). **Tier 1
   verified independently**: `cargo test` 2/2, `libnostos_kotlin.so`
   cross-compiles for `aarch64-linux-android`, gradle `assembleDebug` → `.aar`
   bundling `jni/arm64-v8a/libnostos_kotlin.so` + `classes.jar`. **Tier 2
   device-verified (2026-07-12)**: the Android-16 (API 37) 16KB-page linker had
   rejected UniFFI-Kotlin's JNA `libjnidispatch.so` — an upstream ecosystem
   issue (all JNA libs on Android-16), NOT a Nostos bug. Fix = run on an API-34
   emulator (4KB pages, lax enforcement): booted `nostos_api34` AVD headless,
   `connectedDebugAndroidTest` → `BUILD SUCCESSFUL`, test report
   `TEST-nostos_api34(AVD) - 14-_-.xml` shows `<testsuite tests="1"
   failures="0" errors="0" skipped="0">` with `offline_connect_query_roundTrip`
   (NostosClient construct + connect + `query("SELECT 1 AS one")` asserting
   `"one":1` + `checkpoint()==0UL`) green. Verified independently (report
   re-read, `adb devices` confirms the AVD was cleaned up). On Android-16
   hardware the real fix remains JNA 5.17+ or a direct-JNI bridge (~1 day).
   `forbid(unsafe)`; `.so` gitignored as a build artifact. **Still 7/10**
   (Kotlin was already counted; this elevates it from asterisk to fully 🟢).

### Path to 10/10 (honest roadmap)

Each new SDK is a thin FFI bridge over `nostos-core` / `nostos-client` — Node
proved the pattern ports in a session. Order = value-per-effort, with
this-session verifiability noted:

| Platform | FFI strategy | Verifiable here? | Effort |
|---|---|---|---|
| ✅ Flutter | frb | yes (`flutter test`) | shipped |
| ✅ WASM/Web | wasm-bindgen | yes (`smoke.mjs`) | shipped |
| ✅ Node | napi-rs | **yes (`node smoke` EXIT=0)** | scaffold shipped |
| ✅ Rust SDK | `nostos-client` + README (native, `forbid(unsafe)`, live-tested) | yes (`cargo test`) | shipped (4/10) |
| ✅ Tauri | `sdk/nostos_tauri` plugin over `nostos-client` (Rust-native) | yes (`cargo test` 2/2) | scaffold shipped (6/10) |
| ✅ Web JS SDK | `sdk/nostos_web` (`@nostos-sync/web`) over `nostos-ffi-wasm` | yes (node smoke `SMOKE_OK`) | scaffold shipped (5/10) |
| React Native | reuse the JS core (RN shares it, like `@powersync/web`↔RN) | partial (Jest; full E2E needs device) | medium |
| 🟢 Kotlin/KMP | `sdk/nostos_kotlin` (UniFFI) over `nostos-client` | **yes (`connectedDebugAndroidTest` PASS on API-34 emu — construct+connect+query round-trip)** | device-verified (7/10) |
| ✅ Swift/iOS | `sdk/nostos_swift` (UniFFI) over `nostos-client` | yes (`cargo test` 2/2 + `swiftc -typecheck`) | scaffold shipped (7/10) |
| .NET | uniffi-cs / cbindgen + DllImport | ✅ live (`dotnet/smoke` PUSH+ECHO E2E 2026-07-13) | done |
| Capacitor | JS facade over the web JS core | partial | small-medium |

Remaining *feature* gaps (independent of platforms): P5 Sync Streams;
incremental/differential `watch` (PowerSync is JS-only there too); attachments;
ORM hooks (Drift/Drizzle/Room/GRDB); SQLCipher encryption.

Hardening items the push surfaced:
- `sdk/nostos_node` and `sdk/nostos_flutter/rust` do NOT set
  `#![forbid(unsafe_code)]` — napi/frb macros expand to inline `unsafe` (the
  documented FFI-glue exception, ADR-0015 addendum). Hand-written code in both
  is verified unsafe-free (grepped). Follow-up: scope `allow(unsafe_code)` to
  generated modules only; record an ADR note.
- `sdk/nostos_flutter` is a separate workspace — root `make ci` does NOT cover
  it; it has pre-existing `cargo fmt` drift (lines 109/290/339, predates this
  session). Add a fmt/clippy gate for the SDK dirs (make target or CI step).
