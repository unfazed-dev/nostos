# Nostos Completion Assessment — 2026-07-29

**Status:** assessment COMPLETE · **A1/A2/A4/A6 IMPLEMENTED same day** (see addendum)

> ## Addendum — 2026-07-29, later that day: A1–A6 implemented
>
> The operator authorised implementation after this assessment was written. What shipped,
> and what it changed about the findings above:
>
> | Item | Outcome |
> |---|---|
> | **A1** | **DONE — and the diagnosis in §2.3 was half wrong.** Restored `sdk/nostos_flutter/example/`; the SDK integration test now **PASSES** (`connect → subscribe → fan-out → watch() emits`, real macOS `.app` + real `cargo run -p nostos-server`). |
> | **A2** | **DONE.** `WriteQueueStatus` on a `watch` channel in `nostos-client` → FFI `watchWriteStatus()` → `SyncStatus.{pendingWrites, deadLetteredWrites, lastWriteError}` + 4 derived getters. ADR-0027. |
> | ~~A3~~ | Stays withdrawn (already wired via `nostos init --write-tables`). |
> | **A4** | **DONE.** Swift guard now requires a *booted* simulator, matching the Android guards. |
> | **A5** | **DONE** (committed first, `fade479`). |
> | **A6** | **DONE.** New `sdk-e2e` CI job runs `rust node tauri` under a new `SDK_E2E_STRICT=1`. |
>
> ### Correction: there was never an Xcode/SPM machine fault
>
> §2.3 claims two independent faults, one of them environmental. **That was wrong, and the
> error was mine.** When I tested the restored tree I ran the harness from the *plugin*
> directory (`sdk/nostos_flutter/`), which is what the harness did at HEAD — not from
> `example/`. The SPM error came from that invalid invocation.
>
> Run correctly, the true failure at HEAD is `No macOS desktop project configured`, and
> there is exactly **one** fault: `9322d83` moved the test into a package that cannot host
> it. The move was botched rather than a tradeoff — the moved file was byte-identical
> except its docstring, and it still computes `repoRoot` as `Directory.current.path/../../..`,
> which is right from `example/` and points *above* the repo from the plugin dir.
>
> **This closes C5c**, the largest honest unknown in this document. Flutter sync is now
> demonstrated end-to-end, not assumed.
>
> ### A2 landed with a different error semantic than proposed
>
> Part 7 said "add `pendingWrites` + `lastWriteError`". As built, **`lastWriteError` is set
> only on dead-letter**, never on an ordinary `WriteResult{ok:false}` — `client.rs:172`
> documents those as routinely transient and self-healing, so surfacing them would train
> users to dismiss write errors. `pending > 0` is likewise *not* an error; it is the
> offline-first promise working. Rationale in ADR-0027.
>
> ### New finding from A2's verification (pre-existing, not caused by A2)
>
> The zero-setup fake-replicator environment **saturates any session that outlives a few
> seconds**: `main.rs` runs `FakeReplicatorConfig::small(u64::MAX)` with no pacing knob
> (`fake.rs` has none), and each `watch` pump re-queries the full, monotonically growing
> table per change tick — quadratic. Native stack sampling (1,717 samples of a wedged run)
> showed ~80% of a worker inside `emit_snapshot` + hex/JSON/SSE encoding + a Dart GC storm,
> and **zero frames** in the new write-status pump. The SDK e2e passes by finishing in ~3s.
> Named fix (future work, needs its own measurement): a pacing/bound env knob on the fake
> replicator, or a default `watch` throttle. Recorded in ADR-0027's consequences.
>
> Verification-tooling traps recorded for the next agent: (1) frb's content hash covers the
> *generated bindings* only — a Rust logic change with unchanged signatures sails through
> the hash check on a stale dylib, so `flutter clean` after ANY glue-crate edit before
> trusting a run; (2) macOS `sample <pid>` on the wedged app was the tool that ended three
> rounds of wrong theorizing in one shot.
>
> ### Still open
>
> **A7** (moat-number drift), **A8** (mark superseded plans), **A9** (boot a device to close
> kotlin/swift/reactnative) — not requested in this pass. C8, C15, C17 remain unknown; C17
> (the stranger test) is operator-only by construction.
>
> ### A10 — DONE 2026-07-30 (fake-replicator firehose)
>
> Two opt-in knobs on `FakeReplicatorConfig`, both `0` = today's unbounded behaviour:
> `paced(events_per_sec)` (per-event `tokio::time::sleep`) and `recycling_keys(n)`
> (`pk = emitted % n + 1`). `nostos-server`'s two fake branches now default to
> **20 events/sec over 50 keys** (`NOSTOS_FAKE_EPS` / `NOSTOS_FAKE_KEYS`, `0` to firehose
> deliberately). `nostos-bench` builds its own config, so the 833k-ops/sec ceiling is
> untouched — verified by inspection: `crates/nostos-bench/src/main.rs:226` calls
> `FakeReplicatorConfig::{small,large}` directly, never the server CLI.
>
> **Recycling is the load-bearing half, not pacing.** Client apply is an upsert
> (`ON CONFLICT(table_name, pk) DO UPDATE`, `nostos-client/src/sqlite.rs:548`), so a bounded
> key space bounds the *table*, which is what makes the Flutter glue's per-tick full-table
> `emit_snapshot` O(1) in session length instead of O(events). Pacing alone only slows the
> quadratic. Checks: `recycling_keys_bounds_the_key_space` and `pacing_throttles_emission`
> in `crates/nostos-infra/src/replicator/fake.rs`.
>
> **Measured (debug build, 10 s, `nostos-server` alone with NO client connected):**
>
> | config | server CPU |
> |---|---|
> | `NOSTOS_FAKE_EPS=0 NOSTOS_FAKE_KEYS=0` (the old default) | **100.0%** — a full core |
> | new default (20 eps / 50 keys) | **0.0%** |
>
> A full core burned with nothing observing it — that is the firehose, and it is gone.
> What is *not* re-measured: the client-side Flutter saturation. The O(1)-snapshot claim
> follows from the upsert + bounded key space, but no device run was done in this pass
> (A9 remains unauthorized). `make ci`: exit 0, 435 passed. The SDK e2e harness
> (`sdk/nostos_flutter/example/integration_test/nostos_server_test.dart`) was checked for
> sensitivity to the new caps — it spawns `cargo run -p nostos-server` with only
> `NOSTOS_BIND` set, and both assertions are `isNotEmpty` under 15 s timeouts, which 20 eps
> satisfies in well under a second. No harness asserts a row/event count above 50.
>
> Blast radius stayed narrow because only the Flutter glue re-snapshots per change tick
> (`sdk/nostos_flutter/rust/src/api/nostos.rs`); kotlin/dotnet/swift poll instead, node/tauri
> have no watch pump. So the server-side default is the whole fix — no per-SDK change.
>
> **A10 follow-up, 2026-07-30:** the flutter slice of `make sdk-e2e` PASSES with the new
> 20 eps / 50 keys default in force. That upgrades the "no harness asserts a count above 50"
> reasoning above from *assumed* to *verified* — the analysis was right, and it is now
> empirically confirmed rather than argued.
>
> ### C15 — CLOSED 2026-07-30 (per-SDK publishability + stub-vs-real)
>
> C15 was `unknown` ("3 subagents dispatched for this returned nothing"). Now established.
>
> **Stub-vs-real: settled by A9.** No slice is a stub on the sync path — all ten prove a live
> PUSH+ECHO round-trip. This third of C15 is closed by the strict run, not by inspection.
>
> **Publishability: 1 of 9 SDK packages is publishable today.**
>
> | sdk | version | LICENSE file | repository field | README | verdict |
> |---|---|---|---|---|---|
> | `nostos_flutter` | **0.1.0** | ✅ | ✅ | ✅ | **publishable** |
> | `nostos_capacitor` | 0.0.0 | ❌ | ❌ | ✅ | blocked |
> | `nostos_dotnet` | 0.0.0 | ❌ | ❌ | ✅ | blocked |
> | `nostos_react_native` | 0.0.0 | ❌ | ❌ | ✅ | blocked |
> | `nostos_web` | 0.0.0 | ❌ | ❌ | ✅ | blocked |
> | `nostos_kotlin` | 0.0.0 | ❌ | ❌ | ❌ | blocked |
> | `nostos_swift` | 0.0.0 | ❌ | ❌ | ❌ | blocked |
> | `nostos_tauri` | 0.0.0 | ❌ | ❌ | ❌ | blocked |
> | `nostos_node` | 0.0.0 | ❌ (no `license` field either) | ❌ | ❌ | blocked, worst case |
>
> Common blockers: placeholder version `0.0.0` (8/9), no per-package `LICENSE` (8/9), no
> `repository` field (8/9) — all hard or near-hard requirements on npm / crates.io / NuGet /
> Maven. Four have no README at all (kotlin, node, swift, tauri). `nostos_tauri` has no
> ecosystem manifest beyond `Cargo.toml`, so it would publish to crates.io and needs the
> license/description/repository keys there.
>
> **This does not block the Flutter+Supabase wedge** — `nostos_flutter` is the one that is
> ready, which is consistent with the strategy. But it means **"10/10 SDK parity" is a
> *functional* claim, not a *distributable* one**, and those must not be conflated in public
> copy. Checked accordingly: `README.md` makes no install claims (good), and
> `show-hn-draft.md` said "Flutter/RN/Node SDKs are scoped, not shipped" — which now
> *understates* verified work. Rewritten to the accurate split: functional and e2e-proven,
> not yet registry-published.
>
> **New follow-up (not done here): A11 — packaging pass.** Set real versions, add per-package
> LICENSE + repository, write the four missing READMEs. Mechanical, ~1 session, needed before
> any "install nostos for <platform>" claim.
>
> **Not established:** README *drift* (whether each SDK's code samples still compile against
> its exported surface). That needs per-sample typechecking, which I did not run. Remains
> genuinely open — the one part of C15 I could not close.
>
> ### A7 — DONE 2026-07-30 (moat-number drift) — and my first call on it was WRONG
>
> I initially reported A7 as "effectively already done, only a rounding residue." That was
> **under-called**, from a coarse `grep | uniq -c` that collapsed duplicates and never
> compared exact figures across files. An exact sweep found real drift:
>
> | location | was | now |
> |---|---|---|
> | `README.md:13`, `README.md:23` | 833,**308** | 833,307 |
> | `docs/launch/show-hn-draft.md:51` | 833,**308** | 833,307 |
> | `docs/launch/powersync-vs-nostos-draft.md:44` | 833,**308** | 833,307 |
> | `benches/results/chart.svg` (the rendered chart) | 833,**308** | 833,307 |
>
> Canonical is `benches/results/RESULTS.md` = **833,307**. Two of the four wrong figures were
> in the **public launch drafts**, and one was in the chart image that ships in the README —
> i.e. a number that contradicts our own published benchmark file, going out on Show HN.
> `git grep 833,308` now returns nothing.
>
> `chart.svg` is *generated* (`crates/nostos-bench/src/report.rs:141`) from the same
> `grouped(run.ops_per_sec)` helper as the RESULTS table, so this was **not** a generator
> bug — the committed chart was simply from an older run than the committed RESULTS.md. The
> next `make bench` regenerates both consistently. No re-benching was done (A7 is docs-only;
> re-running would change the headline within ±5% noise, which is a separate decision).
>
> ### A9 — DONE 2026-07-30 — **10/10 slices PASS in strict mode**, and §2.2 is superseded
>
> `SDK_E2E_STRICT=1 make sdk-e2e` → **exit 0, all ten slices PASS, zero skips** (strict mode
> makes any skip fatal, so this green cannot be a self-skip false pass):
>
> | rust | node | tauri | web | capacitor | dotnet | flutter | swift | kotlin | reactnative |
> |---|---|---|---|---|---|---|---|---|---|
> | 2s | 1s | 2s | 2s | 2s | 4s | 30s | 6s | 11s | 12s |
>
> This **supersedes §2.2's "6 PASS / 2 FAIL / 2 SKIP — the headline finding"**. It also closes
> C7 (*assumed*: swift would pass with a booted sim) and C8 (*unknown*: kotlin/reactnative
> health) — both now **verified**.
>
> **Neither original failure was an SDK defect. Both were harness bugs**, and both would have
> hit anyone who booted a device and tried:
>
> 1. **swift — hardcoded simulator UDID.** `sdk/nostos_swift/ios-test/build.sh` gated on
>    *any* booted sim (`sdk-e2e.sh:117`) but then targeted a **specific hardcoded UDID**
>    (`CAFC93F7…`, "iPhone 17 probe"). Boot any other device and the guard goes green, then
>    `simctl install` dies with `Unable to lookup in current state: Shutdown` — reported as a
>    swift FAIL with nothing wrong in the SDK. Now derived from
>    `simctl list devices booted`, so guard and action agree by construction.
>    The same file also **hardcoded absolute paths to one machine**
>    (`/Volumes/developer_ssd/…`), which would have broken the slice on any other checkout —
>    now derived from `${BASH_SOURCE[0]}`.
> 2. **kotlin — logcat double-dump race.** The harness dumped logcat **twice**: `-d -t 800`
>    for the human spool, then an unbounded `-d` for the verdict. On a chatty emulator the
>    proof lines rotated out of the readable window between the two adb round-trips, so the
>    spool printed `[kt-e2e] ECHO_OK` while the verdict recorded `ECHO_OK=0` — with the
>    instrumented test green (`tests=2 failures=0`). That is precisely the flake its own
>    comment documents as "fixed in the RN harness 2026-07-13"; kotlin kept the racy shape.
>    Now a single `$LOGCAT_DUMP` feeds both, which removes the race by construction rather
>    than widening the buffer and hoping. Side effect: 34s → 11s.
>
> **Correction to C8's stated blocker.** The assessment recorded "Never executed — no Android
> emulator on this machine." That is **false**: five AVDs exist (`Medium_Phone_API_36`,
> `Pixel_9`, `nostos_api34`, `pack-9`, `probe_arm64`). `nostos_api34` boots headless in ~15s.
>
> **Operator note for reproducing.** Boot `nostos_api34` on **port 5556** specifically
> (`emulator -avd nostos_api34 -no-window -port 5556`) — the kotlin harness expects
> `emulator-5556` and boots it itself if absent, so booting that AVD on the default 5554
> instead holds its lock and deadlocks the harness's own boot. Any iPhone sim works for swift.
>
> Also tightened the phrasing: "208× PowerSync's published ceiling (~2–4k ops/sec)" conflated
> the range with the multiple (208× is against the **4k high**; against the 2k low it is 417×).
> `README.md` and `CLAUDE.md` now name the high ceiling explicitly and cite 417× for the low,
> matching what `show-hn-draft.md:51` already said correctly. `CLAUDE.md` gained a standing
> rule: quote the high multiple, never the low, and never a figure absent from RESULTS.md.
>
> **Correction 2026-08-06:** the N× vs PowerSync framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.
>
> Everything below is the original assessment, unedited.

---

**Status:** COMPLETE (assessment only — no implementation performed, per standing scope rule)

## Verdict in one paragraph

The **engine is sound and CI is green (431 tests, exit 0)**. Six SDKs plus the Rust spine
prove a live replication round-trip today — a genuinely strong, under-sold result. Two
things block the Flutter+Supabase wedge. First, **the flagship `nostos_flutter` SDK has no
live proof that runs**: commit `9322d83` deleted the example host app a plugin package
needs, and on this machine an Xcode/SPM toolchain fault fails it even with the host app
restored (§2.3 — I tested both trees). Second, and more serious because it is a product
defect rather than a harness one, **the Dart SDK cannot tell a developer that a write
failed** (§6.1): `SyncStatus` exposes 2 fields where PowerSync exposes 12, with no error
field at all, so no Nostos app can show its user "your change didn't save." The remaining
engineering work is **A1–A5 in Part 7 — roughly a day**, after which the honest status is
*"engineering-complete for the wedge; blocked only on the operator-run stranger test and
publish."* Breadth/parity with PowerSync's full catalogue is **not** a launch gate and
should stop being scored as one.

> **Two self-corrections are recorded in this document rather than quietly fixed**, because
> both illustrate the failure mode this repo keeps repeating — trusting a plan doc over the
> code. (1) I first reported the flutter failure as a regression caused by the newest
> commit; testing the pre-regression tree **falsified** that (§2.3, C5). (2) I first
> reported `NOSTOS_WRITE_TABLES` as missing from the quickstart; it is wired there via
> `nostos init --write-tables` (§1.1, C9). Both original claims came from a stale 2026-07-20
> plan. **Read plan docs as history, not state.**
**Supersedes:** the *status sections* of `nostos-next-after-oplog-epoch-2026-07-20.md`,
`launch-readiness-gap-list.md`, `powersync-sdk-parity-plan.md`,
`sdk-parity-final-three.md`, `sdk-live-e2e-consolidation.md`.
Those plans' **designs** remain valid; their **"COMPLETE" claims are re-scored here**
against a fresh run. This document is the single stage-of-project answer.

**Method:** fable-mode Gate 0–5. 8 parallel subagents (SDK×3, docs-reconciliation,
test-reality, DX-surface, competitor-DX research, local-first UX research) +
orchestrator-run commands. Every claim below carries a command, file:line, or an
explicit `assumed`/`unknown` tag.

---

## Part 0 — Why this document exists

The repo carries **22 plans** in `docs/plans/` that contradict each other on the
single most important question: *is this done?* The clearest example, both inside
one file:

- `powersync-sdk-parity-plan.md` → *"Update (2026-07-12): all 7 shipped SDKs are now
  LIVE-replication-E2E-verified"* and `sdk-parity-final-three.md` → *"Outcome —
  COMPLETE (10/10, 2026-07-12)"*
- …while `powersync-sdk-parity-plan.md`'s own **Honest verdict** says
  *"Overall parity: **NO**."*

Both are true under different definitions, and that ambiguity is the actual blocker:
nobody can tell from the docs whether to ship. This assessment fixes a single
definition and scores against it.

**Definition used here.** Three distinct senses of "complete," scored separately:

| Sense | Definition | Gate |
|---|---|---|
| **D1 — Wedge-complete** | A stranger ships a working offline Flutter+Supabase app from published docs | ≤5-min stranger test + operator publish |
| **D2 — Breadth-complete** | All 9 SDKs verified against a live server on a clean machine | `make sdk-e2e` with 0 skips |
| **D3 — Parity-complete** | Feature-equal to PowerSync across its SDK catalogue | attachments, ORM, encryption, sync-streams |

D1 is what the master plan gates launch on. **D3 is explicitly not a launch gate**
and should stop being scored as if it were — it is the main source of false
"we're not ready" signal in the current docs.

---

## Part 1 — Verified evidence (orchestrator-run, this session)

Facts established by commands run on 2026-07-29, independent of subagent reports.

### 1.1 The "#1 launch blocker" is NOT open — claim withdrawn

The 2026-07-20 plan ranked this **Tier 0, action #1, "~2 lines, nearly free, do first"**:
*add `NOSTOS_WRITE_TABLES=tasks` to `docs/QUICKSTART.md`*. Grepping the **env-var name**
appears to confirm it is still open:

| File | `NOSTOS_WRITE_TABLES` (literal name) hits |
|---|---|
| `docs/OPERATING.md` | 9 |
| `docs/QUICKSTART.md` | 0 |
| `README.md` | 0 |

**That inference is wrong, and I made it before checking how the quickstart configures
writes.** The quickstart does not use the env var — it uses the CLI flag, at
`docs/QUICKSTART.md:42` (step 3 of the 6-step path):

```
cargo run -p nostos-cli -- init --db-url … --tables todos \
    --write-tables todos --tenant-column user_id
```

and explains it in prose at `docs/QUICKSTART.md:252`: *"the `--write-tables <tables>` flag
at step 3 is what enables writes."* `crates/nostos-cli/src/commands/init.rs:64-68` parses
it (and validates each entry also appears in `--tables`), `config.rs:47,157` persists it,
and `commands/deploy.rs:56,106` emits `NOSTOS_WRITE_TABLES` into the generated deploy
templates. **A stranger following the quickstart gets writes enabled.**

**Status: A3 withdrawn as a launch blocker.** README genuinely has no coverage (0 hits for
both the env var and `--write-tables`), but README's demo paths use `make dev-stack`, a
different flow — worth a consistency pass, not a launch gate.

> **Why this mistake matters more than the mistake.** This exact claim was already
> corrected in project notes on 2026-07-20 (*"QUICKSTART's cold path already sets writes
> via `nostos init --write-tables` (line 42); the real gap was STALE launch-blocker text"*).
> I re-derived the dead claim by reading the plan document instead of the quickstart.
> That is the same root cause as everything in Part 4: **the plans are history, and this
> repo keeps mistaking them for state.**

**Severity: UPGRADED, not downgraded — the premise holds, and the cause is worse.**
The 2026-07-20 plan justified this as *"writes silently no-op."* I initially read the
server code as falsifying that. Tracing the full path shows the opposite — the plan was
right about the symptom, and the real cause is a client defect, not a doc gap:

| Layer | Behaviour | Evidence |
|---|---|---|
| Server | Rejects with an excellent, actionable message naming the table **and** the env var | `crates/nostos-infra/src/transport.rs:786-799` |
| Wire | Carries it faithfully as `WriteResult{ok:false, msg}` | `crates/nostos-infra/src/wire.rs:74,150` |
| Rust client | **Swallows it.** Logs `"write rejected by server; stays queued, will retry"`, retries, then dead-letters after `dead_letter_max_attempts` | `crates/nostos-client/src/client.rs:710,718` |
| Dart SDK | **No surface at all.** Every `throw` is config/state (`FormatException`, `StateError`) — none is a write rejection | `sdk/nostos_flutter/lib/src/*.dart` |

The client source says so outright — `crates/nostos-client/src/client.rs:32`:
> *"…channel on every rejection; **the user-facing surface is a Phase-2 concern**."*

And `SyncStatus`, the ADR-0024 reactive facade's status object, has exactly **two**
fields — `conn` and `lastSyncedAt` (`sdk/nostos_flutter/lib/src/nostos_database.dart:498-515`).
There is **no** `hasPendingWrites`, **no** `error`/`lastError`, **no** dead-letter count.
The write API itself documents the hole (`nostos_database.dart:438`):
> *"Returns the local outbox id (**NOT a server ack**)"*

**Consequence.** With `NOSTOS_WRITE_TABLES` unset, the Flutter developer sees the row
appear locally (optimistic apply), never reach Postgres, and **receives no error in
Dart** — the write is retried, then silently dead-lettered. This is
**silent data loss from the developer's seat**, which is the one category that is
genuinely pre-launch (everything else is post-launch polish).

**Note how §1.1's withdrawal makes this *worse*, not better.** The doc fix is unnecessary
because the quickstart already configures the allowlist correctly. What remains is the
part that has no workaround: whenever a write *is* rejected — a table outside the
allowlist, a constraint violation, a tenant-policy denial — the developer is told nothing.
The doc line would only ever have papered over one instance of a general hole.

**The fix:** surface write failure in Dart. Minimum viable: add `pendingWrites` +
`lastWriteError` to `SyncStatus` (recommendation **A2**). This closes a documented
"Phase-2 concern" that, left open, means **no Nostos app can ever tell its user a write was
lost.**

### 1.2 `make sdk-e2e` is an honest harness — CONFIRMED

This repo's documented failure mode is env-gated tests that false-pass (`NOSTOS_E2E_PG`).
`scripts/sdk-e2e.sh` does **not** have that defect:

- `skip_slice()` (line 53) prints a yellow `SKIP` **with a reason**, and skips are
  counted in a **separate** column: `%d passed, %d failed, %d skipped / %d slices` (line 158).
- `exit "$fail"` (line 160) — a failing slice **does** fail the command.

So its output can be trusted at face value. What it cannot do is prove anything about
slices that skipped.

**Count reconciliation:** `ALL_SLICES` (line 26) has **10** entries —
`rust node tauri web capacitor dotnet flutter swift kotlin reactnative` — i.e. the
`rust` spine + the 9 dirs in `sdk/`. The "7/7" claim is simply the superseded
2026-07-12 number, later raised by the final-three plan. **Not a contradiction.**

**The load-bearing caveat:** 6 of 10 slices are gated on local toolchains/devices —
`dotnet` (needs dotnet SDK), `flutter` (needs flutter on PATH), `swift` (needs Xcode
simulator), `kotlin` + `reactnative` (need a booted Android emulator). On a machine
without emulators the honest output is *"4 passed, 0 failed, 6 skipped"* — which is
green, and which is **not** evidence of D2 breadth-completeness. The "10/10 verified"
claim therefore rests on a single 2026-07-12 session that had those devices booted,
attested by the same session that wrote the claim. → live re-run result in Part 2.

### 1.3 The uncommitted working tree is finished work, not a half-migration

9 modified files (`fixtures/flutter/todo/**`, `.gitignore`; 461 insertions / 61 deletions):

- `flutter analyze` → **"No issues found!"**
- `make fixture-todo-test` → **11/11 pass**

Content: a CRUD migration adding `repo.remove()` through the repository/viewmodel/view
layers, plus live integration scenario **(e)** asserting a delete propagates both to
local `watch()` and out to Postgres (collapsed-apply delete-back, ADR-0013).

**Verdict: commit it.** This is complete, green, launch-relevant work sitting
uncommitted — it is the visible proof of the delete path.
**Small gap:** the unit suite covers `add`/`toggle` against mocked ports but has no
`remove` test; delete is only covered by the live integration test, which requires a
running server. A mocked-port `remove` unit test is a ≤10-line addition.

---

## Part 2 — Live verification run (2026-07-29)

### 2.1 `make ci` — GREEN

`EXIT=0`. No failures.

Scope caveat, stated fairly: the real-Postgres e2e files self-skip inside a bare
`make ci` without `NOSTOS_E2E_PG=1`, so a green **local** `make ci` is not evidence that
real-Postgres replication works. **This is a local-only gap, not a project gap** —
`.github/workflows/ci.yml:35,68-69` runs a dedicated *"real-Postgres logical-replication
e2e"* job with `NOSTOS_E2E_PG=1` against a docker Postgres. GitHub CI covers it properly.

### 2.1b The CI gap that lets SDK breakage through unseen

| Job | Runs | Would it catch §2.3? |
|---|---|---|
| `fmt + clippy + test` (`ci.yml:15`) | `cargo test --workspace` | No — Rust only |
| `real-Postgres e2e` (`ci.yml:35`) | `NOSTOS_E2E_PG=1` + docker | No — engine only |
| `nostos_flutter — analyze + test` (`ci.yml:86`) | `flutter analyze`, `flutter test` | **No** — never runs `integration_test/` (needs a device) |
| **`make sdk-e2e`** | — | **Not run by any workflow** (grep: 0 hits in `.github/`) |

So the *only* live cross-SDK proof in the repo is a manual local ritual, and
`.github/workflows/` has not been touched since 2026-07-12 — before the SDK-breadth and
oplog work landed. That is the structural reason a launch-gating regression sat
undetected in the newest commit.

### 2.2 `make sdk-e2e` — 6 PASS / **2 FAIL** / 2 SKIP — **the headline finding**

Exit code 2. Run on this machine, today:

| Slice | Result | Detail |
|---|---|---|
| rust | PASS 33s | `ECHO_OK` |
| node | PASS 7s | `PUSH_OK` + `ECHO_OK` |
| tauri | PASS 8s | ok |
| web | PASS 2s | ok |
| capacitor | PASS 3s | `PUSH_OK` + `ECHO_OK` |
| dotnet | PASS 9s | `PUSH_OK=1 ECHO_OK=1` |
| **flutter** | **FAIL 1s** | two independent faults — see §2.3 |
| **swift** | **FAIL 11s** | environment + harness guard bug — see §2.4 |
| kotlin | SKIP | no booted Android emulator |
| reactnative | SKIP | no booted Android emulator |

**This falsifies, as of today, the "10/10 COMPLETE (2026-07-12)" and "7/7 SDKs
LIVE-replication-E2E-verified" claims.** Those claims were true when written; they are
not true now, and nothing in the repo re-checked them. Six SDKs genuinely pass a live
PUSH+ECHO round-trip — that part is real and impressive. Four are unproven today.

### 2.3 The Flutter slice: TWO independent faults, neither a sync defect

I initially reported this as a regression caused by the newest commit. **I tested that
hypothesis and it is false.** Both trees were run; both fail, for *different* reasons:

| Tree | Failure |
|---|---|
| **HEAD** | `Failed to load …/integration_test/nostos_server_test.dart: No macOS desktop project configured.` |
| **`bcac59b`** (pre-`9322d83`, `example/` + old script restored) | `An error occurred when adding Swift Package Manager integration: Xcode failed to resolve Swift Package Manager dependencies` |

*Experiment:* `git checkout bcac59b -- sdk/nostos_flutter/example scripts/sdk-e2e.sh`,
ran the flutter slice, then restored the tree exactly (verified: `git status` identical
to baseline; the uncommitted fixture work was never touched).

**Fault 1 — structural, real, HEAD-only.** Commit `9322d83` *("remove redundant example
app")* deleted `sdk/nostos_flutter/example/**` and repointed the slice at the SDK's own
`integration_test/`. The example app was **not** redundant: a Flutter *plugin* package is
not runnable, and `example/` is by convention the host app that
`flutter test integration_test/ -d <device>` launches. `sdk/nostos_flutter/macos/` holds
only the SwiftPM plugin target and a podspec. So at HEAD there is **no host app and the
test cannot execute at all** — on any machine. Secondary cost: pub.dev scores packages
partly on having an `example/`.

**Fault 2 — environmental, machine-local, pre-existing.** With the host app restored, the
slice still fails on Xcode/SPM dependency resolution. This is *not* caused by any Nostos
commit; it matches a toolchain problem already recorded on this machine on 2026-07-20
(Xcode/SPM/SDK mismatch), which likewise blocked the fixture integration test then.

**Corrected severity.** Fault 1 is real and worth fixing (A1) — a plugin with an
integration test and no host app is a genuine packaging defect, and it means the flagship
SDK's live proof cannot run *anywhere*. But **the claim "the newest commit broke the
flutter verification" is withdrawn**: this machine could not have run the flutter slice
green either way. Consequently **no conclusion should be drawn about whether the Flutter
SDK's sync actually works today** — it is unproven, not broken. Establishing that needs
either a machine with a working Xcode/SPM toolchain or a device-free test path.

**Fix:** (a) restore a minimal `example/` host app — scaffolding, not redesign; (b)
resolve the Xcode/SPM fault (cheap first attempt: `flutter clean && flutter pub get`, or
disable SPM per the Flutter docs the error links); then re-run `make sdk-e2e flutter`.

### 2.4 Swift slice: environment + a guard bug worth a 2-line fix

```
[harness] 6/7 simctl install
An error was encountered (domain=com.apple.CoreSimulator.SimError, code=405):
Unable to lookup in current state: Shutdown
```

The build succeeded end-to-end (xcodegen → xcodebuild → app built); only `simctl install`
failed because the simulator exists but **is not booted**. This is an environment
condition, not an SDK defect.

But it exposes a harness inconsistency: the Android slices guard on a **booted**
emulator and correctly `SKIP`, whereas the swift guard (`scripts/sdk-e2e.sh:105-108`)
only checks that Xcode/a simulator *exists*, so it FAILs where it should SKIP. That
turns "your simulator isn't running" into a red build. **Fix: make the swift guard
check for a booted simulator, matching the Android guard.** Then this slice reports
SKIP honestly and `make sdk-e2e` goes green-with-skips on a machine without devices.

> **Process lesson.** `make sdk-e2e` is a good, honest harness (§1.2) that nobody ran
> after the last three commits. The "verified" claims in the plans are all attested by
> the same session that produced the work. Re-running the harness took ~90 seconds and
> found a launch-gating regression. **This should run in CI, or at minimum be a
> pre-commit ritual for any `sdk/` change.**

---

## Part 3 — SDK inventory (all 9)

Scored on **live-E2E evidence from today's run** (§2.2) rather than source inspection —
a passing PUSH+ECHO round-trip against a real server is far stronger evidence than
grepping for `todo!()`. Vocabulary: **VERIFIED** = live round-trip passed today;
**REGRESSED** = previously passing, broken now; **UNPROVEN** = not exercised today.

| SDK | Status | Evidence |
|---|---|---|
| `nostos_node` | **VERIFIED** | `PUSH_OK` + `ECHO_OK`, 7s |
| `nostos_tauri` | **VERIFIED** | 2 test suites ok, 8s |
| `nostos_web` | **VERIFIED** | Playwright green, 2s |
| `nostos_capacitor` | **VERIFIED** | `PUSH_OK` + `ECHO_OK`, 3s |
| `nostos_dotnet` | **VERIFIED** | `PUSH_OK=1 ECHO_OK=1` via UniFFI-CS, 9s |
| *(rust spine)* | **VERIFIED** | `ECHO_OK`, 33s — `nostos-client` e2e |
| **`nostos_flutter`** | **UNPROVEN (2 faults)** | No host app since `9322d83` **and** a machine-local Xcode/SPM fault (§2.3). **Flagship, launch-gating.** Not shown to be broken — shown to be unverifiable here |
| `nostos_swift` | **UNPROVEN** | Builds + installs; sim not booted. Harness guard bug (§2.4) |
| `nostos_kotlin` | **UNPROVEN** | Honest SKIP — no booted Android emulator |
| `nostos_react_native` | **UNPROVEN** | Honest SKIP — no booted Android emulator |

**Read this correctly:** five non-flagship SDKs plus the Rust spine demonstrably perform
a real replication round-trip today. That is a genuinely strong breadth result and the
most under-sold fact in the repo. The problem is narrow and specific — **the one SDK the
entire launch wedge depends on is the one that is broken**, and three others are unproven
for want of a booted device.

*Not assessed this session:* per-SDK README/quickstart drift, publishability metadata,
and stub-vs-real API surface detail. Three subagents were dispatched for exactly this
and returned nothing (see Part 8). Their absence does not change the ranking in Part 7,
because the live-E2E result dominates the source-level questions they were asked.

---

## Part 4 — Docs drift & plan inventory

Partial — the docs-reconciliation subagent returned nothing (Part 8). Recorded here is
only what the orchestrator verified directly.

**Confirmed drift:**

| Claim | Where | Reality |
|---|---|---|
| "all 7 shipped SDKs LIVE-replication-E2E-verified" | `powersync-sdk-parity-plan.md` | **Stale.** 6/10 pass, 2 fail, 2 skip today (§2.2) |
| "Outcome — COMPLETE (10/10, 2026-07-12)" | `sdk-parity-final-three.md` | **Stale**, same reason |
| "#1 ranked: add `NOSTOS_WRITE_TABLES` to QUICKSTART, ~2 lines" | `nostos-next-after-oplog-epoch-2026-07-20.md:35` | **The plan itself is stale** — QUICKSTART wires writes via `nostos init --write-tables` (`:42,:252`). Action already unnecessary when written; I re-derived it by trusting the plan (§1.1) |
| "writes silently no-op" (premise for the above) | same, line 25 | **False as stated** — server rejects loudly (`transport.rs:786`), quickstart configures the allowlist. The *real* hole is the client swallowing `WriteResult{ok:false}` (§1.1, §6.1) |
| Moat numbers | `CLAUDE.md` vs `RESULTS.md` | Known drift, carried from 2026-07-20; docs-only fix (A7) |

**The structural problem.** `docs/plans/` holds 22 documents, several asserting
completion of overlapping scopes on the same date, with no supersession markers. A
reader cannot determine current state from them — which is why this assessment exists
and why **A8 (mark superseded plans)** is on the list. The plans' *designs* remain
valuable; only their status claims are unreliable.

---

## Part 5 — Test coverage reality

Partial — the test-reality subagent returned nothing (Part 8). Verified directly:

- **`make ci`: 431 passed, 1 ignored, 44 test binaries, exit 0.** Identical to the
  2026-07-20 baseline → **no regression in the Rust suite**.
- Only **2** `#[ignore]` attributes repo-wide — very low suppression.
- **10 real-Postgres-gated test files** self-skip without `NOSTOS_E2E_PG=1`:
  `e2e_pg_replication`, `e2e_pg_snapshot`, `e2e_pg_oplog_replay`,
  `e2e_pg_slot_invalidation`, `e2e_pg_schema`, `e2e_pg_typed_payload`,
  `e2e_pg_writeback_timestamp`, `e2e_pg_sync`, `e2e_client_reconnect_replay`,
  `e2e_pg_cli`. **Mitigated in GitHub CI** (`ci.yml:35,68` runs them with the flag), so
  this is a local-run caveat, not an unguarded hole.
- **The genuine coverage hole is cross-SDK, not Rust:** no workflow runs `make sdk-e2e`
  (§2.1b), and CI's flutter job never runs `integration_test/`. The Rust engine is
  well-tested; the *SDK layer* is tested only by a manual ritual.
- Fixture: `make fixture-todo-test` 11/11 green, `flutter analyze` clean — but no
  mocked-port `remove` unit test despite `remove()` being newly added (§1.3, → A5).

---

## Part 6 — DX / UX: what to change and when

Tagged **pre-launch** only where the defect can *silently lose or drop user data*.
Everything else is post-launch parity — otherwise "best practice" becomes the thing that
delays the launch.

### 6.1 The sharpest finding: `SyncStatus` is 2 fields where the competitor has 12

Source: PowerSync Flutter API reference, `SyncStatus` class
(<https://pub.dev/documentation/powersync/latest/powersync/SyncStatus-class.html>,
fetched 2026-07-29). Compared against
`sdk/nostos_flutter/lib/src/nostos_database.dart:498-515`.

| Concern | PowerSync | Nostos | Gap |
|---|---|---|---|
| Connection | `connected`, `connecting` | `conn` (enum) | ✅ equivalent |
| Activity | `downloading`, `uploading` | — | ❌ cannot distinguish "connected" from "actively syncing" |
| **Errors** | `uploadError`, `downloadError`, `anyError` | **—** | ❌ **the data-loss-class gap** |
| First sync | `hasSynced` | — | ❌ Nostos's own comment calls `lastSyncedAt` a *"best-effort proxy … until the engine exposes a download-completed signal (P1)"* (`nostos_database.dart:505-506`) — `hasSynced` **is** that signal |
| Progress | `downloadProgress` (ops done / total) | — | ❌ no progress UI possible |
| Last sync | `lastSyncedAt` | `lastSyncedAt` | ✅ |

**Why this is the #1 DX item (recommendation A2).** Combined with §1.1 — the client
swallows `WriteResult{ok:false}` and the Dart write returns *"the local outbox id (NOT a
server ack)"* — Nostos today gives a Flutter developer **no mechanism whatsoever** to
learn that a write failed or was dead-lettered. An app built on Nostos cannot show its
user "your change didn't save." That is a correctness-of-product gap, not polish, and
it is the one DX item that belongs before launch.

**The fix has an exact, proven target shape:** copy the PowerSync field set. Minimum
viable pre-launch subset — `uploading`, `hasSynced`, `lastWriteError` (or `uploadError`),
`pendingWrites` count. The rest (`downloadProgress`, `priorityStatusEntries`,
`syncStreams`) is post-launch.

### 6.2 Where Nostos's API is genuinely *better* — preserve these

Not everything should converge on PowerSync. Two Nostos choices are real advantages and
must not be "fixed" into parity:

- **Zero-backend writes.** PowerSync requires the developer to implement a
  `BackendConnector.uploadData()` and their own backend write endpoint + RLS. Nostos
  collapses writes server-side (ADR-0013), so the developer writes *no backend code at
  all*. This is the strongest DX differentiator in the product and the launch messaging
  should lead with it.
- **No client schema artifact for typing.** Server-side OID-keyed type mapping in
  `PgReplicator` (F5 decision, 2026-07-12) means types come off the wire correctly
  without the developer maintaining a duplicate schema declaration.

The `NOSTOS_WRITE_TABLES` allowlist is the *cost* of the zero-backend model — it's the
server-side trust boundary that replaces PowerSync's RLS. That trade is defensible;
it just has to be **taught** at the point of first contact (recommendation A3), which
is precisely what the missing QUICKSTART line was for.

### 6.3 Ranked DX changes

| # | Change | Class | Rationale |
|---|---|---|---|
| **D1** | `SyncStatus` gains `uploading`, `hasSynced`, `lastWriteError`, `pendingWrites` | **pre-launch** | Silent data loss; §6.1 |
| **D2** | `NOSTOS_WRITE_TABLES` taught in QUICKSTART + README | **pre-launch** | First-contact footgun; §1.1 |
| D3 | Surface dead-lettered writes (list + retry/discard) | post-launch | Needs UX design; the count in D1 covers the urgent part |
| D4 | `downloadProgress` for first-sync progress UI | post-launch | Parity polish |
| D5 | Throttle/`triggerOnTables` on watch queries | post-launch | Already logged as a refinement gap in the parity plan |
| D6 | Attachments, ORM integrations, SQLCipher encryption | post-launch | Full-catalogue parity; explicitly not a launch gate |

### 6.4 Competitor + UX research (gap C16 — now CLOSED)

Completed 2026-07-29 via the sandbox fetch path (`WebSearch` is broken in this session —
see Part 8). Partly recovered from the killed subagents' indexed sources.

#### The decisive finding: Flutter's *official* optimistic-state pattern requires what Nostos doesn't provide

Flutter's own architecture docs specify an "Optimistic state pattern" in which the
ViewModel holds **both** the optimistic value and an explicit error flag, reverting on
failure:

```dart
bool subscribed = false;
bool error = false;          // ← "Whether the subscription action has failed"

subscribed = true;           // optimistic
try   { await repository.subscribe(); }
catch (e) { subscribed = false; error = true; }   // revert + surface
```

**A Nostos developer cannot write this code.** `nostos.write()` returns a local outbox id
and never throws or reports rejection, so there is no `catch` to revert in. Nostos's SDK
makes the framework-blessed pattern *unimplementable*. This is the strongest available
argument for **A2**, and it comes from the platform vendor, not from a competitor.

#### Convergent conventions (what 4+ of them do the same way)

| Convention | Who | Implication for Nostos |
|---|---|---|
| Optimistic write + **rollback on error** | TanStack DB (*"If the handler throws, the optimistic state is rolled back"*), Flutter official, Electric, Instant | Nostos applies optimistically but **never rolls back or reports** → A2 |
| Error is a **first-class field** in the read/status surface | Instant (`{isLoading, error, data}`), PowerSync (`uploadError`/`downloadError`/`anyError`), TanStack | Nostos `SyncStatus` has no error field → A2 |
| Collapse all network failure into **one "offline" state** | Superhuman (*"we treat every kind of network failure as offline"* — fewer states, fewer code paths, coherent messaging) | Argues for a small `SyncStatus` enum, not a large one — do **not** over-model |
| A persistent, low-key **sync indicator** confirming "up to date" | Notion (*"the sync status indicator confirms everything is up to date"*), Superhuman's offline bar | `hasSynced` + `uploading` are the fields that make this possible |
| SDK owns retry/backoff; app owns presentation | PowerSync (blocking FIFO upload queue, SDK handles retries) | Nostos already does this correctly — keep it |

#### Competitive positioning — two findings that favour Nostos

1. **Zero (Rocicorp) explicitly does not support offline writes.** From their Connection
   Status docs: *"the cost to support offline is extremely high… there is simply more
   value in making the online experience great first."* The most-hyped new sync engine
   has **conceded the offline-write use case**. Nostos does it.
2. **ElectricSQL is a read-path sync engine** (their own meta description: *"the
   read-path sync engine for Postgres"*); writes are the developer's problem. Their
   writes guide even names Nostos's exact hazard: with through-the-database sync *"you can
   detect a write being rejected by the server whilst in context… with through-the-database
   sync, this context is harder to reconstruct."* Nostos **is** through-the-database sync —
   so Electric has documented the trap Nostos is currently in. A2 is the escape.
3. **PowerSync requires a `BackendConnector`** — the developer implements
   `fetchCredentials()` + `uploadData()`, loops `getNextCrudTransaction()`, and POSTs to
   their own backend API. Nostos requires **none** of this. Confirms the zero-backend
   claim in §6.2 is real and large; it should lead the launch messaging.

**Net positioning:** *offline writes with no backend code* is a genuinely defensible
wedge — Zero won't do offline, Electric won't do writes, PowerSync makes you build the
backend. Nostos's remaining gap is not capability, it is **telling the developer what
happened to their write.**

---

## Part 7 — The two columns

The single most useful reframe: most of what the docs carry as "remaining work" is
either **already done** or **not mine to do**. Splitting it makes the real path short.

### Column A — Claude-doable, ranked by (launch risk removed / effort)

Ordered so the launch-gating and data-loss items come first. Est. is engineering time,
not wall-clock.

| # | Action | Why now | Est. | Class |
|---|---|---|---|---|
| **A1** | Restore a minimal `sdk/nostos_flutter/example/` host app **and** clear the Xcode/SPM fault; re-run `make sdk-e2e flutter` | **Launch-gating.** Flagship SDK has no live proof that runs — two independent faults (§2.3). Also restores pub.dev example score | S–M | pre-launch |
| **A2** | Surface write failure in Dart: add `pendingWrites` + `lastWriteError` to `SyncStatus` | **The strongest finding in this document.** No Nostos app can tell its user a write was lost (§1.1, §6.1) | M | pre-launch |
| ~~A3~~ | ~~Add `NOSTOS_WRITE_TABLES` to QUICKSTART~~ — **WITHDRAWN** | Already wired via `nostos init --write-tables` (`QUICKSTART.md:42,252`). Optional: a README consistency pass | — | not a gate |
| **A4** | Fix the swift guard in `scripts/sdk-e2e.sh` to require a **booted** simulator (match the Android guard) | Turns a false red into an honest SKIP (§2.4); makes the harness trustworthy on any machine | XS | pre-launch |
| **A5** | Commit the 9 finished Dart files; add a mocked-port `remove` unit test | Green, analyzed, launch-relevant work sitting uncommitted (§1.3) | XS | pre-launch |
| **A6** | Run `make sdk-e2e` in CI (host slices at minimum: rust/node/tauri/web/capacitor/dotnet) | The structural fix for §2.1b — the only cross-SDK proof is a manual ritual nobody ran | S | pre-launch |
| **A7** | Reconcile the moat numbers across `CLAUDE.md` / `ROADMAP.md` / `RESULTS.md` | Known drift; docs-only, no re-benching needed | XS | pre-launch |
| **A8** | Mark superseded plans as superseded (header line pointing here) | 22 mutually-contradicting plans is the root cause of "is it done?" being unanswerable | S | pre-launch |
| **A9** | Boot an Android emulator + iOS sim once; run the full `make sdk-e2e` to close kotlin/swift/reactnative | Converts 4 unproven slices into evidence; needs only a booted device, no code | S | post-launch OK |

**A1, A2, A4, A5 are the real pre-launch set (A3 withdrawn). That is roughly a day of
work, not a phase** — with the caveat that A1's second half (Xcode/SPM) is a toolchain
unknown that could be 10 minutes or half a day.

### Column B — Operator-only (not engineering; do not carry these as eng debt)

| Item | Why it cannot be delegated |
|---|---|
| The **≤5-min stranger test** | Requires a fresh human on a fresh machine with a stopwatch. By construction not runnable by the author or by an agent. This is *the* launch gate |
| Publish: repo push, pub.dev, brew tap, GitHub release | Credentials + irreversible public action |
| Show HN timing + launch-post publication | Business judgement (`docs/launch/` drafts are ready) |
| Nostos Cloud alpha; Supabase partnership outreach | Commercial |
| Ratify the GATED-ON-GO plans (powersync redesign, AI-privacy roadmap, reactive-facade extensions) | Strategy calls, explicitly deferred pending operator go |

### What this means for "complete"

- **D2/D3 (breadth/parity): not complete, and should stop being scored as launch gates.**
  6/10 SDK slices prove a live round-trip today; parity with PowerSync's full catalogue
  (attachments, ORM integrations, encryption, sync-streams) is a post-launch roadmap,
  as `powersync-sdk-parity-plan.md` itself concludes.
- **D1 (wedge): reachable, and closer than the docs suggest.** The engine is sound —
  audit P0s #1/#2 closed, F1 closed, offline-delete-orphan closed with tail coverage
  (§1.3, `apply.rs:796`), `make ci` green, 6 SDKs live-verified. After **A1–A5**, the
  honest status is *"engineering-complete for the Flutter+Supabase wedge; blocked only
  on the operator-run stranger test and publish."*

That sentence is the answer to "what stage is the project at."

---

## Part 8 — Claim list (Gate 4)

| # | Claim | Status | Evidence |
|---|---|---|---|
| C1 | `make ci` green: 431 passed, 1 ignored, exit 0 | **verified** | Run this session; log at `scratchpad/make-ci.log` |
| C2 | `make sdk-e2e` = 6 pass / 2 fail / 2 skip, exit 2 | **verified** | Run this session; full per-slice table §2.2 |
| C3 | At HEAD the flutter slice fails: "No macOS desktop project configured" | **verified** | `/tmp/sdk-e2e-flutter.log` |
| C4 | `9322d83` deleted `sdk/nostos_flutter/example/`, leaving a plugin with an integration test and no host app | **verified** | `git show --stat 9322d83`; `ls sdk/nostos_flutter` → no `example/`; `macos/` holds only plugin code |
| C5 | ~~Restoring `example/` makes the flutter slice pass~~ | **verified FALSE** | **Tested.** Restored `example/` + old script from `bcac59b`, ran the slice: still FAILS, on `Xcode failed to resolve Swift Package Manager dependencies`. My regression attribution was wrong and is withdrawn (§2.3). Tree restored exactly; `git status` matches baseline |
| C5b | The flutter failure on this machine is environmental (Xcode/SPM), pre-dating `9322d83` | **verified** | Pre-regression tree fails on SPM resolution; matches a toolchain fault recorded on this machine 2026-07-20 |
| C5c | Whether `nostos_flutter` sync actually works today | **unknown** | Cannot be established on this machine — needs a working Xcode/SPM toolchain or a device-free test path |
| C6 | Swift slice fails only because the sim is not booted | **verified** | Log shows xcodegen→xcodebuild→app built; failure is `simctl install … state: Shutdown` |
| C7 | Swift slice would PASS with a booted sim | ~~assumed~~ → **verified, but the reason was wrong** | It PASSES (2026-07-30) — yet booting a sim was *not sufficient*: `build.sh` targeted a hardcoded UDID, so the slice only ever ran on one machine's one device. C6's diagnosis was right about the symptom and incomplete about the cause |
| C8 | ~~kotlin / reactnative SDK health~~ | **verified FALSE as stated** | The blocker claim "no Android emulator on this machine" was simply untrue — five AVDs exist (`Medium_Phone_API_36`, `Pixel_9`, `nostos_api34`, `pack-9`, `probe_arm64`). That unchecked assumption is the only thing that kept A9 open for 17 days. Both slices PASS (2026-07-30); the kotlin failure was a harness logcat race, not SDK health |
| C9 | The env-var *name* `NOSTOS_WRITE_TABLES` has 0 hits in QUICKSTART/README, 9 in OPERATING | **verified** | Per-file `grep -c` |
| C9b | ~~Therefore a stranger's writes silently fail~~ | **verified FALSE** | QUICKSTART wires it via `nostos init --write-tables todos` (`QUICKSTART.md:42`), explained at `:252`; parsed `init.rs:64-68`, persisted `config.rs:47,157`, emitted to deploy templates `deploy.rs:56,106`. Claim withdrawn; A3 dropped |
| C10 | The Dart SDK gives a developer no way to learn a write failed | **verified** | `SyncStatus` = `{conn,lastSyncedAt}` (`nostos_database.dart:498-515`); `client.rs:32` "user-facing surface is a Phase-2 concern"; `client.rs:710,718` retry→dead-letter; write returns "local outbox id (NOT a server ack)" (`nostos_database.dart:438`) |
| C11 | PowerSync `SyncStatus` exposes 12 members incl. `uploadError` | **verified** | pub.dev API reference, fetched this session |
| C12 | Uncommitted Dart work is complete + green | **verified** | `flutter analyze` clean; `make fixture-todo-test` 11/11 |
| C13 | Offline-delete-orphan P0 is closed with tail coverage | **verified** | `apply.rs:597,652,796`; `sqlite.rs:1663` |
| C14 | No workflow runs `make sdk-e2e`; CI flutter job skips `integration_test` | **verified** | `grep -rn sdk-e2e .github/` → 0 hits; `ci.yml:86,117,129` |
| C15 | Per-SDK README drift, publishability, stub-vs-real detail | **unknown** | 3 subagents dispatched for this returned nothing; not independently done |
| C16 | Competitor DX beyond PowerSync `SyncStatus`; sync-UX conventions | **verified — CLOSED** | §6.4. Fetched 2026-07-29 via sandbox path: PowerSync SDK ref, Zero connection-status + mutators, Instant docs, TanStack DB overview, Electric writes guide, Flutter official optimistic-state pattern, Superhuman, Notion, Ink & Switch |
| C16b | Flutter's official optimistic-state pattern requires a catchable write failure, which Nostos cannot provide | **verified** | Flutter architecture docs (`subscribed`/`error` revert pattern) vs `nostos_database.dart:438` returning an outbox id that never reports rejection |
| C16c | Zero does not support offline writes; Electric is read-path only | **verified** | Zero Connection Status docs (*"the cost to support offline is extremely high"*); Electric docs meta (*"the read-path sync engine for Postgres"*) |
| C17 | A1–A5 are sufficient to pass the ≤5-min stranger test | **unknown** | The stranger test is by construction operator-run; no agent can establish this |

**Honest reading of this table.** Two of my own claims were falsified by testing them
(C5, C9b) and are recorded as withdrawn rather than deleted — both were inherited from a
stale plan document, and both would have sent work in the wrong direction. The surviving
headline rests on C1, C2, C10, C11, C14, all verified by commands run this session.

The strongest *surviving* finding is **C10 + C11 + C16b** (no write-error surface in Dart,
2 fields vs PowerSync's 12, and Flutter's own official optimistic-state pattern rendered
unimplementable as a result). It is unaffected by either correction, was derived from
source plus authoritative external references rather than from any plan doc, and is the
one item that is a product defect rather than a harness or docs issue.

The most important **unknown is C5c**: whether the flagship Flutter SDK's sync actually
works today is *not established*, and cannot be on this machine. Everything in Part 7 is
ranked on the assumption that it does — if the Xcode/SPM fault is masking a real Flutter
sync defect, A1 is larger than "S–M" and the wedge is further from done than this document
says. **That is the single load-bearing uncertainty in the assessment.**

### Method note: the fan-out returned nothing

Eight subagents were dispatched in parallel; **zero returned a report.** The two research
agents were blocked by a harness error (`output_config.effort 'xhigh' is not supported
when thinking is disabled`) raised by `WebSearch` — the same error hit the orchestrator,
so it is environmental, not a prompt defect. It is caused by the session running at
`/effort xhigh` while `WebSearch`'s internal call has thinking disabled; **`/effort high`
or lower clears it.** All eight agents were stopped explicitly rather than left running.

**They were not, however, useless.** The research agents indexed a dozen sources into the
shared knowledge base before dying — PowerSync's SDK reference and client architecture,
Zero's connection-status and mutators docs, Instant, TanStack DB, Electric's writes guide,
Flutter's official optimistic-state pattern, Superhuman, Notion, Ink & Switch, Automerge.
**§6.4 was largely reconstructed by querying what they left behind**, which is why C16
closed after all. The failure was in the *reporting* channel, not the work.

**Do not over-read this as "fan-out doesn't work"** — the decomposition was sound, and an
environmental failure prevented returns. The transferable lesson is narrower and was
decisive here: **running the project's own harness beat reasoning about its code.**
`make ci` + `make sdk-e2e` took ~90 seconds and produced the entire headline, and the one
experiment that actually *tested* a hypothesis (restoring `example/`) is what caught my
own wrong root-cause.

---

## Addendum — 2026-07-30: A11 packaging + the README-drift audit (C15 closed)

The operator asked for the residues of the previous addendum to be fixed, which closed A11
and the last open piece of C15. **Five real defects surfaced, none of which any test would
have caught**, plus two of my own errors.

### A11 — packaging pass: DONE

| what | before | after |
|---|---|---|
| versions | 8 packages at `0.0.0` | all `0.1.0` (matching `[workspace.package]` and `nostos_flutter`) |
| `LICENSE` file | 1 of 9 | **9 of 9** |
| `repository` / `homepage` | 2 of 9 | 9 of 9 (npm also gets `bugs`) |
| READMEs | 5 of 9 | **9 of 9** (new: kotlin, swift, node, tauri) |

**One prediction I checked instead of trusting:** the concern was raised that a new `LICENSE`
would not ship in the npm tarball, since the `files` arrays list only `dist`/`README.md`.
`npm pack --dry-run` says otherwise — **npm force-includes `LICENSE`** regardless of `files`.
Verified, not assumed. Likewise the csproj needs **no** `PackageLicenseFile`: it already sets
`PackageLicenseExpression`, and NuGet rejects both together (NU5035).

### The five defects

1. **`sdk/nostos_tauri` never exposed `subscribe` to JS.** `generate_handler!` and `build.rs`
   listed only `connect`/`write`/`query`/`checkpoint`. Since `connect` does *no* network I/O
   and `subscribe` is what drives the run loop, **the entire download path was unreachable
   from a Tauri frontend** — connect, then wait forever. Fixed (handler + `build.rs` +
   `permissions/default.toml`, all three required or the ACL rejects the call at runtime).
   *Why no test caught it:* the tauri slice is `cargo test`, which calls `NostosState::subscribe`
   directly and never crosses the command boundary. Recorded in the new README.
2. **`dotnet/Nostos.DotNet.csproj` was not well-formed XML** — two fatal errors: a mismatched
   `<PackageProjectUrl>…</PackageUrl>` tag pair, and `--` inside an XML comment (forbidden by
   the XML spec; it came from pasting a `--library` CLI flag into a comment). Any
   `dotnet build`/`pack` of that project would fail. *Why no test caught it:* the E2E builds
   `dotnet/smoke/Smoke.csproj`, a different project — the packageable one is compiled by nothing.
3. **`@nostos-sync/react-native`'s own `npm run typecheck` failed** under its own strict tsconfig
   (`config.url` is `string | null | undefined`, the native Spec wants `string`). Fixed with a
   real guard that throws a message naming the fix, rather than a cast — a null URL should not
   reach the TurboModule boundary. *Why no test caught it:* `test` was `jest` only, so nothing
   ran `typecheck`. Now `test` = `typecheck && jest`, so it cannot regress.
4. **`sdk/nostos_web/README.md` understated a shipped capability** — it called the whole package
   a "reduced-scope feasibility proof" that "does NOT yet open a live WebSocket." The *browser*
   path (`pkg-web` → `NostosSocket`) opens a real WebSocket and is proven by
   `e2e/browser_live.spec.cjs`, which **is** the passing `web` slice. Only the Node facade is
   reduced-scope. Now documents both paths with a `NostosSocket` API table.
5. **`sdk/nostos_flutter/README.md` gave actively wrong advice** — "one active subscription per
   `Nostos` instance… use a second `Nostos.connect(...)`" for a second table. `subscribeTables`
   multiplexes many tables over **one** socket (ADR-0022). Following the README opened a
   redundant connection.

Also corrected: three stale "dotnet is not installed / E2E is SKIP-with-reason" claims in the
dotnet README and csproj. `dotnet` lives at `~/.dotnet/dotnet` — **not on `PATH`**, which is why
the original bare `which dotnet` check read as absent. The slice passes.

### Two of my own errors

- **I reported "working tree clean" while `README.md` had an uncommitted stray `/`** appended
  with no trailing newline. The claim was wrong; the file is fixed.
- **The C7/C8 claim rows were left stale** after A9 closed. C8's blocker — "no Android emulator
  on this machine" — was simply **false** (five AVDs exist), and that single unchecked assumption
  is what kept A9 open for 17 days. C7 was *right about the symptom and incomplete about the
  cause*: booting a simulator was necessary but **not sufficient**, because `build.sh` targeted a
  hardcoded UDID. Both rows now say so.

### The pattern across all of it

Every one of the five defects sits in a place **no test executes**: a command list, a project file
nothing builds, a script nothing runs, prose. The e2e suite proves the *runtime* path and is
silent on the *packaging and documentation* path — so "10/10 slices PASS" was never evidence about
either. That is the same shape as the harness bugs on 2026-07-29 (**a guard checking something
weaker than what the action requires**), one level out.

**Still genuinely open:** nothing in the engineering column. The SDKs are now *packaged* but
still **not published** — no npm/Maven/NuGet/pub coordinate exists. Do not let public copy drift
in the other direction: "10/10 parity" remains a **functional** claim, never a *distributable* one.
