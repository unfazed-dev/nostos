# Flutter pilot — adapter conformance sign-off

**Date:** 2026-08-06
**Spec version tested against:** `spec/adapter.md` v0 → frozen to v1 by this sign-off (see Freeze, below)
**App:** `apps/atlet/flutter`
**Versions:** Flutter 3.44.0 (stable) · Dart 3.12.0 · `nostos_flutter` — path dependency on `../../../sdk/nostos_flutter` (pubspec version `0.1.0`, built from nostos repo HEAD `93b980d` — no fixed pub.dev release, it tracks the workspace) · `powersync` 1.18.0 · `powersync_core` 1.8.0 · `supabase_flutter` 2.17.1

## Environment gate (checked live this session, not assumed from prior reports)

- `docker ps`: only `nostos-postgres` running (cairn's own e2e Postgres, port 5433 — unrelated to Atlet). No `atlet-services` containers (`nostos-server` on 8080, `powersync` on 8081) are up.
- `apps/atlet/services/.env`: absent. Only `.env.example` ships, by design — no real Supabase credentials are checked into this tree.
- `docker compose -f apps/atlet/services/docker-compose.atlet.yml config -q`: exits 0 syntactically, but every required variable (`SUPABASE_URL`, `SUPABASE_JWT_SECRET`, `POWERSYNC_PG_URL`, `NOSTOS_WRITE_TABLES`) resolves blank.
- No Android/iOS simulator or physical device was booted this session.

This is the same wall every implementation task in this suite hit — T6, T9, T10, T12, T14, T15 each independently disclosed no live Supabase project, no atlet-services stack, no device. Standing one up (real Supabase project, filled `.env`, `docker compose up`, a signed-in test session) is an operator-gated integration step that has never been performed in any Claude session for this pilot. Task 16 does not change that, and this report does not fabricate results that step would produce.

## Conformance checklist results

Per-item status for both adapters, against `spec/adapter.md` §Conformance checklist:

| # | Item | NostosAdapter | PowerSyncAdapter |
|---|---|---|---|
| 1 | `init→signIn→addSession→serverAcked` mark fires <60s | NOT RUN (live). Scoped to T9's brief, but no live backend was ever available. Statically consistent: `addSession` populates `MarkDeriver.localIds` before the underlying write resolves (independently confirmed in T9's review). | NOT RUN (live). Scoped to T10's brief, same environment gap. Statically consistent: `_deriver.localIds.add` precedes the `execute()` INSERT (independently confirmed in T10's review). |
| 2 | Row inserted via PostgREST → `remoteVisible` <60s | **Never scoped to any task.** T9's brief pinned NostosAdapter's live-run requirement to items 1, 4, 5 only (`task-9-brief.md:33`) — item 2 was never assigned to this adapter, by anyone, at any point. NOT RUN. | NOT RUN (live). Scoped to T10's brief (`task-10-brief.md:39`), but no live backend was ever available. |
| 3 | `setConnected(false)` → 25 writes → `setConnected(true)` → all 25 `serverAcked` | **Never scoped to any task, either adapter.** T9's brief: items 1,4,5. T10's brief: items 1,2,4,5. Item 3 is absent from both. NOT RUN, and — unlike items 1/2/4 — no task in this pilot was ever asked to run it. | Same — never scoped to any task. |
| 4 | `signOut` wipes local DB files; re-init cold-syncs from zero | NOT RUN (live, filesystem-observed). Statically verified: `NostosAdapter.signOut()` → `NostosDatabase.signOut()` performs a full wipe per ADR-0029 — confirmed by direct code read, not by watching files disappear on disk. | NOT RUN (live, filesystem-observed). Statically verified: `PowerSyncAdapter.signOut()` explicitly deletes the `.db`/`-wal`/`-shm`/`-journal` sidecar files (each guarded by `existsSync()`), because `disconnectAndClear()` alone leaves the sqlite file on disk per the SDK's own doc comment. |
| 5 | No adapter API leaks engine types into the app/bench layer | **PASS — statically verified.** `sync_adapter.dart` (frozen, Task 7) contains no `nostos_flutter` symbol; independently confirmed by two separate reviewers across T9's and T10's verdicts. | **PASS — statically verified.** Same file, no `powersync`/`powersync_core` symbol; same independent-reviewer confirmation. |

**Net result: item 5 is the only checklist item with a genuine PASS, for either adapter, at any point in this pilot.** Items 1–4 remain unexecuted against a live backend for both adapters. This sign-off does not upgrade that status — it confirms and formalizes it as the pilot's actual exit condition, because upgrading it here would mean fabricating results this session has no way to produce honestly.

## What *is* covered (engine-neutral, unit-level — not a substitute for the above)

- `test/adapter_conformance_test.dart` exercises the *mark-derivation ordering* contract — `localVisible` before `serverAcked` per row, `remoteVisible` for harness-injected ids absent from `localIds`, and reset-on-`signOut` — against `FakeAdapter`, not either real adapter. This validates `MarkDeriver`, the piece both adapters call into unmodified, but does not exercise a real engine and is not a substitute for checklist items 1–4.
- `flutter analyze` (this session, 2026-08-06): **0 issues.**
- `flutter test` (this session, 2026-08-06): **93/93 passed**, single clean run — the load-correlated flake noted in the task brief (`'detail: complete removes the session and pops'`) did not reproduce this run.
- `make ci` (this session, 2026-08-06): **549 passed, 0 failed.** Confirms the nostos workspace is unaffected by anything in the Atlet tree. Also true by construction, independently: `apps/atlet` does not appear in `Cargo.toml`'s workspace `members`, and `grep -rl atlet .github/workflows/` returns nothing.

## Recommended live run (operator-gated — unchanged in substance from T9's/T10's asks, corrected for the gaps found in this sign-off)

1. Provision a real Supabase project; fill `apps/atlet/services/.env` from `.env.example`.
2. `docker compose -f apps/atlet/services/docker-compose.atlet.yml up -d` (`nostos-server` :8080, `powersync` :8081), with `NOSTOS_WRITE_TABLES=sessions`.
3. Run `create_sdk_users.sh`; sign in as `flutter@atlet.internal`.
4. Drive **all five** items against **both** adapters. In particular: item 2 has no prior coverage for NostosAdapter, and item 3 has no prior coverage for either adapter — do not treat those as "already done elsewhere."
5. Record results in a dated addendum appended to this file; do not overwrite the "not run" entries recorded above — they are accurate for 2026-08-06 and should stay legible as history.

## Pilot retro — what `spec/adapter.md` v0 got wrong

1. **`syncStatus()` was specified; `connected` was built.** The spec text reads `syncStatus() -> stream (connected / syncing / offline, engine's own notion)` — a three-state, enum-shaped stream. What Task 7 actually froze into `SyncAdapter` is `Stream<bool> get connected` — a boolean. Every adapter, the engine toggle (Task 11), and the bench harness work against the boolean; nothing in the shipped pilot implements or needs a three-state status. The spec prose was never corrected to match the frozen interface. Fixed in v1 (see Freeze).
2. **The checklist's per-item task assignment silently dropped coverage.** The spec says the 5 conformance items should be "run per implementation," but the plan split adapter work across two single-adapter tasks that each hand-picked a subset — T9: items 1, 4, 5; T10: items 1, 2, 4, 5. Item 3 (offline queue-drain) was never assigned to either. Item 2 (remote-insert visibility) was never assigned to NostosAdapter. Nothing in the spec says who owns full coverage when work is split across tasks, so this wasn't a violation of anything written down — it fell through a seam between briefs, silently, and stayed silent until this sign-off went looking. Fixed in v1: the checklist now states explicitly that "run per implementation" means every item, every adapter, tracked against one running scorecard (this file), not whatever subset the assigning brief happened to ask for.
3. **The checklist assumes a live backend will exist by sign-off. It never did, at any point in this pilot.** Six separate implementation tasks (T6, T9, T10, T12, T14, T15) and now T16 independently hit the identical wall: no `.env`, no docker services, no device. That is not six unlucky sessions — it is the standing operating condition of this environment for the pilot's entire duration. A checklist whose sign-off gate structurally cannot be satisfied without an operator step it never names isn't frozen, it's aspirational. Fixed in v1: the checklist now carries an explicit prerequisites line naming the operator-provisioned live environment as out of scope for any implementation task.
4. **Marks are engine-neutral by construction, which the spec undersold.** `MarkDeriver` is shared, untouched code — the spec's phrasing ("marks... derived") reads as if each adapter derives its own marks, when in the shipped design the shared ordering logic that items 1–3 depend on lives in one file both adapters call into; adapter-specific responsibility is limited to correct read-path sequencing (populate `localIds` before the write resolves). This is a real strength — it's *why* fairness between engines is achievable at all — but the spec didn't say so, and a future reader could reasonably conclude each adapter needs its own separately-audited mark-derivation logic. Fixed in v1: `spec/adapter.md` now states `MarkDeriver` is the single shared, audited implementation of the marks contract.

## Freeze

`spec/adapter.md` is bumped to **v1** as of this sign-off (2026-08-06). Retro items 1 and 4 are incorporated as direct text corrections in `spec/adapter.md`. Retro items 2 and 3 are process/task-scheduling findings, not spec-text defects — corrected in the spec's own prerequisites/scope wording (item 3) and carried forward as an explicit task-assignment rule for the wave-2 plan (item 2), rather than requiring further spec amendment. No further amendments to `spec/adapter.md` without a new version number and a new dated retro entry in this file.
