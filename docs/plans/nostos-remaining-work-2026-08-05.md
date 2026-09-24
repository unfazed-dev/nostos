> **SUPERSEDED (2026-09-01).** This list froze on 2026-08-05; everything it named
> has since been executed, closed, or re-owned elsewhere. Live state: `docs/ROADMAP.md`
> (status footer), the plans index (`docs/plans/README.md`), and
> `docs/plans/nostos-integration-tauri-flutter-push.md` (studio integration, with its
> 2026-08-28/09 addenda). Kept as the historical record only.

# Nostos — Remaining Work (fable-z 8-subagent audit)

**Date:** 2026-08-05 · **HEAD:** `969ec36` · **Method:** fable-z classify-then-gate; 8 parallel
`general-purpose` subagents (one per dimension) + orchestrator re-verification of the 5
load-bearing defect claims. Reconciles and extends `docs/plans/nostos-status-audit-2026-08-04.md`.
**Claims are tagged:** ✅ verified (observed this audit) · 🟡 assumed · ❓ unknown.

> Read-the-damn-docs note: every status below is grounded in code/commit/test evidence cited
> inline, **not** in prior memory. Three of the 08-04 audit's own premises were falsified by this
> pass (§2) — they are stale vs `969ec36`, not wrong-when-written.

---

## 0. RECONCILED 2026-08-05 (later "close all the gaps" pass — HEAD `0a52b79` + working tree)

This audit was written at `969ec36`. A same-day follow-up pass **closed or
load-bearing-verified** every code-addressable item below. This section is the
delta; the body below is left intact as the historical record. Operator-only
items (cold-stranger stopwatch, W6 publication pipeline) remain the sole gate.

| Item (this doc) | Status now | Evidence |
|---|---|---|
| §3 / Tier-2 #7 — "no live Flutter example app" | **CLOSED** — example + `nostos_server_test.dart` restored (`e749c27`); wired as the **10th** sdk-e2e slice (macOS-gated), **PASS** through the harness | `scripts/sdk-e2e.sh`; `sdk/nostos_flutter/example/integration_test/` |
| §3.1 / §8 #2 — "JWKS/RS256 never hit real Supabase" | **CLOSED** — verified end-to-end vs the real project `ltamqsxxumtusyxswezi`, which is **ES256** (P-256), not RS256; fetch+parse AND signature-verify both pass | `0a52b79`; `crates/nostos-infra/tests/jwks_real_supabase.rs` |
| Tier-3 #8 — "real-PG write-amp never published" | **CLOSED** — measured `amp=1.000`, `oplog_dropped=0`; ADR-0025/0026 + RESULTS.md updated | `0a52b79`; `e2e_pg_write_amp.rs` |
| Tier-2 #4 — dotnet binding lacks signOut/setToken | **CLOSED** (`e749c27`; `nostos.cs:1323,1355`) **and now CI-gated** (dotnet slice added to the `sdk-e2e` CI job) | `.github/workflows/ci.yml` |
| Tier-2 #5 — RN-Android watch iOS-only | **CLOSED** — real `@ReactMethod` bodies mirroring iOS | `e749c27`; `NostosTurboModule.kt:135` |
| Tier-2 #6 — OR-set silent clobber | **CLOSED** — `NOSTOS_OR_SET_COLUMNS` + loud `bail!` | `5229d72` |
| Tier-3 #9 — fanout_scale not in CI | **CLOSED** — `cargo test --workspace -- --include-ignored` | `5229d72`; ci.yml:32 |
| Tier-3 #10 — fan_out panic-as-drop | **CLOSED** — separate `faulted` counter + regression test | `5229d72`; `fanout.rs:215` |
| Tier-3 #12 — ADR-0029 §Decision-2 OPEN | **CLOSED (ratified-deferred)** — full-wipe = v1 cross-principal isolation; per-principal outbox deferred | `18b2b87` |
| Tier-3 #11 — make ci count 431 vs 468 | **CLOSED** — reproduced: **477 passed, 0 failed** | `make ci` |
| Tier-3 #13 — 6/9 SDKs no CI gate | **PARTIALLY CLOSED** — flutter/capacitor/rn/node + scale + **dotnet** now CI-gated; swift/kotlin/web remain toolchain-heavy follow-ups | `.github/workflows/ci.yml` |
| Tier-1 #3 — real-PG e2e "assumed green" | **CLOSED (after a fix)** — first run failed `fresh_slot_yields_snapshot_rows_then_live_stream` to **replication-slot exhaustion** (`max_replication_slots=10` + leaked `e2e_*` slots); fixed (bump→20 + prune); now **196 passed, 0 failed** | `docker/docker-compose.yml`; `e2e_pg_snapshot.rs` |

**Still open (operator-only):** Tier-1 #1 cold-stranger ≤5:00 stopwatch (engineering
prereqs now cleared — JWKS verified, Flutter app live; needs W6 prebuilt binaries);
Tier-1 #2 publication pipeline (brew / pub.dev / `release.yml` / tag).

---

## 1. Headline verdict

**Nostos is engineering-complete, and the 08-04 audit *understated* how complete.** The audit named
two launch gates — the cold-stranger test and a "client-side offline-delete / no-data-loss unknown."
This audit **collapses that to one**: the offline-delete unknown is **resolved in code** (§2.2), the
token-refresh gap is **closed** (§2.1), and web is **more capable than documented** (§2.3).

**The single hard launch gate is the cold-stranger test** (§3): operator-executed, but it carries
real engineering risk because JWKS/RS256 + the TLS heuristic have **never hit real Supabase**, there
is **no live Flutter example app** (only in `archive/`), and the W6 release pipeline (brew/pub.dev/
`release.yml`/tag) is **not live**.

**However**, the swarm surfaced **real defects the 08-04 audit missed** (§5) — none individually block
the documented v0.1 LWW launch, but two of them (`dotnet` binding missing `signOut`/`setToken`; OR-set
silent clobber) are *silent* and should be fixed or loud-failed before publication. The structural
root cause for most: **6 of 9 SDKs have no CI build/typecheck gate** (§5.7).

---

## 2. Three 08-04 audit premises this audit FALSIFIED (code supersedes the doc)

### 2.1 Token-refresh close-on-exp — CLOSED @`969ec36` (audit "NOT applied" is stale) · ✅
The audit (lines 10/50/81) and ADR-0029's D4 body both say an already-open socket is *not* torn down
on token expiry. **`969ec36` landed exactly that fix**, ~0s after the audit committed:
- `transport.rs:489-498` — the writer `select!` gained a **3rd branch** `() = exp_for_writer.notified() =>`
  that sends `CloseFrame { code: 4401, reason: "nostos: token expired" }` and `break`s. This is
  **mid-flight teardown of the live socket**, not just the HS256 reconnect-validate path.
- `transport.rs:241` threads `exp` from the handshake token; `:392-409` arms a one-shot deadline;
  `:603` `.abort()`s it on teardown (no leak).
- `auth.rs`: `token_exp()` is **alg-agnostic** (base64 payload decode) → covers HS256 **and** JWKS/RS256.
- Tests: `auth_sync.rs:278 live_socket_is_closed_after_token_exp`, `:315 live_socket_without_exp_stays_open`.
- **Residual:** the spec's mandatory `make bench` before/after (writer `select!` sits on the 833k hot
  path) was **not visibly run** before landing — but the branch is an inert `Notify` until exp fires
  (zero hot-path cost by construction). Honesty nit, not a correctness issue. 🟡

### 2.2 Client-side offline-delete — RECONCILES, does NOT orphan (audit "honest unknown" falsified) · ✅
The audit flagged the client-side offline-delete orphan as the unknown gating any "no data loss" claim.
**The client reconciles; it does not ghost rows:**
- Server brackets snapshots with `snapshot_begin`/`snapshot_end` control frames (`wire.rs:305-356`).
- Apply engine seeds orphan candidates at `snapshot_begin`, reaps local PKs absent at `snapshot_end`
  via `delete_pks` (`apply.rs:252-298`; `in_memory.rs:173`, `SqliteStorage::delete_pks`).
- Pending outbox writes are **exempted** from reaping (`apply.rs:796 snapshot_reconcile_exempts_pending_local_writes`).
- Unit proof `apply.rs:597 snapshot_reconcile_removes_orphans_absent_from_snapshot`; **integration proof
  the audit omitted:** `nostos-client/tests/e2e_client_reconnect_replay.rs:247 real_client_reconnect_applies_replayed_gap_including_delete`.
- Schema is hard-delete; deletes flow on both the incremental (oplog replay, server backfill resolved
  via ADR-0025/F1) and snapshot-reconcile paths. **Safe to claim "no orphaning on the snapshot path."**
- Residual: no dedicated fixture for *multi-user concurrent hard-delete during a client's offline
  window against real PG* — covered structurally, not by a named test. 🟡

### 2.3 Web is NOT "live-only" — in-session offline outbox shipped (audit premise falsified) · ✅
The audit's WS1 premise ("`NostosSocket::write` never touches the Outbox; a write while offline throws")
describes **pre-`f338188` code.** Commit `f338188` (2026-07-31) shipped:
- In-memory Outbox + optimistic local row + flush-on-reconnect: `ffi-wasm/lib.rs:547-565`
  (`enqueue` → `apply_local` → send-if-open → `mark_done`). `in_memory.rs:201 impl Outbox`.
- What is **genuinely absent** is *reload-durability only* (no IndexedDB/OPFS) — and that deferral is
  **honestly documented and sound** (ADR-0017 addendum §3: `Storage`/`Outbox` are sync traits, WASM-clean;
  IndexedDB is async → can't await on main thread).
- **The defect is the inverse of a hidden gap:** README (`:85,92-93`), ADR-0017 addendum §1, and the
  audit all now **understate** the shipped capability ("throws when offline" is false). A user reading
  them is misled toward *less* trust than warranted. Recommended wording: **"offline-capable within a
  session; nothing survives a reload."**

---

## 3. The ONE hard launch gate — cold-stranger test · ❓ (operator-executed, engineering-risk-bearing)

**Requirement** (launch plan W5 + `docs/QUICKSTART.md`): fresh machine + fresh Supabase project →
`brew install nostos && nostos init && nostos dev` + `flutter pub add nostos_flutter` + ~10 lines Dart,
follow QUICKSTART verbatim, stopwatch **≤5:00** to working offline sync. Defining condition: **JWKS
fetch + TLS heuristic hit real Supabase RS256 for the first time.**

**Status:** warm-cache dry-run "passed comfortably"; **cold version never run.** Prebuilt binaries not
published (W6 pending).

**Why it is *not* pure operator:** three engineering prerequisites/risks —
1. **JWKS/RS256 + TLS heuristic never exercised against real Supabase** (unit-tested via `FixtureJwks`
   only). An RS256 bug surfaces here. ❓
2. **No live Flutter example app** — `sdk/nostos_flutter/example/` does not exist; the only runnable
   example + Supabase wiring is in `archive/` (archived `76ed6ee`). The stranger must restore it or
   scaffold fresh. ✅
3. **W6 release engineering not live** — brew tap `homebrew-tap`, `pub.dev`, `release.yml` (fires on
   tag), prebuilt binaries. ✅ (not yet built)

**Honest framing:** operator-executed, **engineering-risk-bearing.**

---

## 4. Remaining work, tiered

### Tier 1 — launch gate (must clear before tag `v0.2.0`)
| # | Item | Status | Owner |
|---|---|---|---|
| 1 | Cold-stranger test ≤5:00 vs real Supabase | ❓ never run | operator (+ eng prerequisites above) |
| 2 | Operator publication pipeline (brew/pub.dev/`release.yml`/tag/Show HN) | 🚧 not live | operator |
| 3 | Real-PG e2e re-run to convert assumed→verified | 🟡 assumed green (docker down this audit; `make pg-up && NOSTOS_E2E_PG=1 ...`) | operator/eng |

### Tier 2 — launch-adjacent defects (fix or loud-fail before publication)
| # | Item | Evidence | Fix |
|---|---|---|---|
| 4 | **`dotnet` committed binding lacks `signOut`/`setToken`** — `nostos.cs` has SignOut=0/SetToken=0/Watch=2 (2052 lines); Rust `lib.rs:591/638` has them + tests `:947/:1017`. Falsifies audit line 63 "all 9 ✅". A committed binding (`.gitignore` says it's checked in "so reviewers can see the C# surface") ships an incomplete surface. | ✅ verified | regenerate `nostos.cs` from the cdylib |
| 5 | **RN reactive `watch` is iOS-only** — Android TurboModule `watchChanges`=0; iOS has it (`NostosTurboModule.mm`, `NostosBackend.swift`); TS facade (`NostosClient.ts:101`) declares it cross-platform → on Android it calls a non-existent native method. WS2 "reactive → all 9" is **not** complete. | ✅ verified | implement Android `watchChanges` (Kotlin TurboModule) or gate the TS facade to iOS |
| 6 | **OR-set CRDT silently clobbers in production** — every enable switch defaults empty (`sqlite.rs:151`, `write_back.rs:223`); the only callers of `with_or_set_*` are tests (`in_memory.rs:652`, `client.rs:1628`, `e2e_pg_writeback.rs:1143`); no env-var/config. A real client calling `or_set_add("tags",…)` is silently clobbered twice (client raw-LWW + server JSONB clobber) with **no error raised**. Not a v0.1 LWW blocker; **is** a silent-data-loss trap for the OR-set/tags fixture. | ✅ verified | wire an env switch (e.g. `NOSTOS_OR_SET_COLUMNS=table:col`) **or** make `or_set_add` on an unconfigured table `bail!` loudly |
| 7 | **No live Flutter example + facade tests archived** — `sdk/nostos_flutter/example/` absent (only in `archive/`); reactive-facade tests (`nostos_facade_test.dart`, `nostos_ws6_test.dart`) archived (`76ed6ee`). Live `flutter test` covers **only** signOut (2/2 green, verified). Gates item #1 and leaves Collection<T>/SyncStatus/watch untested in the shipped package. | ✅ verified | restore example + facade tests (or scaffold fresh for the stranger) |

### Tier 3 — honesty / hardening (non-blockers; clear before "no data loss"/moat claims)
| # | Item | Evidence |
|---|---|---|
| 8 | **ADR-0025-mandated real-PG write-amp measurement never published** — `RESULTS.md` last touched `1f1544b` (07-20); ADR-0025:30 mandates `make bench` re-measure (Tier-5 revert precedent); ADR-0026:48 records it unmeasured. The "ship before/after or revert" mandate is technically **violated**. Non-blocking (oplog writer is opt-in `NOSTOS_BENCH_OPLOG=1`, off the 833k fan-out path; `833305→833307` with it on). | ✅ verified (absent) |
| 9 | **`fanout_scale.rs` scale-regression floor not in CI** — `nostos-application/tests/fanout_scale.rs` has the workspace's only 2 `#[ignore]` markers; `ci.yml:32` runs `cargo test --workspace` with **no `--include-ignored`** anywhere in `.github/`. A stretch-goal regression guard is silently off (its own comment claims CI runs it). | ✅ verified |
| 10 | **`fan_out` counts a panicked task as a drop** — `fanout.rs:182 Ok(DeliveryDecision::Dropped) | Err(_) => dropped += 1`. A `JoinError` (panic/cancel) increments the drop counter — counted, but **mis-attributed** as a slow-client drop (matters for the "0.00% drops" moat honesty). | ✅ verified |
| 11 | **`make ci` test count unverified** — last independently-documented figure is **431** (2026-07-29); the 08-04 audit claims **468** from a run not reproduced here. | ❓ unknown |
| 12 | **ADR-0029 §Decision-2 (per-principal outbox retention) — OPEN** — `Outbox::clear` is one `DELETE FROM cairn_outbox` (`sqlite.rs`, `ponytail:` marker); discards ALL pending writes, not per-principal. The sole sub-decision keeping ADR-0029 from fully-Accepted. Not a hard blocker (discarding = isolation; loses only the signing-out principal's unsynced offline work, multi-user only). | ✅ verified |
| 13 | **6 of 9 SDKs have no CI build/typecheck gate** — `ci.yml` runs `sdk-e2e rust node` only; swift/kotlin/dotnet/capacitor/web/rn are source-presence-only. (Structural root cause for #4/#5 shipping silently.) Runtime signOut parity confirmed only Flutter (2/2) / Swift (8/8 host) / RN-iOS smoke; **other 6 = source-only 🟡**. | ✅ verified |
| 14 | **`make bench` not run before `969ec36`** (see §2.1 residual) and before any future hot-path edit — the project's "measure before optimize" rule (CLAUDE.md) should gate such commits. | 🟡 |
| 15 | **10k stretch goal failed, table-sharded router OPEN (zero code)** — ~61% drops @10k; dominant cost is the per-event full-store ack/eviction scan (O(N×E)). Non-blocker — 1k/5k moat holds (833k @1k / 0%, 660k @5k). Named follow-up: table-sharded router. | ✅ verified |
| 16 | **Tenant-scoped OR-set merge clobbers by design** — `write_back.rs:362-368` falls through to clobber when `tenant.is_some()` (`ponytail:` "tenant-scoped merge deferred to fixture"). Phase-4 fixture co-design. | ✅ verified |
| 17 | **Doc drift (correct the 08-04 audit + 2 ADRs + 1 plan, do not edit without operator sign-off)** — audit lines 10/50/81 (token-refresh), audit §2.9/WS1 (web live-only), audit internal contradiction (ADR-0029 "Proposed" vs file now "Accepted (D1/3/4 shipped; D2 interim)"); ADR-0029 D4 body scope-note; ADR-0017 addendum §1/follow-up #7; `sdk/nostos_web/README.md:85,92-93`. | ✅ verified |

---

## 5. Swarm-found defects the 08-04 audit MISSED (summary)

The audit is thorough but did not catch: the `dotnet` binding gap (#4), RN-Android watch gap (#5),
OR-set silent clobber (#6), archived Flutter tests/example (#7), `fanout_scale` not in CI (#9),
panic-as-drop (#10), and the 6-SDK CI-gap (#13). All six (except #9/#10) share the same root cause:
**defects hide where no test/gate runs** — the recurring "untested surfaces" pattern. The single
highest-leverage structural fix is a CI build/typecheck gate for swift/kotlin/dotnet/capacitor/web/rn (#13).

---

## 6. Load-bearing claim list (Gate 4)

| Claim | Status | Evidence |
|---|---|---|
| Token-refresh live-socket close CLOSED @`969ec36` | ✅ | `transport.rs:489-498`; `auth_sync.rs:278,315` |
| Client offline-delete reconciles (not orphan) | ✅ | `apply.rs:252-298,796,597`; `e2e_client_reconnect_replay.rs:247` |
| Web has in-session offline outbox | ✅ | `ffi-wasm/lib.rs:547-565`; `f338188` |
| Counter CRDT live end-to-end | ✅ | `outbox.rs:236` → `transport.rs:933` → `write_back.rs:816` |
| OR-set CRDT inert in prod (silent clobber) | ✅ | defaults empty; only test callers; no env/config |
| `dotnet` `nostos.cs` lacks signOut/setToken | ✅ | `nostos.cs` SignOut=0/SetToken=0; Rust `lib.rs:591,638` |
| RN watch iOS-only | ✅ | android `watchChanges`=0; iOS 2 files |
| `fanout_scale` `#[ignore]` not in CI | ✅ | `nostos-application/tests/fanout_scale.rs`; `ci.yml:32` |
| `fan_out` panic-as-drop | ✅ | `fanout.rs:182` |
| 833k/208× honestly scoped (fan-out ceiling, not e2e) | ✅ | `RESULTS.md:23-25`; README/STRATEGY/ROADMAP consistent |
| ADR-0025 write-amp never published | ✅ (absent) | `RESULTS.md` @ `1f1544b`; no write-amp anywhere |
| Cold-stranger test ≤5:00 vs real Supabase | ❓ | never run; JWKS/RS256+TLS never hit real Supabase |
| Real-PG e2e green today | 🟡 | docker down; tests + doc-trail exist, not run live |
| `make ci` count 431 vs audit's 468 | ❓ | 431 last independently verified; 468 unreproduced |
| 6/9 SDK signOut runtime parity | 🟡 | source-verified; runtime only Flutter/Swift/RN-iOS |
| JWKS/RS256 works vs real Supabase | ❓ | unit-tested via `FixtureJwks` only |

> **Correction 2026-08-06:** row 185's "833k/208×" — the N× competitor-comparison framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.

---

## 7. What is NOT left (solidly shipped — for calibration)

ADR-0028 declarative SQLite views + partial index; ADR-0024 reactive facade (code, Flutter); ADR-0027
write-outcome/dead-letter; ADR-0029 D1/D3/D4 (signOut all 9 at source; HS256+JWKS exp; live-socket
close); instant-local writes + reconcile (redesign slice-2); PG write-back + tenant-DELETE replay
(ADR-0025/F1, `REPLICA IDENTITY FULL`) + slot-invalidation recovery (P0#1, `911e340`); both
historical bugs fixed (chrono TIMESTAMPTZ bind; `NOSTOS_REPLICATOR` default); graceful-shutdown +
oplog channel-authority (ADR-0026) **in default CI**; the 3 prior SDK defects **fixed** (tauri
`subscribe`, dotnet csproj XML, RN typecheck); counter CRDT live; `OPERATING.md` present/current.

---

## 8. Gaps / honest unknowns (cannot be closed by reading code)

1. **Cold-stranger test result** — only a live run answers it. The single empirical launch gate.
2. **Real-Supabase JWKS/RS256 + TLS** — verified only against `FixtureJwks`; no real-Supabase exercise.
3. **Real-PG e2e today** — assumed green; docker was down this session. Re-run before tag.
4. **`make ci` count** — 431 vs 468; reproduce.
5. **Multi-user concurrent offline-delete vs real PG** — structurally covered, no dedicated fixture.

---

## 9. Recommended next actions (ranked, operator to ratify)

1. **Restore a live Flutter example** + the archived facade tests (Tier 2 #7) — unblocks the stranger
   test and restores shipped-package coverage.
2. **Regenerate `dotnet` `nostos.cs`** (#4) — one-command fix to the silent all-9 gap.
3. **Make OR-set loud-fail** (#6) — `bail!` on `or_set_add` to an unconfigured table, or wire the env
   switch; removes the silent-data-loss trap.
4. **Add a CI build/typecheck gate for the 6 ungated SDKs** (#13) — highest-leverage structural fix.
5. **Run the cold-stranger test** (#1) — the gate; needs #1 above + W6 binaries.
6. **Publish the real-PG write-amp measurement or formally relax ADR-0025** (#8) — clears the
   mandate-violation before any "no data loss"/moat claim.
7. **Correct the doc drift** (#17) — after operator sign-off; do not edit the 08-04 audit in place
   without it.

*This audit did not modify any source or doc — examination only, per the nostos "plans-only" operator
rule. Implementation awaits operator instruction.*
