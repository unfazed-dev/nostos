# Atlet Wave 2 — React Native + Web shared TS adapter

**Status:** DRAFT — opened per Task 16's gate ("write pilot retro → freeze
spec/adapter.md v1 → THEN open the wave-2 plan"). Not yet operator-approved
for execution; no task in this plan has started.

## Why this wave, why now

Ratified decision #9 in `docs/plans/atlet-nostos-vs-powersync-app-suite.md`
fixes the rollout order: pilot (Flutter, done) → **RN+web (shared TS
adapter)** → kotlin+swift → node+capacitor+tauri → dotnet last. Each wave is
its own follow-on plan; the adapter spec freezes at pilot retro. That freeze
happened in Task 16 (`apps/atlet/spec/adapter.md` is now v1,
`apps/atlet/spec/conformance-flutter.md` records the pilot's sign-off and
retro). This document is that follow-on plan for wave 2.

## What wave 2 inherits from the pilot, unchanged

- `spec/adapter.md` v1 — the conceptual contract (init/signOut/addSession/
  watchSessions/watchProducts/connected/setConnected/marks) is frozen. Wave 2
  ports it to TypeScript; it does not redesign it. If RN or web needs an
  operation the Flutter contract doesn't have, that's a v2 spec change with
  its own retro entry, not a silent wave-2 addition.
- `spec/metrics.md` — Core-4 + storage definitions, clock policy
  (`server_committed_at`-anchored cross-machine intervals, monotonic
  client-only intervals) carry over as-is.
- Ratified decisions #1–#8, #10 from the master plan (benchmark-first, one
  canonical schema + per-SDK Supabase users, profile-as-topology, runtime
  engine toggle with full wipe, neutral analytics store, numbers are
  internal-eval only) — none of these are Flutter-specific; all apply
  unchanged to RN/web.
- `apps/atlet/supabase/` (schema, RLS, seed, per-SDK user provisioning) — RN
  and web get their own Supabase auth users (`react_native@atlet.internal`,
  `web@atlet.internal`) via the existing `create_sdk_users.sh`; no schema
  changes expected.

## What the pilot retro requires wave 2 to do differently

`conformance-flutter.md`'s retro found two process gaps that are not
spec-text defects but *task-assignment* defects — carried into this plan
explicitly rather than left to repeat:

1. **Every conformance-checklist item must be assigned, for every adapter,
   in some task's brief.** The pilot's T9 brief silently dropped item 2 for
   NostosAdapter; both T9 and T10 silently dropped item 3. Wave 2's adapter
   tasks (one per SDK×engine, same shape as pilot T9/T10) must each state
   explicitly which of the 5 checklist items they own, and a single running
   scorecard (a `conformance-rn-web.md`, mirroring `conformance-flutter.md`)
   must show all 5 items × both adapters × both SDKs (RN, web) — 20 cells —
   with no cell silently unassigned.
2. **The checklist's live-environment prerequisite is explicit, not
   assumed.** No wave-2 adapter task should be scoped to "run conformance
   items live" unless the task brief also confirms a provisioned Supabase
   project + running services stack + signed-in session are actually
   available to that task's execution environment. If they aren't (as was
   true for all 16 pilot tasks), the task's brief should say so up front and
   scope to static verification + honest disclosure, matching the pilot's
   own precedent, rather than asking for a live run the environment can't
   satisfy.

## Scope

- **In scope:** a shared TypeScript `SyncAdapter` port (mirroring
  `sync_adapter.dart`'s shape) consumed by both a React Native app and a web
  app, each wired to NostosAdapter/PowerSyncAdapter equivalents for their
  respective SDKs, running the same Core-4 + storage bench suite, on the
  local profile.
- **Out of scope (unchanged from the master plan):** cloud profile execution
  (documented stub only until local numbers are stable — open item #3 from
  Task 16's brief), any comparative/moat numbers, FSL legal review, waves
  3–5 (kotlin+swift; node+capacitor+tauri; dotnet).

## Open questions to resolve before task breakdown (do not guess)

1. **RN and web adapter parity:** does `nostos` ship a `nostos-web` /
   `nostos-react-native` package with the same shape as `nostos_flutter`, or
   does wave 2 need a wrapper task first (mirroring pilot Task 6's
   scaffold-before-adapters ordering)? Needs a source check against
   `sdk/` before task 1 is written.
2. **PowerSync web/RN SDK surface:** `packages/powersync` for web/RN differs
   in API shape from `powersync` (Flutter) — needs the same "verify exact
   signatures before compiling adapter code" discipline the master plan
   applied to Flutter (its own Global Constraints line), not an assumption
   that the Dart-side research carries over.
3. **One shared TS adapter file consumed by two app targets, or two
   thin platform packages sharing a core:** RN and web have different
   storage/runtime primitives (SQLite via RN bridge vs. wa-sqlite/OPFS on
   web for nostos; RN vs. IndexedDB backends for PowerSync). "Shared TS
   adapter" per decision #9 likely means a shared *interface + marks +
   conformance test* module (mirroring pilot Tasks 7/8), with platform-
   specific adapter implementations underneath — needs confirming before
   task breakdown, not assumed from the pilot's single-runtime shape.
4. **Web durability:** per `[[nostos-adr-audit-2026-07-30]]` (project
   memory), nostos's web path is live-only with no browser outbox — the
   IndexedDB mirror was rejected in the ADR-0017 addendum. If item 3's
   offline-queue-drain conformance check applies to the web SDK at all, it
   needs to be scoped against that known limitation, not assumed to work
   like the native adapters.

## Suggested task shape (skeleton only — not final until the open questions above are answered)

Mirroring the pilot's proven task decomposition (spec → schema/services →
scaffold → adapter interface + marks → per-adapter implementations → engine
toggle → UI → bench wiring → analytics → sign-off), scaled to two SDKs
sharing one TS adapter layer:

1. Confirm/scaffold `nostos`'s RN and web client packages (resolves open
   question 1).
2. Port `spec/adapter.md` v1 + `spec/metrics.md` to a shared
   `packages/atlet-adapter-ts` (or equivalent) — interface + `MarkDeriver`
   port + conformance test harness, engine-neutral.
3. RN NostosAdapter + RN PowerSyncAdapter (own conformance-item assignment,
   explicit per the retro's rule above).
4. Web NostosAdapter + web PowerSyncAdapter (own conformance-item assignment;
   explicit disclosure against the web-durability limitation in open
   question 4).
5. Engine toggle + full-wipe flow, ported per-platform.
6. UI parity (signin/home/detail/shop/analytics) — reuse the pilot's design
   tokens/assets where the web/RN design system allows; do not silently
   diverge from `apps/atlet/design/` without a provenance note (mirrors
   pilot Task 2's discipline).
7. Bench runner wiring (Core-4 + storage), reusing `spec/metrics.md`
   unchanged.
8. Analytics tab + upload, reusing the `analytics_runs` schema and
   internal-eval labeling rules unchanged.
9. Conformance sign-off — `conformance-rn-web.md`, all 20 cells accounted
   for (5 items × 2 adapters × 2 SDKs), honest disclosure for whatever
   remains operator-gated, same standard this document's Task 16 held
   itself to.

## Global constraints (carried forward, unchanged)

- `apps/atlet/` only (or its RN/web equivalents under the same tree) — no
  nostos crate touches unless a task explicitly confirms a wrapper package is
  missing and gets separate sign-off to add one.
- Single-line conventional commits, no author mentions, explicit paths only
  (never `git add -A`).
- No comparative/moat numbers anywhere; internal-eval labeling is mandatory,
  not optional, on every analytics surface.
- No live Supabase/docker/device results may be fabricated; disclose
  untested live paths as concerns, per the pilot's own precedent.
