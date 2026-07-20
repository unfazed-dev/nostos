# What's Next for Nostos — After Oplog Epoch (ADR-0025)

**Date:** 2026-07-20
**Status:** analysis/plan (plans-only scope — no implementation herein)
**Inputs:** three read-only subagent audits (blocker state, documented-next, playbook/ops) + primary docs

## TL;DR

ADR-0025 closed the two **code** P0s from the 2026-07-19 soundness audit (slot-invalidation, watch bug) and, via F1 (`a711df7`), the tenant-scoped DELETE replay hole. The engine is sound. **What remains is not more engine work — it is (a) the operator-doc blocker that fails the launch stranger-test cold, and (b) two unverified integrity claims (a latent delete-loss race + a stale moat number) that must be measured before declaring launch-ready.** Two independent critical paths, runnable in parallel.

## 1. What ADR-0025 actually closed (P0 tracker)

| Audit P0 | Status | Evidence |
|---|---|---|
| #1 Slot-invalidation silent data-loss | **RESOLVED** | `8cd67c0` — `SlotProbe` trichotomy at `crates/nostos-infra/src/replicator/pg.rs:82-96` (used `:565-576`); SQLSTATE 55000 handler `pg.rs:1223`; regression `crates/nostos-infra/tests/e2e_pg_slot_invalidation.rs:107` drops slot mid-stream, asserts recreate + epoch bump |
| #2 Watch bug (Dart + Rust) | **RESOLVED at code level (pending W5 empirical re-verify)** | Dart `5661083` — `_replayLatest`; Rust `nostos.rs:307-322` — `subscribe_changes()` before `emit_snapshot()`; PLUS the deeper root causes from the 2026-07-12 QUICKSTART finding are addressed: `feed()` time-bounded flush (`apply.rs:194-219`, `flush_quiesce` 50 ms) + `idle_timeout` now a `SyncClientConfig` knob (`client.rs:90-200`). **C9:** the QUICKSTART's `nostos.rs:145` citation is stale; the integration repro (`nostos_live_test.dart` = W5 stranger step) has not been re-run post-fix |
| #3 Playbook gap | **RESOLVED (drafted + spot-verified 2026-07-20)** | `docs/OPERATING.md` written — 377 lines, 7 sections (env + 5 startup-failure modes, slot lifecycle + manual recreate, "connected but lists empty" 5-line triage, CLI reference, make targets, docker, refs), every env var cited file:line. Subagent corrected two stale audit pointers against primary sources: `example/README.md:194-239` (slot recipe) does not exist; the audit's "`main.rs:437` bail" is actually a `warn!` at `main.rs:517-519` (confirmed — see C10) |
| (new) Tenant-DELETE replay (NULL tenant) | **CLOSED 2026-07-20** | `a711df7` (ADR-0025 F1) — `RowOp::Delete` carries `old_payload` via `REPLICA IDENTITY FULL`; `lift_tenant` in `crates/nostos-infra/src/oplog.rs`; real-PG e2e asserts `saw_delete=TRUE` in `tests/e2e_pg_oplog_replay.rs` |

**Correction to prior memory:** the tenant-DELETE gap was *not* "covered by slice-1 reconcile." Slice-1 reconcile (`crates/nostos-core/src/apply.rs:597`) covers the **snapshot path only**. F1 (`a711df7`) closed the **replay path**. The gap is genuinely closed now, not papered over.

## 2. What's actually blocking launch — two paths

### Path A — Launch-readiness (the ≤5-min stranger test)
The master plan gates launch on: *W0–W8 done + stranger test ≤5 min + operator publishes* (`flutter-supabase-plug-and-play-launch.md`, Sequencing). That stranger test **cannot pass cold today**: `NOSTOS_WRITE_TABLES` is absent from `docs/QUICKSTART.md` (0 grep hits), so writes silently no-op (server-gated allowlist, empty default — ADR-0013) and the stranger concludes "nostos is broken." This is the single biggest cold-demo failure (ops-audit finding #4).

### Path B — Soundness-integrity (moat + no silent loss)
Two claims are **unverified post-oplog** and must be measured, not assumed:
- **Late-append race on the replay path.** `crates/nostos-infra/src/oplog.rs:357` admits late appends "may be lost"; the stated mitigation (slice-1 reconcile) covers the **snapshot path only**. `e395dea` (graceful-shutdown drain) fixed the *detached-flush* case (the last ≤BATCH_MAX batch no longer dropped on SIGTERM), but a residual **append-after-drain-started** window remains — a matching-epoch reconnect with no snapshot trigger could miss a delete. No regression test covers it. Same "silent data-loss" category as P0 #1.
- **Moat re-verification (already done at the fan-out layer).** `benches/results/RESULTS.md` already records the post-oplog fan-out number with `NOSTOS_BENCH_OPLOG=1`: oplog is statistically invisible — **833,305 → 833,307 ops/sec @ 1000 clients, `oplog_dropped=0`** (channel-send cost, within ±5% noise). The genuinely OPEN measurement is the real-PG `cairn_oplog` multi-row INSERT write-amp (slice 6, off the fan-out loop) — needs docker PG + a real-PG bench harness, not `make bench`.

## 3. Ranked next actions

**Tier 0 — collapses the launch blocker (docs; stranger-test-gating)**
1. Add `NOSTOS_WRITE_TABLES=tasks` to the cold demo path in `docs/QUICKSTART.md` (and README demo paths A/B/C, `README.md:114-175`). ~2 lines.
2. Write `docs/OPERATING.md` per audit ask (`nostos-soundness-audit-2026-07-19.md:254-260`): server env vars + startup-failure modes (incl. the `NOSTOS_REPLICATOR != pg` bail at `crates/nostos-server/src/main.rs:437`), slot-recreate recipe, "connected but lists empty" 5-line diagnostic, CLI command reference.

**Tier 1 — collapses the two unknowns (verify before declaring done)**
3. Run `make ci`. Green status is **unknown to this session** (subagents were read-only).
4. Write a regression test for SIGTERM-during-oplog-batch + matching-epoch reconnect + delete. Either confirms the window is covered, or surfaces a real P1 → fix. Cheapest probe of the load-bearing unknown.
5. ~~Run `make bench`~~ — **already done.** `RESULTS.md` records 833k ops/sec @ 1k clients, 0% drops, oplog invisible (`NOSTOS_BENCH_OPLOG=1`). The open measurement is the real-PG `cairn_oplog` write-amp (slice 6, off-loop) — stand up docker PG + a slice-6 real-PG bench if/when that cost is claimed; it does not regress the fan-out moat.

**Tier 2 — harden evidence (audit/ADR both flagged, not blocking)**
6. Extend real-PG e2e: multi-tenant concurrent reconnect; oplog compaction-window-expiry; epoch-rollover-under-load. Current "2/2 green" covers only single-client happy paths.

**Tier 3 — launch-gate items from the master plan (independent of audit)**
7. W0b live-Supabase stranger run; F5 typed payloads (= WS2 slice-2: instant-local writes + reconcile, per `nostos-ws2-view-storage`).

**Operator-decision gates (surface only — not this plan's call):**
- `nostos-flutter-powersync-connection-redesign.md`, `dart-dev-api-reactive-facade-2026-07-19.md`, `nostos-ai-privacy-and-runner-roadmap.md` — all GATED-ON-GO.

## 4. Claim list (Gate 4)

| # | Claim | Status |
|---|---|---|
| C1 | P0 #1 slot-invalidation resolved (`8cd67c0` + regression test) | **assumed** — subagent-cited; not personally re-verified this session |
| C2 | P0 #2 watch bug resolved both layers (`5661083` + Rust swap) | **assumed** — subagent-cited |
| C3 | `docs/OPERATING.md` missing / P0 #3 open | **verified-resolved** — file now exists, 377 lines, 7 audit-required sections, env vars cited 29×, `main.rs:517-519` `warn!` spot-checked |
| C10 | `NOSTOS_PG_URL` set + `NOSTOS_REPLICATOR != pg` should "fail loudly" per its own comment | **verified-fixed** — promoted `warn!`→`anyhow::bail!` (`main.rs` C10, 2026-07-20); the server now REFUSES to start on the misconfiguration instead of degrading silently. `OPERATING.md §1.1(a)` rewritten for the new behavior. `make ci` green at 431 (no test sets PG_URL + non-pg replicator, so nothing broke) |
| C4 | Tenant-DELETE replay closed by F1 (`a711df7`) | **assumed** — subagent read code + commit msg; supersedes stale memory |
| C5 | `NOSTOS_WRITE_TABLES` absent from QUICKSTART | **verified** — grep returned 0 hits |
| C6 | Late-append race is a real uncovered delete-loss window | **verified-fixed (A+B applied 2026-07-20).** Race was confirmed real + production-reachable, then closed by two fixes: **A** (`main.rs:615`) retains the pg replicator `JoinHandle` + aborts it between axum drain and oplog shutdown (producer stops before consumer drains); **B** (`oplog.rs`) makes `tx: Mutex<Option<Sender>>` and `shutdown()` `.take()`s the sender first → the flush_loop's all-senders-dropped `None` path becomes authoritative (drains everything; no silent buffer-during-final-flush) + `append()` on `None` rejects loudly via `oplog_dropped`. The `#[ignore]`'d probe was un-ignored, renamed `drain_boundary_late_append_is_rejected_not_lost`, and its assertion flipped — it now PASSES (would fail under the old bug). `make ci` green at 431 passed / 0 failed / 0 warnings |
| C7 | Oplog writes regress the moat | **verified — no regression.** `benches/results/RESULTS.md` already documents the post-oplog fan-out measurement with `NOSTOS_BENCH_OPLOG=1`: 833,305 → 833,307 ops/sec @ 1000 clients, `oplog_dropped=0` (channel-send cost, within ±5% noise). The moat is **833k ops/sec @ 1k clients, 0% drops, 208.3× PowerSync** (RESULTS.md). The remaining OPEN measurement is the real-PG `cairn_oplog` multi-row INSERT write-amp (slice 6, off-loop — needs docker + `NOSTOS_E2E_PG=1` + a real-PG bench harness), not `make bench` |
| C11 | Moat number is consistent across docs | **verified-fixed** — 142k was stale eval-only drift (same measurement as 833k, pre-optimization; `ROADMAP.md:14` already labeled it the Week-1 historical baseline). `CLAUDE.md:6-7` + `docs/STRATEGY.md:16` updated to 833k/208× with an explicit eval-only-fan-out label; README/ROADMAP/RESULTS were already correct |
| C12 | W5 integration test loads on the author machine | **blocked — macOS toolchain failure, not a code regression.** `fixtures/flutter/todo` integration test fails at LOAD: `nostos_flutter.NostosFlutterPlugin` symbol missing for arm64 + `CoreAudioTypes`/`SwiftUICore` auto-link warnings (Xcode/SPM/SDK mismatch on this machine). The watch-bug fix is not exercisable until this is resolved. Cheap unblock to try: `flutter clean && flutter pub get` in `fixtures/flutter/todo/`, then re-run; else Xcode/SPM investigation. Operator-owned (env + the W5 gate is fresh-machine by definition) |
| C8 | `make ci` currently green | **verified** — real exit 0 (read from the log, not the wrapper); **431 passed / 0 failed / 1 ignored** (real-PG self-skip) / 0 clippy or fmt warnings, after fixes A+B + the flipped C6 guard |
| C9 | P0 #2 fully resolved (the QUICKSTART 2026-07-12 watch() launch-blocker) | **assumed — code-fixed, empirically unconfirmed.** Code-level root causes addressed (subscribe-before-emit, `flush_quiesce`, `idle_timeout` knob). Re-run attempt 2026-07-20: server stack came up green (docker PG + `nostos dev` + PgReplicator on real slot + write-back + HS256 + healthz ok), but the integration test **failed to LOAD** — blocked by C12 (a macOS linker error, unrelated to the watch fix). The W5 stranger gate (fresh machine/person) still stands by definition |

**The `unknown`/`assumed` claims (C6, C7, C8, C9) are load-bearing** — they determine whether Tier 1 is real correctness/moat work or just hygiene, and whether the watch-bug is truly closed. The cheapest next actions (test #4, bench #5, `make ci` #3, and re-running the W5 stranger step) exist specifically to collapse them.

## 5. Recommended sequencing

**Severity verdict on the late-append race (C6): P1, conditionally launch-gating — not acceptable as-is.** Reasoning: the slot P0 was prioritized #1 for the *silent data-loss* category; a missed **delete** is the worst op to lose (ghost row, unbounded staleness, no self-heal); and the recovery path for any loss window is a snapshot, which ADR-0025 F2 *disables* on matching-epoch reconnect. So the "narrow trigger" understates it — routine server SIGTERM during deploy + the new optimization removing the self-heal = a real design gap, unguarded by any test. The fix is cheap and test-first.

Two senses of "launch" split the ranking:
- **≤5-min stranger test (demo launch):** gated by Path A #1 (QUICKSTART) — a stranger never reaches sync-correctness if writes silently no-op.
- **Production / self-host readiness:** gated by the late-append test (C6) + the moat re-measure (C7).

Sequenced:
1. **#1 QUICKSTART `NOSTOS_WRITE_TABLES`** (~2 lines) — unblocks the literal stranger gate. Nearly free; do first.
2. **#4 late-append regression test** — P1; test-first either confirms the window closed or surfaces the fix. Bumped ahead of OPERATING.md because P1 + cheap.
3. **#3 `make ci`** + **#2 OPERATING.md** — parallel; ci verifies the current tree, OPERATING.md closes audit P0 #3 / self-host posture.
4. **#5 moat** — already verified at the fan-out layer (`RESULTS.md`: oplog invisible, 833k @ 1k, 0% drops); the real-PG `cairn_oplog` write-amp (slice 6) remains open and needs a real PG.
5. Then the stranger test itself; then Tier 2 e2e hardening.

Defer Tier 3 + gated-on-go plans (powersync redesign, reactive facade, AI roadmap) until Path A + B land, and never without operator sign-off.

> **Confidence note:** the verdict's cheapest resolution is the test in step 2 — run it first; it materially settles C6 regardless of severity opinion. The independent advisor call can be re-run before committing to a fix if the test confirms the race.

## 6. RESOLVED — late-append delete-loss P1 (A+B applied 2026-07-20)

The C6 probe confirmed the race real + production-reachable; the operator authorized both fixes; both shipped and verified.

- **Fix A — `main.rs:615` (production ordering):** the pg replicator `JoinHandle` is now retained (was `mem::forget`) and `.abort()`ed between axum's graceful drain and `OpLogWriter::shutdown()`. Producer stops before the consumer drains → no append can race into the final flush. Fake branch untouched (oplog is pg-only).
- **Fix B — `oplog.rs` (channel authority, closes it for ALL callers):** `tx: Mutex<Option<Sender>>`; `shutdown()` `.take()`s the sender FIRST, so the flush_loop's all-senders-dropped `None => break` path becomes authoritative (drains everything; `recv()` returns `None` only when the buffer is empty). Concurrent `append()` finds the sender gone → rejects loudly via `oplog_dropped`, never silently buffers. Mutex serializes append/shutdown; anything buffered pre-`take()` is drained.
- **Test flipped:** `drain_boundary_late_append_during_final_flush_is_lost` (`#[ignore]`'d, asserted `is_ok()` to demonstrate the bug) → `drain_boundary_late_append_is_rejected_not_lost` (non-ignored, asserts the late append is rejected). PASSES under the fix; would FAIL under a regression that re-clone's `tx` past shutdown.

**Verification:** `make ci` real exit 0; **431 passed / 0 failed / 1 ignored** (the real-PG self-skip) / 0 clippy or fmt warnings. Diffs reviewed at the mechanism level by the orchestrator (not just test-pass).

**ADR formalized:** `docs/adr/0026-oplog-shutdown-durability.md` records the A+B decision (defense-in-depth: A is per-caller operational ordering, B is the channel-enforced invariant for all callers; either alone leaves a residual ghost-row vector on ADR-0025 F2's matching-epoch snapshot-skip). Code citations updated — `main.rs` Fix A + `oplog.rs` Fix B now cite ADR-0026.
