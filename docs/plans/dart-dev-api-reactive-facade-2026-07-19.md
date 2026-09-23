# Dart/Flutter Dev-API — Reactive Facade (`Collection<T>` + `NostosStore`)

**Status:** Proposed (awaiting go). **Date:** 2026-07-19. **Owner:** tech lead, nostos flutter/dart.
**Governing ADR:** [ADR-0024](../adr/0024-client-reactive-facade-and-query-primitive.md).
**Glossary:** [`CONTEXT.md`](../../CONTEXT.md).
**Scope (operator-approved):** Dart/Flutter SDK only, plus a "principles that generalize" section.

---

> **Revision 1 (2026-07-19, post-implementation-read).** Reading the full
> `sdk/nostos_flutter/lib/src/nostos.dart` impl **falsified this plan's central
> premise** — that the as-built `watch()` is a cold stream that storms at nostos's
> throughput. It is not: `Nostos.watch` (`nostos.dart:117-182`) already returns a
> hot broadcast stream that replays the latest value, cached per-table in
> `_watchCache`, so N widgets share ONE upstream pump; `watchQuery` already has
> `triggerOnTables`/`throttle`. **Storms do not occur.** The reactive primitive
> was re-decided to **`Stream<List<T>>`-primary + optional `.asValueListenable()`
> adapter** (operator-approved). The body below retains the original
> ValueListenable-primary framing as an audit trail; the **binding decision is
> ADR-0024 rev 1** and the implemented surface is `Collection<T>.watch() → Stream`
> + `count()` + `upsert()/delete()` + honest `SyncStatus`. The facade's *value*
> (typed Collection + count + SyncStatus + typed writes) is unchanged — only the
> primitive-justification changed.

---

## TL;DR

Add a **reactive facade** — typed `Collection<T>` (conceptual `NostosStore` layer)
— over the ratified 2026-07-13 `NostosDatabase` SQL core. The facade is the beautiful
default surface (`db.collection<T>().watch().upsert()`); raw SQL stays as the escape
hatch. The reactive primitive is a **`Stream<List<T>>`** building on the as-built
hot-replay-shared pump (re-reading `nostos.dart` falsified the original cold-stream-storms
premise — see Revision 1 + ADR-0024 rev 1), with an optional `.asValueListenable()`
adapter. `SyncStatus` ships honest now; `DataTrust` is gated behind the P0 sync
fixes. Hand-written `fromRow` now; `@NostosRow` codegen is a P1 fast-follow.

This **extends** the ratified redesign; it does **not** supersede it. The ratified
plan named the API shape; this plan fills its under-specified gap — the reactive
layer above `watch()`.

---

## 1. What this plan is — and isn't

**Is:** a design for the dev-facing reactive abstraction layer over `NostosDatabase`,
grounded in (a) the as-built surface, (b) the 2026-07-13 ratified redesign, (c) a
four-way research fan-out, (d) a consultant pressure-test, (e) a `/grill-with-docs`
session with the operator.

**Isn't:** a re-litigation of the ratified 2026-07-13 API redesign (that's ratified), an
implementation (scope is plans-only unless the operator says go), or a multi-SDK
redesign (dotnet/RN get their own plans — the reactive layer is Dart-specific).

---

## 2. Steelman of the ratified plan (do not regress)

The 2026-07-13 plan is correct and this plan builds on it, not over it:

- **API shape** — a declared-schema, connector-based DX (`Schema`/`Connector`/`Database`/SQL) + nostos's
  collapsed-write moat. Right call.
- **Auto-schema** — `GET /schema` (ADR-0021) zeroes the boilerplate that is
  a comparable sync SDK's biggest tax. The headline DX edge.
- **Storage pivot** — WS2 JSON-column payload + SQLite `VIEWS` over `cairn_data`
  (`json_extract`), slice-1 shipped. Typed reads without materialized tables.
- **Conflict model** — per-field LWW (ADR-0014 tier a), implicit, no client surface.
  Matches the no-enum strategy (ADR-0004/0014; `docs/plans/sync-strategy-analysis-2026-07-19.md`).
- **Replace, not dual-maintain** — pre-1.0 with one demo consumer; clean break is right.

This plan **preserves every one of these** and adds the one thing they under-specified:
the reactive layer + the missing `watch()` knobs + a `SyncStatus` that is honest about
the unfixed P0s.

---

## 3. The gap (verified from the as-built code)

`NostosDatabase` (`sdk/nostos_flutter/lib/src/nostos_database.dart`, verified) exposes:

```
connect/open/supabase(...) → Future<NostosDatabase>     // auto-schema via GET /schema
subscribe(table, {where}) / subscribeTables([...])     // server predicate
watch(sql)                  → Stream<List<Map<...>>>   // re-emits on every invalidation
getAll(sql) / execute(sql)  → Future<List<Map<...>>>
write({table, op, pk, payload}) → Future<int>          // collapsed write
connectionState             → Stream<NostosConnectionState>
```

Pain points, all verified:

1. **No reactive ergonomics** — `watch()` is a thin passthrough; no distinct, no
   selector, no throttle, no typed records at the call site.
2. **Subscription lifecycle** — the demo does manual `.listen()` +
   `StreamBuilder`; `dashboard_shell.dart:184` literally comments on the
   "subscribe once and stay subscribed" workaround.
3. **No `SyncStatus`** — only a `connectionState` enum stream; no `syncing`/
   `reconciling`/`lastSyncedAt`/errors, and no honest surfacing of the unfixed P0s.
4. **No query knobs** — a parameterized, trigger-scoped, throttled `watch(sql, {parameters, triggerTables,
   throttle})` is table-stakes among comparable sync SDKs; nostos's `watch(sql)` has none.
5. **No derived selector** — a count widget rebuilds on every column change because
   there's no `count()`/`watchCount()`.

This is the design target.

---

## 4. Research evidence (four subagents + consultant, 2026-07-19)

| Source | Grade | Headline take (what we steal) |
|---|---|---|
| **A comparable Dart sync SDK** (leading in-market) | 🔥 | `watch(sql,{parameters,triggerTables,throttle})`, `watchCount`, `writeTransaction`, `statusStream`. Weaknesses to beat: `uploadData` toll-booth (we already deleted it), stringly-typed 3-type columns, raw `setState` status, 10ms Dart throttle footgun. |
| **Modern Dart/Flutter reactive (2026)** | 🔥 | Framework-native primitives = `Listenable`/`ValueNotifier`/`ValueListenable` + `Stream` (no native signals). A comparable SDK's `watch()` = fresh cold stream per call. Don't `asBroadcastStream()` per-query (storms). |
| **rxdart** 0.28.0 | 🔥 | `BehaviorSubject`/`ValueStream` for cached-state; `distinct`/`switchMap`/`scan`; pitfalls = broadcast loses backpressure, subscription leaks, `switchMap` cancellation errors post-0.28. Publish `ValueStream` interface, not concrete Subject. |
| **ng-elf** | 🔥 (dead 2026-06-05) | **Cautionary tale, not a template.** Fatal mistake: never bridged to Angular signals → eclipsed by `@ngrx/signals`. Lesson: **ship the platform's native reactive primitive or die.** Also steal: per-key request status, `skipWhileCached` as a transformer, entities-by-id. |
| **Consultant (GLM-5.2, HIGH)** | — | (a) codegen-now = negative ROI pre-1.0 → hand-write now/codegen P1. (b) cold-per-watch storms at our throughput → hot ref-counted `ValueListenable` per query; no row-diff until measured. (c) `dataTrust` now = permanent stale badge → gate behind P0s. (d) de-risk: measure demo fan-in before lock-in. |

**Key tension resolved:** rxdart brief leaned `BehaviorSubject` (broadcast, cached);
modern-state brief warned *against* broadcast per-query (storms). Resolution: **per-query
= hot ref-counted `ValueListenable`** (shares ONE re-execution, distinct, throttle);
**singleton state (`db.status`) = hot `ValueListenable`** (many widgets, one value). Cold
`Stream` available via `.asStream()`. This is neither rxdart's broadcast-Subject nor
a comparable SDK's cold-per-call; it is the throughput-correct middle.

---

## 5. The design — the facade API

```dart
// === Connect (ratified, unchanged) ===
final db = await NostosDatabase.supabase(wsUrl: 'wss://nostos.../sync');
//   auto-schema via GET /schema (ADR-0021). Or .connect(url:, sqlitePath:, schema:).

// === The facade: typed collection (the DEFAULT beautiful surface) ===
class Todo {
  final String id; final String title; final bool completed;
  factory Todo.fromRow(Map<String, Object?> r) => Todo(
    id: r['id'] as String, title: r['title'] as String,
    completed: (r['completed'] as int) == 1);
  Map<String, Object?> toRow() => {'id': id, 'title': title, 'completed': completed ? 1 : 0};
}

final todos = db.collection<Todo>(
  table: 'todos', fromRow: Todo.fromRow, toRow: (t) => t.toRow());

// Reactive reads — HOT ValueListenable (the primary primitive)
final ValueListenable<List<Todo>> active = todos.watch(
  where: 'completed = ?', parameters: [0],
  triggerTables: const ['todos'],          // only re-run on relevant writes
);
//   ^ one re-execution fans out to N listeners; ref-counted per (table, where, params).

// Cold Stream escape hatch (comparable-SDK / StreamBuilder muscle-memory)
final Stream<List<Todo>> activeStream = todos.watch(...).asStream();

// Derived selector — a count widget does NOT rebuild on unrelated columns
final ValueListenable<int> activeCount =
    todos.count(where: 'completed = ?', parameters: [0]);

// Collapsed writes (the moat — NO uploadData toll-booth)
await todos.upsert(Todo(id: '1', title: 'ship', completed: false));
await todos.delete('1');

// Atomic batch
await db.batch((tx) async {
  tx.upsert('todos', {'id': '1', ...});
  tx.upsert('todos', {'id': '2', ...});   // commits together
});

// Status — hot singleton ValueListenable
final ValueListenable<SyncStatus> status = db.status;
final SyncStatus now = db.currentStatus;

// === SQL escape hatch (ratified, enriched with knobs) ===
final ValueListenable<List<Map<String, Object?>>> raw =
    db.watch('SELECT * FROM todos WHERE list_id = ?', parameters: [lid],
             triggerTables: const ['todos']);
```

### Reactive mechanics (implementation sketch)

`Collection<T>.watch(...)` returns a `NostosQuery<T>` (implements `ValueListenable<List<T>>`).
Internally the store keeps a ref-counted cache keyed by `(table, where, parameters)`:

- **First listener** → run the query, subscribe to the table-invalidation broadcast from the Rust engine.
- **Invalidation** → coalesce within a **16ms frame-budget window** (not the 10ms throttle footgun in comparable SDKs), re-run, apply **distinct** (deep equality on the row list — replace the list each emit so `ValueNotifier` actually fires), set `.value`.
- **N listeners** → share the same `NostosQuery<T>`; one re-execution fans out.
- **Last listener detaches** → cancel the upstream subscription, drop the cache entry.
- `.asStream()` → thin adapter: emits the current value on subscribe, then deltas.

Singleton state (`db.status`) is a single hot `ValueNotifier<SyncStatus>` updated by the engine.

---

## 6. `SyncStatus` and the P0 honesty gate

```dart
class SyncStatus {
  final ConnState conn;               // connecting | connected | reconnecting | disconnected
  final bool syncing;                 // actively downloading / applying
  final bool reconciling;             // local optimistic writes being reconciled to server image
  final DateTime? lastSyncedAt;
  final Object? uploadError;
  final Object? downloadError;
  // DataTrust dataTrust — INTENTIONALLY ABSENT until the P0s land.
}

enum ConnState { connecting, connected, reconnecting, disconnected }
// enum DataTrust { fresh, stale, reconciling }  // gated — see below
```

**The P0 gate (operator + consultant approved).** Two unfixed soundness issues
(memories `nostos-no-client-backfill-resume-unsound`, `nostos-offline-delete-orphan-p0`)
mean the local image cannot always be rendered as ground truth:

- **No client WAL backfill** across offline gaps → a reconnect can drop offline-gap changes in multi-user.
- **Offline hard-deletes orphan** client-side (present-rows-only snapshot, no reconcile).

Surfacing `DataTrust { fresh, stale, reconciling }` **now** would put a permanent
`stale` badge on every app until those land — honest UX that reads as broken ships
no software. So: **ship `SyncStatus` without `DataTrust` now**; add `DataTrust` as a
**P0-fix-gated** field that lands *only* when the backfill + orphan-reconcile fixes
ship. The rest of honest status (`syncing`/`reconciling`/`lastSyncedAt`/errors) ships
immediately — that is real, in-flight truth, not a trust grade.

---

## 7. De-risk spike (do this before locking the primitive — P0)

Consultant recommendation (d): the load-bearing assumption is that the demo's real
query fan-in (widgets × trigger-tables) under realistic data does not storm. Verify it
before committing to the hot-`ValueListenable`-per-query model.

1. Instrument the demo's widget tree: how many `watch()` calls per screen, across how
   many distinct tables, under a realistic row count (1k–10k rows).
2. Drive a synthetic upstream burst (FakeReplicator at ~10k events/sec) and measure
   query re-execution count + frame time per invalidation.
3. **Decision rule:** if fan-in × invalidation-rate stays inside the 16ms frame budget
   with the throttle, the model is locked. If a screen storms, tune the throttle
   (coalesce window, `triggerTables` specificity) **before** considering incremental
   row-diff. Row-diff is architecture-of-last-resort, only on a measured bottleneck.

This spike is cheap (a day) and prevents the most expensive possible mistake
(building row-diff prematurely, or shipping a storming primitive).

---

## 8. Sequenced slices

**P0 (ship the facade, hand-written, honest status):**
- [ ] Fan-in de-risk spike (§7).
- [ ] `NostosStore` + `Collection<T>(table, fromRow, toRow)` — `watch()`/`count()`/`upsert()`/`delete()`.
- [ ] Hot ref-counted `NostosQuery<T> implements ValueListenable<List<T>>` (distinct + 16ms throttle + `.asStream()`).
- [ ] `watch()` knobs: `where` + `parameters` + `triggerTables` + `throttle` (override).
- [ ] `db.batch((tx) => ...)` atomicity over collapsed writes.
- [ ] `SyncStatus` (without `DataTrust`) on `db.status` / `db.currentStatus`.
- [ ] Migrate the demo (`example/lib/views/*`) off manual `listen()` + raw `StreamBuilder` onto `Collection<T>.watch()` + `ValueListenableBuilder`.
- [ ] `make ci` green; no throughput regression on `make bench`.

**P1 (codegen + bridges):**
- [ ] `@NostosRow('table')` + `nostos_generator` (build_runner) → typed `Collection<T>` with generated `fromRow`/`toRow` from the auto-schema. Deletes the one glue line per type.
- [ ] Optional `.toSignal()` bridge (`signals_flutter`) for the signals crowd — opt-in, not a dependency.
- [ ] Migration guide: comparable sync SDKs → nostos (`watch()`-returns-`ValueListenable`, no `uploadData`, auto-schema).

**P0-fix-gated (lands when the P0s land):**
- [ ] `DataTrust { fresh, stale, reconciling }` field on `SyncStatus`.
- [ ] Depends on: client WAL backfill across offline gaps; offline hard-delete orphan reconciliation (`nostos-no-client-backfill-resume-unsound`, `nostos-offline-delete-orphan-p0`).

**Out of scope (this plan):** dotnet/RN reactive surfaces (ADR-0020 RN gets its own plan);
wire-protocol changes; incremental row-diff (only if §7 forces it).

---

## 9. Principles that generalize to the nostos API

The reactive layer is Dart-specific, but the design *principles* apply to every nostos SDK
and to the wire/server surface:

1. **Ship the platform's native reactive primitive, first-class.** Dart: `ValueListenable`
   + `Stream`. Dotnet: `IObserver<T>`/`INotifyPropertyChanged`. RN: hooks/subscriptions.
   (ng-elf's fatal mistake was skipping this.)
2. **No top-level strategy enum.** Per-field conflict tier (ADR-0004/0014) is the seam;
   keep conflict resolution implicit in the client surface. Industry consensus.
3. **The common case is one line.** Auto-schema (ADR-0021) + collapsed writes (ADR-0013)
   → `db.collection<T>()` with zero boilerplate; no `uploadData`, no hand-written `Schema`.
4. **Honest status over polish.** Surface `syncing`/`reconciling`/errors; gate trust grades
   behind the fixes that make them true.
5. **Throughput shapes the primitive.** nostos's 142k ops/sec moat means the SDK shares work
   (hot, ref-counted) where lower-throughput rivals can afford per-call cold streams. The SDK
   is where the moat meets the dev — don't throw it away copying the competitor's ergonomics.
6. **Escape hatch always present.** Typed/beautiful default + raw-SQL/power-user escape hatch
   in every SDK.

These inform — not constrain — the other SDK plans.

---

## 10. Adversarial review (Gate 3)

| Attack | Resolution |
|---|---|
| Does the facade violate ADR-0004/0014 (no enum, per-field tier)? | No — conflict resolution stays implicit; `watch()`/`upsert()` don't expose strategy. |
| Cold-per-watch at 142k ops/sec → query storms? | Resolved by hot ref-counted `ValueListenable` (consultant). De-risk spike (§7) before lock-in. |
| Codegen on a churny pre-1.0 facade? | Switched to hand-write-now / codegen-P1 (consultant + operator). |
| Permanent `stale` badge poisons launch demo? | `DataTrust` gated behind P0 fixes (consultant + operator). |
| `ValueListenable` breaks muscle-memory from comparable SDKs? | `.asStream()` escape hatch + migration guide. Trade is deliberate (throughput). |
| `Collection<T>` naming collision? | Use `NostosCollection<T>` or `db.collection<T>()` accessor (no bare export collision); matches the existing `NostosTable`/`NostosColumn` aliasing pattern (export-barrel). |
| Is `batch()` YAGNI? | Justified for offline-first atomic multi-writes (invoice + line items); P0, thin wrapper over collapsed writes. |
| Ref-counted cache lifecycle bugs? | Real implementation cost; mitigated by keying on `(table, where, parameters)` + dispose-on-last-detach + a test that asserts no upstream leak after widget dispose. |

---

## 11. Claim list (Gate 4)

| Claim | Status | Evidence |
|---|---|---|
| As-built `NostosDatabase` surface is as described (§3). | verified | `sdk/nostos_flutter/lib/src/nostos_database.dart:61-291` (read this session). |
| Demo has manual-`listen()` + `StreamBuilder` pain. | verified | `example/lib/views/dashboard_shell.dart:69,184`; `appointments_view.dart:26,69`. |
| A comparable Dart sync SDK's `watch()` = cold fresh stream, table-level invalidation, has `triggerTables`/`throttle`/`watchCount`/`writeTransaction`, 10ms Dart throttle. | verified | that SDK's public API docs (subagent, 🔥). |
| ng-elf is dead (2026-06-05), eclipsed for not bridging to signals. | verified | ngneat-archive/elf + reddit r/angular (subagent, 🔥 + 🌡️). |
| Hot-`ValueListenable`-per-query is throughput-correct; cold-per-watch storms at nostos throughput. | assumed | Consultant (GLM-5.2, HIGH) reasoning; NOT yet measured against the demo — that is the §7 spike. |
| 16ms throttle avoids the 10ms footgun in comparable SDKs. | assumed | Inference from frame-budget + the competitive research brief's 10ms-self-DOS flag; validate in §7. |
| `dataTrust=stale` would show on every app pre-P0-fix. | assumed | Inference from the two P0 memories; not yet observed in a running demo. |
| Facade preserves the ratified plan's decisions. | verified | Cross-checked against the ratified 2026-07-13 connection-redesign plan (read this session). |

**Most load-bearing `assumed`:** the throughput claim (row 5). The §7 spike is what converts
it to `verified` — and is the gate before implementation.

---

## 12. Gaps / open questions (honest)

1. **Fan-in measurement not yet done** (§7) — the single biggest de-risk; do it first.
2. **`DataTrust` semantics post-P0** — what exactly makes `fresh` true (LSN caught up +
   reconcile complete + backfill window clear?) is deferred to the P0-fix design, not this plan.
3. **`where:` is still a SQL fragment** — injection-safe via `parameters:`, but a typed
   query-builder is a possible P2; not designed here (Ponytail — SQL+params is the safe default).
4. **Codegen `@NostosRow` annotation shape** — deferred to P1; the P0 hand-written surface
   must lock first so codegen has a stable target.
5. **Throttle default (16ms) is an assumption** — validate in §7; may need to be configurable
   per-query (the `throttle:` override exists for exactly this).

---

## Operator decisions ratified this session (2026-07-19, `/grill-with-docs`)

1. **Shape:** facade (`Collection<T>` + `NostosStore`) over ratified `NostosDatabase`. SQL = escape hatch.
2. **Types:** hand-written `fromRow` now; `@NostosRow` codegen = P1 (consultant-override of original "codegen-now").
3. **Reactive primitive:** hot ref-counted `ValueListenable<List<T>>` per query (consultant; not the cold-stream approach comparable SDKs use).
4. **`SyncStatus`:** honest now; `DataTrust` gated behind P0 fixes (consultant-override of original "honest/full incl dataTrust now").
5. **Scope:** Dart-only + principles section.
6. **De-risk:** measure demo fan-in before locking the primitive (consultant (d)).

**Explicit-go gate:** this plan is *proposed*. No implementation (per `nostos-scope-plans-only`)
until the operator says go. First action on go = the §7 spike, not the facade.
