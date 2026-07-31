# Multi-SDK pomodoro fixture matrix — plan

**Status:** plan only. No implementation, no deletions performed. Three decisions below need an
operator call before the sweep or the build starts.

**Date:** 2026-07-30

---

## 0. Repo state — DECIDED: `fixtures/` is greenfield

`fixtures/` is empty on disk but has **228 tracked files in `HEAD`**, all showing as ` D` (deleted in
worktree, unstaged):

| app | deleted files |
|---|---|
| `fixtures/flutter/pomodoro` | 110 |
| `fixtures/flutter/todo` | 118 |

**Operator decision 2026-07-30: leave both deleted. Treat `fixtures/` as greenfield and rebuild to
the new spec.** I recommended restoring (both apps are directly reusable) and was overruled; that is
the operator's call and this plan proceeds on it.

Two consequences that are *not* optional follow-ups:

1. **8 orphaned `make` targets** now reference paths that do not exist: `fixture-test`,
   `fixture-e2e`, `fixture-todo-test`, `fixture-todo-smoke`, `fixture-todo-smoke-live`,
   `fixture-todo-nostos-live-{up,down,proof}`. They must be replaced by the new matrix targets, not
   left dangling.
2. **`docs/testing/persona-e2e-baseline.md` names `fixtures/flutter/pomodoro/` as its reference
   implementation** (§2). That pointer breaks and must be repointed at the new Flutter fixture.

**Greenfield does not mean the knowledge is lost.** Both apps remain in `HEAD`, so every pattern in
§2 is still readable via `git show HEAD:fixtures/flutter/todo/<path>` and can be lifted into the new
build without restoring the worktree. Treat §2 as a source library, not as living code.

**Because the deletions stay uncommitted, every commit in this workstream uses pathspec-scoped
`git add <path>`. No `git add -A`, no `git add .`, no `git commit -a`** — otherwise the deletions get
swept into an unrelated commit.

**Correction against myself:** I first said a CI workflow on `fixtures/**` was also broken. It is
not — `.github/workflows/` holds only `ci.yml` and `release.yml` and neither mentions fixtures.
The `fixtures/**` text is a Makefile *comment* proposing such a workflow ("if CI coverage is wanted
later"), which I misread as an existing one.

**Until this is resolved, every commit in this workstream uses pathspec-scoped `git add <path>`.
No `git add -A`, no `git add .`, no `git commit -a`.**

---

## 1. What was asked

1. Delete every example / example-app directory from all SDKs — no example apps inside an SDK.
2. Plan a fixtures smoke test for every SDK, in `fixtures/`.
3. All apps identical; all on Supabase; each SDK gets its own table(s) named after the SDK, its own
   users.
4. App = multi-user pomodoro. Shells: **dashboard** (streaks, recent + latest sessions),
   **sessions** (pomodoro CRUD + player), **community** (many users on one shared session player).
5. Community exercises nostos CRDT.
6. Supabase email+password auth, plus sign-out.
7. Community shows any user live the moment they open it.
8. Sessions saved to PDF only — exercising nostos storage.

## 2. What already exists — do not rebuild it

The request reads as greenfield. It is not. `fixtures/` is an established, Makefile-wired concept
and most of the hard problems are already solved.

| Asset | Where | Why it matters here |
|---|---|---|
| **Pomodoro app** | `fixtures/flutter/pomodoro` | a working pomodoro already exists — 5 Dart files, single view, local only: no auth, no sync, no dashboard/sessions/community |
| **Supabase-backed fixture** | `fixtures/flutter/todo` | **this is the template.** Not the pomodoro one |
| **Dual-mode testing** | `fixture-todo-smoke` (MOCK, no creds) vs `-smoke-live` (`env.json`) | exactly what a 10-SDK matrix needs: CI runs mock, operator runs live |
| **Ports & adapters in the fixture** | `lib/domain/{auth_gateway,todo_repository}.dart` + 4 impls in `lib/infra/` | `in_memory_`, `supabase_` (direct baseline), `nostos_` — nostos is tested *against a control*, which is the whole trick |
| **Local-live harness** | `tool/nostos_live_up.sh`, `nostos_live_down.sh`, `mint_jwt.sh`, `nostos_env.sh` | real nostos-server + docker Postgres + dev JWTs, standing in for Supabase |
| **Two-user acceptance test** | `fixture-todo-nostos-live-proof` → `integration_test/nostos_live_test.dart` | already asserts two-user offline sync + read/write tenant isolation |
| **Persona discipline** | `docs/testing/persona-e2e-baseline.md`, `docs/personas/`, 3 journeys + `persona_mapping_test.dart` guard | a ratified convention that *anticipates this task* |
| **Schema shape** | `fixtures/flutter/todo/supabase/schema.sql` | `user_id uuid references auth.users(id)`, RLS on, `auth.uid() = user_id` |

`persona-e2e-baseline.md` explicitly scopes itself to "any Flutter fixture **or SDK example app**"
and closes with: "persona journeys then double as SDK E2E: same personas, plus sync assertions
(offline write → reconnect → row echoed)." **This plan extends that document. It does not invent a
parallel system.**

Four of its rules are binding constraints the request does not mention:

- **Compressed time is a product config, not a test hack.** A `demo()` config in seconds reachable
  by real users (`--dart-define=DEMO_MODE=true`), plus a unit test proving the transition graph is
  identical across configs. A pomodoro fixture's entire domain is time; without this a 25-minute
  session test takes 25 minutes.
- **Assert transitions, never wall-clock.** Any test asserting elapsed duration is a review-blocker.
- **Keys, not text** — `Key('area.thing')`. **This is the mechanism for "all the apps must be
  exactly the same."** See §8.
- **The ladder, cheapest first:** unit → widget → smoke → persona journeys → Patrol. "Smoke test" as
  requested is rung 3.

## 3. Decisions — ALL TAKEN 2026-07-30

| # | Question | Operator decision | Consequence |
|---|---|---|---|
| D0 | the 228 deleted fixture files | **leave both deleted, `fixtures/` is greenfield** | I recommended restoring and was overruled. 8 orphaned `make` targets; `persona-e2e-baseline.md` pointer breaks |
| D3 | capability gaps across 9 SDKs | **build web durability first** (ADR-0017 Worker + SQLite-WASM) | workstream 1. Activates the destination ADR-0017 already committed to; does *not* overturn the IndexedDB rejection |
| D4 | reactivity outside Flutter | **generalize ADR-0024 to all 9** | workstream 2. I recommended poll-plus-declared-skips and was overruled |
| D1 | community / CRDT | **implement CRDT as a third workstream** | workstream 3. I argued the requirement is misattributed (a timer is a register; LWW is *correct*) and was overruled |
| D2 | PDF / storage | **PDF locally + bytes as a row payload** | no new subsystem; probes an unmeasured base64-over-JSON ceiling |
| D5 | SDK example dirs | **archive, reference only — "take it all"** | **DONE, see §4** |
| D6 | sign-out + local wipe | **RATIFIED as workstream 4** | new core primitive + 8 binding exposures + server `exp` work. See §9 WS4 |
| D7 | CRDT benchmark gate | **binding: ships with before/after numbers** | a wire-format change with no measurement is not mergeable, per `CLAUDE.md` |

**The shape this produces:** three engine workstreams precede any fixture work, and the pomodoro
matrix becomes the **acceptance suite for all three** rather than a set of workarounds around their
absence. That is a coherent strategy, but it is a materially larger program than "plan a fixtures
smoke test" — worth stating plainly.

**One risk that follows from D1 and is not optional.** CRDT needs per-field causal metadata
(version vectors / HLCs), and the wire protocol carries none today — ordering is purely
LSN/server-arrival. So D1 forces a **wire-format change**, which puts the **833,307 ops/sec @ 1k
clients, 0.00% drops** moat figure at risk. Per `CLAUDE.md`'s measure-before-optimize rule that work
ships with before/after benchmark numbers, and a regression there hits nostos's central competitive
claim, not merely a test.

The subsections below are the analysis that produced these decisions; they are kept because the
reasoning is what makes the decisions auditable, not because anything is still open.

### Analysis behind the decisions

### D1. Community/CRDT — the requirement is misattributed

A shared community timer is `is_running` / `started_at` / `duration_secs`. That is a **register**,
and last-write-wins is the *correct* primitive for a register: "someone pressed start at T, latest
press wins" is right, not lossy. So the community shell **as specified is buildable today** — it is
a good multi-writer **LWW** test, not a CRDT test. CRDT is genuinely absent from the engine
(ADR-0004/0014, Phase-4 opt-in).

Genuine CRDT pressure needs state where concurrent edits must *both* survive — a counter
(community total pomodoros: two concurrent increments must yield +2, not +1) or an add/remove set
(who is present).

- **(a) Recommended.** Build the community shell as specified, labelled honestly as an LWW
  multi-writer test. Add the community *counter* as a written-but-skipped test that becomes the
  CRDT acceptance test when the tier lands. Ships now.
- (b) Add the counter as a required assertion → blocks on implementing CRDT (multi-week, new ADR).
- (c) Implement CRDT first.

### D2. PDF/storage — three readings, only one touches nostos

Nostos syncs Postgres *rows*. It has no blob/file/object-storage subsystem *(pending slice-3
confirmation — treat as assumed, not verified)*.

| Reading | What it actually tests |
|---|---|
| (i) Local PDF via a Dart `pdf` package | the Dart package |
| (ii) Upload to Supabase Storage | Supabase |
| (iii) **PDF bytes as a row payload** | **nostos** — and stresses something unmeasured: the wire is JSON, so bytes go base64 (~1.37× inflation) through fan-out and write-back |

- **Recommended:** (i) as the user-facing app feature, (iii) as the nostos smoke test — it is the
  only one that exercises the engine, and it probes a real unmeasured ceiling.
- **Which was meant?** If "nostos storage" meant (ii), the honest answer is that nostos is not in that
  path at all and the test would prove nothing about it.

### D3. "All apps exactly the same" is not achievable literally

The SDKs are not capability-equivalent *(matrix in §10 pending slices 5–6)*:

- **Web + Capacitor are live-only KV**: no outbox, no offline write capture, no optimistic local
  row, a write **throws** when the socket is closed, rows vanish on reload, and reads are byte
  payloads not SQL (ADR-0017 addendum). An offline-write journey cannot pass there.
- **Only Flutter has a reactive facade** (`Collection<T>`/`watch`, ADR-0024). The other eight poll —
  which directly hits requirement 7 (community live on open).
- **React Native is Android-only** (no iOS TurboModule).

- **(a) Recommended.** Identical *assertions* and identical key namespace; per-SDK implementations;
  every unsupported assertion an **explicit declared skip** with a reason string — never a silent
  pass. A skip that reports PASS is the false-green failure this repo has been bitten by before.
- (b) Build only on the capable SDKs.
- (c) Build the ADR-0017 Worker/SQLite-WASM slice first so web can participate (multi-day).

## 4. The sweep — DONE 2026-07-30

**The request's premise did not hold: there is exactly ONE SDK example app.** Verified across all
git-tracked files, not assumed:

| path | files | own manifest | an SDK example app? |
|---|---|---|---|
| `sdk/nostos_flutter/example/` | 109 | `pubspec.yaml` | **yes — the only one** |
| `crates/nostos-client/examples/` | 1 | none | no — `reactive_scroll.rs`, a documented `CLAUDE.md` verb |
| `crates/nostos-infra/examples/` | 2 | none | no — **`e2e_server.rs` is the shared spine 9 of 10 slices spawn** |
| `web/src/routes/demo/` | 1 | none | no — a marketing-site route |

8 of 9 SDKs had no example dir at all. So "sweep all the SDKs" was a one-directory move.

**`crates/nostos-infra/examples/e2e_server.rs` is the near-miss worth remembering.** It sits in an
`examples/` dir but is test infrastructure: 9 of the 10 sdk-e2e slices spawn it. A literal
"delete every examples/ dir" would have broken nine slices, not one. Test infrastructure hiding in
an `examples/` directory is the trap here.

### What was moved

Per operator instruction ("archive the tests as well - take it all"), to `archive/` — reference
only, nothing built or run from it:

- `sdk/nostos_flutter/example/` → `archive/sdk/nostos_flutter/example/` (109 files)
- `sdk/nostos_flutter/test/` → `archive/sdk/nostos_flutter/test/` (4 Dart test files)
- `sdk/nostos_flutter/test_driver/` → `archive/sdk/nostos_flutter/test_driver/` (1)

All via `git mv`, so `git log --follow` still works. `archive/README.md` records the contents, the
cost, and what was deliberately *not* archived.

### Fallout, fixed rather than left broken

| Change | File | Why |
|---|---|---|
| `flutter` removed from `ALL_SLICES` | `scripts/sdk-e2e.sh:29` | its host app is gone; **9 live slices, not 10** |
| explicit `sdk-e2e.sh flutter` now **fails loudly** | `scripts/sdk-e2e.sh` | a no-op branch reporting success is the false-green this harness guards against everywhere else. Deliberately *not* `skip_slice` — `SDK_E2E_STRICT=1` converts SKIP to failure in CI |
| `flutter test` step removed | `.github/workflows/ci.yml` | no Dart tests remain; `flutter analyze` kept |
| **doc-signature guard rehomed** | `.github/workflows/ci.yml` | it ran *only* in the archived slice, whose comment said it "has no other home that runs". It is the check that caught three invented `NostosDatabase.supabase` parameters |
| slice-count claims corrected 10 → 9 | `Makefile:82`, `sdk-e2e.sh:2,20`, `docs/adr/0013:9` | |
| archived paths repointed | `docs/adr/0022` ×2 | |

**"10/10 platforms" parity claims were left alone deliberately** — those count SDKs that exist, and
the Flutter SDK still exists; only its example and tests moved. Slice count changed, breadth did not.

### The cost, recorded rather than softened

Two real losses, both on the flagship SDK:

1. **No automated test coverage of the Flutter SDK's Dart surface.** The CI `flutter` job ran
   `flutter test` on every push.
2. **No live sync proof for Flutter.** It was the only slice running the *real* `nostos-server`
   binary; the other 9 spawn the in-process `e2e_server` spine.

Restoring both is a Flutter host app + Dart tests under `fixtures/` — i.e. this plan.

## 5. Target layout

```
fixtures/
  <sdk>/pomodoro/          # flutter, web, node, kotlin, swift, dotnet, react_native, capacitor, tauri
    supabase/schema.sql    # per-SDK tables
    env.example.json       # per-SDK creds + TWO users
    tool/                  # cairn_live_{up,down}.sh, mint_jwt.sh  (shared, symlink or generated)
  shared/
    spec/keys.json         # THE contract: key namespace + assertion list (see §8)
    spec/personas/         # multi-actor journeys
```

## 6. Supabase — per-SDK tables and users

Per-SDK table names, all in **`public`** (a non-`public` schema is known-broken: `view_name`
collapses the dot to `myschema_tasks` while `Collection.watch` emits `myschema.tasks` — ADR-0028):

```
pomodoro_<sdk>_sessions     -- id, user_id, started_at, duration_secs, phase, completed_at
pomodoro_<sdk>_community    -- id, user_id, is_running, started_at, duration_secs  (the shared register)
pomodoro_<sdk>_presence     -- id, user_id, last_seen_at                            (requirement 7)
```

Shared tenant column **`user_id`** across every table — `NOSTOS_TENANT_COLUMN` is one global name,
so per-SDK tables cannot each pick their own *(pending slice-8 confirmation)*.

**Two users per SDK**, because community needs ≥2: `pomodoro+<sdk>-a@…`, `pomodoro+<sdk>-b@…`.
Extends `env.example.json` to `SUPABASE_TEST_EMAIL_A/B` + passwords.

Server requirements, each a documented foot-gun:

- `NOSTOS_WRITE_TABLES` must list **every** table — empty means all writes refused.
- The publication must include every table, with `REPLICA IDENTITY FULL` (F1, needed for DELETE
  replay).
- RLS per the existing pattern: `enable row level security` + `auth.uid() = user_id`. Note nostos's
  own connection bypasses RLS by construction — RLS here protects the *direct-Supabase control
  path*, not the sync path.

## 7. App spec

Three shells, one state machine, identical across SDKs:

- **Dashboard** — streak count, latest session, recent sessions list. Pure derived reads.
- **Sessions** — CRUD + the player. Player drives the injected ticker, never wall-clock.
- **Community** — shared register (D1) + live presence (requirement 7).
- **Auth** — email+password sign-in, and sign-out that **stops sync, closes the socket, and clears
  local SQLite.** Local data surviving a logout is a real multi-user-on-one-device leak; that is an
  assertion, not a nicety *(per-SDK support pending slice 5)*.

## 8. How "identical" becomes checkable

`fixtures/shared/spec/keys.json` is the single source of truth: every asserted element's key
(`dashboard.streak`, `sessions.player.start`, `community.presence.count`, …) plus the ordered
assertion list per journey. Each SDK maps that namespace onto its platform idiom
(`Key()` / `testID` / `AutomationId` / `contentDescription`). A guard test per SDK — the
generalization of `persona_mapping_test.dart` — fails when an SDK drifts from the spec.

That makes "exactly the same" a mechanically enforced property instead of a promise.

## 9. Staging

**0. DONE — the sweep/archive (§4).**

Then **four** ratified engine workstreams. 1 and 2 are independent and may run concurrently; 3 should
land last because it changes the wire; 4 carries a trait-design dependency that has to be settled
*before* 1 freezes its protocol (see WS4's closing note).

1. **Web durability** — ADR-0017 Worker + SQLite-WASM/`opfs-sahpool`. Spawn the Worker, define the
   `postMessage` command protocol, marshal `RowOp`/`PendingWrite`, and **move the WebSocket
   transport in too** (it cannot call sync storage from the main thread). Must land `Storage` **and**
   `Outbox` together — rows-only leaves an offline write still throwing. Safari Private Browsing
   disallows OPFS, so the in-memory fallback stays. ADR-0017's own admission: no Node-verifiable
   test path.
2. **Reactive facade → all 9** — generalize ADR-0024's `Collection<T>`/`watch`. Floor set by
   `nostos_dotnet` and `nostos_swift`, which have zero reactive code today.
   **DESIGN RESOLVED 2026-07-31 (verified in code):** the push channel **already exists** —
   `SyncClient.subscribe_changes()` → `broadcast::Receiver<ApplyOutcome>` (`client.rs:387`), capacity
   64, **no replay**, fired at `:469` (after a local write), `:701` and `:1030` (after remote applies).
   So the 5 native SDKs (kotlin, swift, dotnet, node, tauri + RN-via-kotlin) are **pure binding work
   over the existing channel** — no nostos-client change. The load-bearing invariant every binding must
   encode (tested `client.rs:1473`): **`subscribe_changes()` BEFORE the first snapshot read**, or a
   commit landing between the two is permanently lost (broadcast has no replay). Each SDK owns a
   replay-last-snapshot-to-late-subscribers layer (Flutter's `_replayLatest`). Full-snapshot per tick,
   not diffs. **web + capacitor stay blocked on WS1** — they drive the in-memory WASM `NostosEngine`,
   not `SyncClient`, so the channel cannot reach them until WS1's Worker hosts a real
   `SqliteWasmStorage`; their `watch()` is currently a ponytail stub. **Thin slice = kotlin** (UniFFI
   callback interface): proves the port and unblocks RN (wraps kotlin) + informs swift/dotnet.
3. **CRDT merge tier** — new ADR, per-field causal metadata, merge logic in `nostos-core`.
   **Gated by D7: it ships with before/after benchmark numbers or it does not merge.** The wire
   carries no causal metadata today — ordering is purely LSN/server-arrival — so this *is* a
   wire-format change, and **833,307 ops/sec @ 1k clients at 0.00% drops** is the figure at risk.
   That is nostos's central competitive claim, not merely a test. Re-run `make bench` on the same
   recorded environment; per `CLAUDE.md` and the Tier-5 index precedent, a regression is a revert.
   Note the wire is also deliberately human-debuggable JSON "until a measurement says otherwise" —
   per-field causal metadata is exactly the kind of change that pressures that rule, so decide it in
   the ADR rather than by accident.

**4. Sign-out + local wipe — RATIFIED (D6).** Requirement 6 cannot be honestly tested without it,
and a multi-user pomodoro on one device is precisely the shape that leaks.

**Sizing: this is new engine work, not binding exposure.** Grep for
`fn (wipe|clear|reset|purge|truncate|drop_all|delete_all|sign_out|logout)` across
`crates/nostos-core/src` and `crates/nostos-client/src` returns **nothing** (verified 2026-07-31).
`Storage` exposes `checkpoint`, `epoch`, `save_epoch`, `apply_batch`, `pks_for_table`, `delete_pks`;
`Outbox` exposes `enqueue`, `pending`, `mark_done`, `bump_attempts`, `mark_dead_letter`,
`apply_local`, `pending_pks_for_table`. Nothing clears either.

- **4a. A `clear()` primitive on `Storage` and `Outbox`** — sync, WASM-clean (`nostos-core` has no
  tokio). Composing it from `pks_for_table` + `delete_pks` is O(rows) *and* misses the four things
  that actually matter:
  - **the checkpoint** — wipe the rows but keep the LSN and the next user resumes from it, never
    receives a snapshot, and sees an empty database permanently. Same failure class as the
    resume-without-snapshot unsoundness already on record.
  - **the epoch** — a stale epoch makes the oplog epoch check misfire on next login.
  - **the outbox** — see 4b.
  - **the dead-letter queue** (ADR-0027).
- **4b. Decide what happens to pending writes. This is the ADR, not an implementation detail.**
  Unsynced outbox entries belong to the user signing out. Keep them and they replay under the *next*
  user's token — cross-user write attribution and a tenant violation. Discard them and that user's
  offline work is destroyed silently. Three candidates: block sign-out until the outbox drains (fails
  offline, which is the whole point of the product); discard with a loud surfaced outcome (ADR-0027's
  dead-letter surfacing already exists); or **persist per-principal and refuse to replay across a
  principal change**. **Recommend the third** — the only one that neither loses the data nor
  misattributes it.
- **4c. Expose `setToken` + `signOut` in the 8 non-Flutter bindings.** `set_token` already exists in
  `nostos-client` (`client.rs:351` — token behind a `RwLock`, read by `connect_url`) and in Flutter
  (`nostos.dart:336`). Every other binding takes an opaque token with no swap primitive at all.
  Surfaces: UniFFI (kotlin, swift), napi (node), wasm-bindgen (web, capacitor), a tauri command,
  RN TurboModule, dotnet P/Invoke.
- **4d. Server `exp` enforcement — and the ordering trap.** `auth.rs:74-78` skips `exp` deliberately
  (Phase 0: "GoTrue mints short-lived tokens and the gateway handles expiry"), and auth runs **once**
  at WS upgrade with no re-check, so an open socket outlives its token indefinitely. Enforcing it
  means checking `exp` at upgrade *and* dropping the socket on expiry.
  **This must land AFTER 4c, never before.** Enforce `exp` while only Flutter has a refresh trigger
  and the other 8 SDKs disconnect about an hour after login with nothing to recover them — turning a
  silent problem into a loud outage on 8 platforms.
- **4e. Tests — the leak test is the point.** Nothing anywhere exercises a real Supabase sign-in
  today. The one that matters: **user A signs in, writes, signs out; user B signs in on the same
  device; B must not see A's rows, and A's unsynced writes must not be attributed to B.** Plus a
  resume assertion proving the checkpoint was cleared — B receives a snapshot, not an empty database.
- **4f. ADR-0029** (next free number) records 4b's choice and 4d's ordering constraint.

**Dependency on workstream 1 — and it runs opposite to the obvious order.** 4a adds methods to
`Storage` and `Outbox`; workstream 1 marshals every `Storage`/`Outbox` method across a `postMessage`
protocol. **Design 4a's trait surface before WS1 freezes that protocol**, or WS1 gets designed twice:
the required-method count goes 7 → 9, and the ADR-0017 addendum's parity estimate moves with it.

- **4g. Trait contract — the thing WS1 must marshal (DESIGNED 2026-07-31).** Two new sync,
  WASM-clean methods. This is the stable surface WS1's `postMessage` protocol fronts, so the Worker
  is not designed twice:

  ```rust
  // nostos-core/src/storage.rs
  /// Clear ALL persisted state: rows, the checkpoint, AND the epoch.
  /// The checkpoint MUST reset to 0 — a stale LSN makes the next principal resume
  /// past the snapshot and see an empty DB permanently (resume-without-snapshot
  /// unsoundness, same class). Sign-out / principal-change only. ADR-0029.
  fn clear(&mut self) -> crate::Result<()>;

  // nostos-core/src/outbox.rs
  /// Clear pending writes + the dead-letter queue (ADR-0027). The 4b decision
  /// (what happens to UNSYNCED writes on sign-out) layers ABOVE this: the discard
  /// option calls clear() directly; the per-principal option adds an outbox-internal
  /// principal tag + a refuse-replay-on-mismatch check — an impl policy, NOT a new
  /// trait method, so it is invisible to WS1's protocol.
  fn clear(&mut self) -> crate::Result<()>;
  ```

  **WS1 consequence (for the swarm):** the Worker `postMessage` protocol carries a `clear` command
  fronting BOTH clears **atomically** (one Worker transaction — half a clear is a data leak). With
  WS4 landed the ADR-0017 addendum's required-method parity moves **7 → 5 (1.4×) becomes 9 → 5
  (1.8× floor)** — both `clear()`s are required, not defaulted, because a no-op default would be a
  silent cross-user leak (the same "defaults degrade" trap as ADR-0025's other defaulted methods).

Then the fixtures:

5. `fixtures/shared/spec/keys.json` + multi-actor personas. **The persona convention needs
   extending, not just applying** — today it binds one persona to one journey, and the community
   shell needs two actors in one session.
6. **Flutter reference fixture** — most capable SDK, and it also restores the two coverage gaps §4
   opened.
7. Port outward one SDK at a time: native first (kotlin, swift, dotnet, node, tauri), then RN
   (Android — no iOS TurboModule), then web/Capacitor.
8. Extract the shared spine-spawn helper (§10) instead of reimplementing it a tenth time.

Nine apps at this spec is nine products. Stage it; do not fan out on step 5.

## 10. Per-SDK capability matrix

Verified against code 2026-07-30, not from memory.

| SDK | reactive? | reads | offline writes | Supabase auth wired | sign-out clears local |
|---|---|---|---|---|---|
| flutter | **yes** — `Collection<T>.watch` | SQL | yes | **yes — the only one** (`nostos_database.dart:217`) | **no** |
| node | no — poll | SQL | yes | no — opaque token | no |
| kotlin | no — poll | SQL | yes | no — opaque token | no |
| swift | no — poll | SQL | yes | no — opaque token | no |
| dotnet | no — poll | SQL | yes | no — opaque token | no |
| tauri | no — poll | SQL | yes | no — opaque token | no |
| react_native | no — poll | SQL | yes | no — opaque token | no |
| web (browser) | no — poll | **KV bytes** | **no — write throws** | no — opaque token | no |
| capacitor | no — poll | **KV bytes** | **no — write throws** | no — opaque token | no |

Evidence for the reactive column: 25 files carrying reactive tokens under `sdk/nostos_flutter` versus
0–4 for every other SDK, and every `watch` signature in the tree is Flutter's. `nostos_dotnet` and
`nostos_swift` contain **zero** reactive tokens — they start from nothing, which is the floor on
workstream 2's cost.

### Three cross-cutting findings the requirements depend on

1. **No SDK implements sign-out that clears local SQLite — including Flutter.** `Nostos.close()`
   closes the engine and the state stream only; its docstring (`nostos.dart:314`) says outright "Does
   not delete the local SQLite file". A repo-wide grep for `signOut`/`sign_out` in source returns
   zero hits outside Flutter's own Supabase dependency. So requirement 6's sign-out is **new work on
   all 9 SDKs**, and today a logout-without-wipe leak exists everywhere — worst on Flutter, the only
   SDK with a session concept to leak from.
2. **The server never checks `exp`.** `auth.rs:74-78` documents this as deliberate Phase-0 scope
   ("Supabase's GoTrue mints short-lived tokens and the gateway handles expiry"); signature and a
   non-empty `sub` are verified, nothing more. Auth runs **once at WS upgrade** and is never
   re-checked, so an open socket is never dropped when its JWT expires. Expiry only bites on the next
   reconnect — which is exactly why Flutter's `onAuthStateChange` listener matters and why no other
   SDK has an equivalent trigger.
3. **No test anywhere exercises a real Supabase sign-in.** `crates/nostos-infra/tests/auth_sync.rs`
   covers server-side accept/reject/tenant-isolation with minted tokens; Flutter's `setToken`
   delegation is unit-tested against a fake engine. The real sign-in → refresh → sign-out path is
   untested end to end, which is precisely what requirement 6 would close.

### Harness facts that shape the fixture design

- **No sdk-e2e slice touches real Postgres or Supabase.** 9 of 10 spawn
  `crates/nostos-infra/examples/e2e_server` — a hardcoded `AllowAnonymous` in-process spine with no
  `NOSTOS_REPLICATOR` / `NOSTOS_SYNC_AUTH` / `NOSTOS_WRITE_TABLES` at all. Real-PG e2e is a separate
  suite (`NOSTOS_E2E_PG=1`, `make dev-stack`) unreachable from `make sdk-e2e`. **A Supabase-backed
  fixture matrix is therefore new capability, not a re-wiring** — and it is where
  `NOSTOS_WRITE_TABLES` (empty ⇒ all writes refused) will bite first.
- Slice discovery contract: bind `127.0.0.1:0`, print `NOSTOS_E2E_PORT=<port>` then
  `NOSTOS_E2E_READY`, flush.
- **No shared spawn helper exists** — kotlin, dotnet and web each reimplement "spawn spine, poll
  stdout for port, trap cleanup". A 9-app matrix should extract that once rather than a tenth time.
- `dotnet`'s verdict is **exit code only**, no log assertion — the weakest verdict in the suite.
- The `cmd | grep -q` SIGPIPE-under-pipefail bug is **already fixed** in the kotlin harness
  (`[[ ]]` against a here-string). No live instance remains. Do not reintroduce it.

## 11. Status — design wave RESOLVED 2026-07-31

**Done and verified:** the sweep/archive (§4) + its fallout; all seven decisions (§3); the WS4
`clear()` trait contract (§9 4g); the analyzer-fix. **Design wave complete** — three read-only
spikes (ws1/ws2/ws3) ran, all primary-source-verified, and two new ADRs are written:

| | Workstream | State |
|---|---|---|
| WS1 | web durability — ADR-0017 Worker + SQLite-WASM | **DESIGNED** (executes existing ADR-0017; protocol below) |
| WS2 | reactive facade → all 9 — ADR-0024 generalization | **DESIGNED**; kotlin thin-slice IN FLIGHT (worktree) |
| WS3 | CRDT tier | **DESIGNED → ADR-0030 Proposed**; scope decision pending (below) |
| WS4 | sign-out + local wipe — `clear()` on `Storage`/`Outbox` | **DESIGNED → ADR-0029 Proposed** |

### WS1 design (executes ADR-0017 — no new ADR; protocol captured here so it survives compact)

- **Architecture:** one Worker owns the engine + WS transport + `SqliteWasmStorage` (impls
  `Storage` AND `Outbox`); main thread is a thin `postMessage` proxy. WS must live in the Worker —
  `createSyncAccessHandle` is Worker-only and the apply loop calls `apply_batch` synchronously
  against the same DB the WS echo writes into.
- **Boundary commands (only these cross; the rest — apply_batch, pks_for_table, delete_pks, pending,
  mark_done, bump_attempts, mark_dead_letter, epoch, save_epoch — run intra-Worker):** `connect`,
  `write`, `rowsFor`, `checkpoint`, `close`, `clear` (WS4). Push events: `rowsChanged`,
  `writeResult{client_write_id,ok}`, `status`.
- **BREAKING `write` change:** today `NostosSocket.write` throws synchronously when the socket isn't
  OPEN; after WS1 it always enqueues (durable-first via `Outbox::enqueue` + instant row via
  `apply_local`) and the throw vanishes. `e2e/browser_live.spec.cjs` must be rewritten to assert the
  async `writeResult{ok}`. On landing the **"live-only" ceiling docs go stale** and must be updated:
  `sdk/nostos_web/README.md` (Ceiling), `docs/api/web.md`, `docs/api/README.md` (matrix warning).
- **VFS:** `opfs-sahpool` (no COOP/COEP tax, unlike wa-sqlite; atomicity, unlike raw OPFS). Safari
  Private Browsing → `InMemoryStorage` + `localStorage` checkpoint fallback (storage-backend-level;
  correctness preserved, only durability lost).
- **Marshalling:** RowOp payload crosses as a transferable `Uint8Array` (zero-copy on the echo hot
  path); `PendingWrite.payload_json` crosses inline (already a string).
- **Dual-entry decision RESOLVED 2026-07-31 (ws1-thin → option c):** NOT a separate crate.
  `wasm-pack --target web` emits one ES module + one `.wasm` with no entry selector, and the
  generated `nostos_ffi_wasm.js` has **0 main-thread-only globals** (`window.`/`document.` grep = 0),
  so the Worker is the sole consumer of the existing artifact and the main thread becomes a pure
  `postMessage` proxy that imports NO wasm. **Zero Rust changes.** ws1-thin proved the boot boundary
  (Worker loads the wasm, round-trips a `postMessage`, returns `checkpoint`). Remaining gap for the
  full build: confirm the heavier in-Worker path — `NostosSocket` (web-sys WebSocket) + the
  OPFS / `FileSystemSyncAccessHandle` SQLite-WASM VFS — boots; this slice exercised only `NostosEngine`.
- **Test plan:** Playwright + bundled Chromium (OPFS since 102+), `launchPersistentContext`;
  durability (offline write → reload → present), drain, Safari-fallback stub. **Open gap: OPFS
  persistence across reload in HEADLESS Chromium — unverified.** ADR-0017's "no Node path" is about
  Node, not headless Chrome.

### WS3 design → ADR-0030 — SCOPE DECISION PENDING (the headline)

The design **falsified the workstream's premise**, verified against source (see ADR-0030):

- **Counter → NOT a CRDT.** nostos is server-authoritative (ADR-0013); an `op:"increment"` delta that
  `WriteBack` translates to `UPDATE SET val=val+?` lets Postgres serialize concurrent +1/+1 → +2.
  Zero wire cost. **The "community total pomodoros" use case needs a delta op, not a CRDT.**
- **Add-wins OR-set → genuine CRDT** (tags/presence). Per-element HLC inside the opaque payload.
- **WireFrame is unchanged** → the 833k@1k fan-out hot path is unaffected (it doesn't apply rows).
  Moat risk is **~0 IF no top-level wire fields are added and the bench payload isn't fattened** —
  both avoidable. Benchmark gate (D7): `make bench --clients 1000`, 3× median,
  `NOSTOS_FAKE_EPS=0 NOSTOS_FAKE_KEYS=0`; revert if >3% regression (<808k vs 833,307) or any drop.
- **DECISION NEEDED:** confirm the scope = counter delta-op + OR-set CRDT (recommended, low-risk),
  NOT a full CRDT tier (PN-counter etc.) the architecture has no use for.

### Also outstanding

- **`fixtures/` is empty by design** (D0); the 228 deletions are committed (`f0f3986`).
- **The sweep boundary** — "take it all" covered Flutter's example+tests only; the other 8 SDKs'
  test/e2e dirs and `crates/*/examples/` were left alone (wired into the 9 surviving slices).

**Next actions:** (1) operator confirms WS3 scope; (2) kotlin thin-slice returns → fan out the WS2
native reactive ports (worktree-isolated + commit coordinator); (3) WS1 thin slice = validate the
Worker-entrypoint crate decision; (4) WS4/WS3 implementation follows their ADRs.
