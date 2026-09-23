---
adr_decision:
  hard_to_reverse: true
  reversal_cost: "Public SDK API surface: the facade shape (Collection<T>, count(), SyncStatus, Stream-primary + adapter) is what every consumer codes against; changing it after launch breaks call sites and the eventual codegen contract."
  surprising_without_context: true
  surprise_reason: "A future reader asks two things: (1) 'why a Collection<T> facade when NostosDatabase.watch(sql) already works?' and (2) 'why Stream-primary when the research leaned ValueListenable?' The answers — typed ergonomics + honest SyncStatus, and the as-built Stream is already hot-replay-shared so ValueListenable would wrap working machinery for a marginal win — are non-obvious without reading nostos.dart."
  result_of_real_tradeoff: true
  rejected_alternatives: "ValueListenable-primary (wraps the existing hot-replay Stream in a per-query ValueNotifier; storm-justification falsified — nostos.dart:117-182 already shares via _watchCache + _replayLatest; marginal widget-ergonomics win the demo doesn't need); both-first-class watch()+values (more surface to maintain); supersede NostosDatabase (throws away 2026-07-13 ratified alignment)."
  all_three_true: true
status: accepted (shipped 2026-07-19; status corrected 2026-07-30 — it read "proposed" while Collection<T>/SyncStatus were exported and used by the demo)
date: 2026-07-19
revision: 1 (2026-07-19, same day — primitive re-decided after reading full nostos.dart impl; see Revision note)
---

# ADR-0024: Client reactive facade (`Collection<T>` + `NostosStore`) over the existing hot-replay stream

## Revision note (2026-07-19)

The original draft (rev 0) chose a **hot ref-counted `ValueListenable<List<T>>` per
query**, justified by "a cold-fresh-stream-per-`watch()` storm risk at nostos's
142k ops/sec." **That premise was falsified the same day by reading the full
`nostos.dart` implementation** (not just the signatures): `Nostos.watch(table)` already
returns a hot broadcast stream that replays the latest value to each new listener
(`_replayLatest`, `nostos.dart:144-182`), cached per-table in `_watchCache` so N widgets
share ONE upstream pump; `Nostos.watchQuery(sql, {triggerOnTables, throttle})` already
has knobs offering that parity. **Storms do not occur.** The primitive was re-decided
to **Stream-primary + optional `.asValueListenable()` adapter** (operator-approved).
The facade's value is typed ergonomics + `count()` + `SyncStatus`, not a primitive swap.

## Context

The 2026-07-13 reactive-query redesign locked `NostosDatabase` as the SQL-core sync
handle. Reading the full impl (2026-07-19) showed the reactive layer is **already
substantial**: per-table hot-replay-shared broadcast streams, parity-class
`triggerTables`/`throttle` knobs, and typed `watchMapped<T>`. What is genuinely
missing — the real gap — is: (a) a typed `Collection<T>` facade so devs don't hand-write
`SELECT * FROM <table>` + `fromRow` at every call site; (b) a `count()` derived selector;
(c) a `SyncStatus` value object (only a `NostosConnectionState` enum exists); (d) typed
writes (`upsert(T)` / `delete(id)`); (e) an optional `ValueListenable` bridge for
`ValueListenableBuilder` users.

Research (2026-07-19): ng-elf is **dead** (2026-06-05), eclipsed because it never bridged
to Angular's native reactive primitive — the lesson is "offer the platform's native
widget primitive," which the `.asValueListenable()` adapter satisfies without forcing it.
A cold-fresh-`Stream`-per-call `watch()` is common elsewhere; nostos's is already hotter
(replay-shared). rxdart 0.28's `BehaviorSubject`/`ValueStream` is the cached-state
pattern nostos already hand-rolls in `_replayLatest` (no dep).

## Decision

1. **`NostosStore` + `Collection<T>` facade** over the ratified `NostosDatabase` SQL
   core. `db.collection<T>(table, fromRow, toRow)` → `watch()`/`count()`/`upsert()`/
   `delete()`. Raw SQL via `NostosDatabase.watch()` stays as the escape hatch.
2. **Primary reactive primitive = `Stream<List<T>>`**, building on the existing
   `Nostos.watchMapped` + `watchQuery({triggerOnTables, throttle})`. An optional
   **`.asValueListenable()`** adapter exposes a `ValueListenable<List<T>>` for
   `ValueListenableBuilder` users (the ng-elf lesson, opt-in).
3. **Hand-written `fromRow`/`toRow` now**; `@NostosRow` codegen is a **P1 fast-follow**
   once the facade shape locks (negative ROI to build codegen on a churny pre-1.0
   surface with one demo consumer — consultant).
4. **`SyncStatus` is honest now** (`conn`/`syncing`/`reconciling`/`lastSyncedAt`/
   `uploadError`/`downloadError`) as a hot `ValueListenable<SyncStatus>` on `db.status`;
   **`DataTrust` is gated behind the P0 sync fixes** (client WAL backfill across offline
   gaps; offline hard-delete orphan reconciliation). A permanent `stale` badge on every
   app would poison the launch demo; `DataTrust` ships only when it can be true.

## Rationale

The facade gives the typed, dev-excellent surface the operator asked for **without
discarding or rewriting the ratified SQL core or its (already-good) reactive plumbing.**
Stream-primary builds on what works and keeps `watch()` → `Stream` muscle-memory; the
`.asValueListenable()` adapter honors ng-elf's "offer the native widget primitive"
lesson without breaking muscle-memory or wrapping working machinery for a marginal
widget-ergonomics win the demo (which hoists streams to `late final` fields) doesn't
need. Gating `DataTrust` keeps the API honest without shipping a badge that reads as
broken.

## Consequences

- **+** Facade is small (pure Dart over `Nostos.watchMapped`); no Rust changes, no new
  reactive machinery, no ref-counted-cache lifecycle to get wrong.
- **+** `watch()` → `Stream` muscle-memory preserved; `ValueListenableBuilder`
  users get the adapter.
- **−** Does NOT dedupe identical `(table, where)` queries across callers (each
  `Collection<T>.watch()` gets its own `watchQuery` pipeline). Acceptable at the demo's
  fan-in (12 sites / 6 tables); a shared per-query cache is a fast-follow IF a measured
  bottleneck appears — not before (consultant: no speculative architecture).
- **−** `triggerTables` defaults to the subscribed set (existing engine behavior); a
  future engine that tags invalidations per-table could sharpen this — P1.
- **P0 de-risk spike (DONE 2026-07-19):** demo fan-in measured at 12 `watch()` sites /
  6 tables / 7 views; worst screen (`chat_view`) watches 3 tables. Trivially inside any
  frame budget; confirms no storm, no row-diff needed.

## References

- Plan (this decision's implementation): `docs/plans/dart-dev-api-reactive-facade-2026-07-19.md`
- As-built reactive plumbing: `sdk/nostos_flutter/lib/src/nostos.dart:117-182` (`watch`/`_replayLatest`/`_watchCache`), `:216-267` (`watchQuery`/`watchMapped` with `triggerOnTables`/`throttle`)
- ADR-0014 (per-field LWW — the conflict surface the facade leaves implicit)
- ADR-0021 (client schema discovery REST — the auto-schema the facade + codegen consume)
- ADR-0013 (direct write-back — the collapsed-write model `Collection.upsert()` relies on)
