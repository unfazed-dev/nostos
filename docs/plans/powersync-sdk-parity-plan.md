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

### SDK platform coverage (the dimension the original plan never scored)

PowerSync ships **10** client SDKs; Nostos ships **2** (3 counting the native
Rust `nostos-client`):

| Platform | PowerSync | Nostos today |
|---|---|---|
| Flutter (Dart) | ✅ GA `powersync` 2.3.1 | ✅ `sdk/nostos_flutter` (frb) |
| Web / WASM | ✅ GA `@powersync/web` 1.39.0 | ✅ `crates/nostos-ffi-wasm` (OPFS deferred — ADR-0015) |
| React Native | ✅ GA `@powersync/react-native` 1.35.9 | ❌ ROADMAP Phase 3 (ADR-0015 UniFFI) |
| Node | 🟡 Beta `@powersync/node` 0.19.4 | 🟡 `sdk/nostos_node` (napi-rs) — **loads in node, async query round-trips `EXIT=0` (verified independently)**; offline-only (no live `subscribe`/replicator path yet) |
| Kotlin (KMP) | ✅ GA `com.powersync:core` 1.12.0 | ❌ |
| Swift (iOS/macOS) | ✅ GA `powersync-swift` | ❌ |
| Capacitor | 🟡 Beta | ❌ |
| Tauri | 🔴 Alpha | ❌ |
| .NET | 🟡 Beta | ❌ |
| Rust | 🔴 Alpha | ✅ `crates/nostos-client` (native API, not packaged as an SDK) |

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
   **3/10 platforms** (Flutter + WASM + Node).
4. P5 Sync Streams — biggest remaining *feature* gap for the "equivalent SDK"
   claim.
5. Package `nostos-client` as the Rust SDK (platform #10, trivial — re-export +
   docs) for a fast 4/10.

### Path to 10/10 (honest roadmap)

Each new SDK is a thin FFI bridge over `nostos-core` / `nostos-client` — Node
proved the pattern ports in a session. Order = value-per-effort, with
this-session verifiability noted:

| Platform | FFI strategy | Verifiable here? | Effort |
|---|---|---|---|
| ✅ Flutter | frb | yes (`flutter test`) | shipped |
| ✅ WASM/Web | wasm-bindgen | yes (`smoke.mjs`) | shipped |
| ✅ Node | napi-rs | **yes (`node smoke` EXIT=0)** | scaffold shipped |
| Rust SDK | re-export `nostos-client` + docs | yes (`cargo test`) | **trivial — fast 4/10** |
| Tauri | tauri-plugin over `nostos-client` (Rust-native — most natural next) | yes (`cargo build`) | small-medium |
| Web JS SDK | package `nostos-ffi-wasm` as `@nostos-sync/web` w/ PS-style API | yes (vitest) | small-medium |
| React Native | reuse the JS core (RN shares it, like `@powersync/web`↔RN) | partial (Jest; full E2E needs device) | medium |
| Kotlin/KMP | UniFFI; android cross-targets present, JDK present | compile yes; E2E needs Android SDK/gradle | medium-large |
| Swift/iOS | cbindgen + Swift PM (`swift-bridge`); Xcode + iOS targets present | compile yes; E2E needs simulator | medium-large |
| .NET | uniffi-cs / cbindgen + DllImport | needs .NET SDK (not probed) | medium-large |
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
