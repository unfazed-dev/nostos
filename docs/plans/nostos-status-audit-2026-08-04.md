# Nostos Status Audit — 2026-08-04

**Question:** What is left to do, and where is Nostos at?
**Method:** fable-z Hard-band audit; 6 parallel subagents read master-plan, ROADMAP, all 30 ADRs, the 4 workstream plans, the 9-SDK source tree, git log + CI + RESULTS, and the known-issue list. Synthesis cross-checked (Gate 3) against conflicting subagent findings.
**Claims are tagged:** ✅ verified this session · 🟡 assumed · ❓ unknown (load-bearing, not yet opened).

> **POST-INVESTIGATION UPDATE (same day, Wave 2):** the two ❓ load-bearing claims below were then resolved by a deeper 5-subagent investigation + a live `make ci` run:
> - **Client-side offline-delete orphan → ✅ RESOLVED.** Traced end-to-end on the *client apply path*: server `snapshot_begin`/`snapshot_end` control frames (`transport.rs:717,726`) → client pump intercepts before row-decode, exempting pending outbox PKs (`client.rs:1018`) → apply engine reaps absent PKs (`apply.rs:272-298`) → `SqliteStorage::delete_pks` (`sqlite.rs:766`). Proven by `snapshot_reconcile_removes_orphans_absent_from_snapshot` (`apply.rs:597`), green in the 468-test run. (The original audit's "two subagents conflicted" was a server-oplog-vs-client-apply conflation — resolved.)
> - **`make ci` → ✅ GREEN @ 468 passed / 0 failed / 1 ignored** (clippy `-D warnings` clean, fmt clean). Resolves the contradictory 188/250/273/431 doc counts.

> **CORRECTIONS (2026-08-05 fix pass — see `docs/plans/nostos-remaining-work-2026-08-05.md`):** three
> premises in this audit's body are STALE vs HEAD `67eecc3` and should be read with these corrections:
> - **Token-refresh close-on-exp is now CLOSED** (`67eecc3`: writer `select!` sends `CloseFrame 4401` at
>   JWT `exp`, alg-agnostic; tests `auth_sync.rs`). The audit's "packaged, NOT applied" (lines 10/50/81)
>   and the ADR-0029 D4 "future hardening" caveat are superseded — ADR-0029 D4 body corrected in place.
> - **Web is NOT "live-only / no outbox"** — `9004b3c` shipped an in-session outbox + optimistic row +
>   flush-on-reconnect (`ffi-wasm/lib.rs:547-565`); only reload-durability is absent (deferred, ADR-0017).
>   The audit's WS1 premise (§2.9) and `sdk/nostos_web/README.md` understate the shipped capability.
> - **ADR-0029 is `Accepted`** (its Status line reads "D1/3/4 shipped; D2 interim"), NOT "Proposed / the
>   only non-Accepted" — the audit's TL;DR/Tier-B lines that say otherwise are the drift. D2 remains the
>   sole open sub-decision.
> - **Token-refresh close-on-exp → packaged, NOT applied.** Real gap, ~25-line fix designed + test feasible via the existing `auth_sync.rs` harness; deferred because the writer `select!` is on the 833k hot path and the project rule mandates a `make bench` before/after this session couldn't run. Spec: `docs/plans/token-refresh-close-on-exp-ready-2026-08-04.md`.
> - **Doc accuracy landed:** ADR-0029 (D1/D3/D4 shipped; D2 interim), ADR-0014 + ADR-0004 (CRDT shipped via ADR-0030), ADR-0025 (offline-delete P0 annotated RESOLVED).

---

## TL;DR

Nostos is **engineering-complete and launch-gated on the operator, not on engineering** — with **one hard empirical gate still unproven** (the cold-cache stranger test ≤5:00 vs a real fresh Supabase project; JWKS/TLS has never hit real Supabase) and **one honest unknown** that should be resolved before any "no data loss" launch claim (the **client-side** offline-delete orphan / WAL backfill, see §3). The 833k ops/sec moat figure is valid *as the narrow fan-out ceiling it claims to be*; it is not end-to-end, and an ADR-0025-mandated real-PG write-amp measurement was never published.

---

## 1. Where Nostos is at (phases)

| Phase | Status | Source |
|---|---|---|
| P0 Spike / moat baseline | ✅ DONE (142k → refreshed 833k @1k / 0%) | ROADMAP:7, RESULTS.md:21 |
| P1 Core + real Postgres | ✅ DONE (code-complete) | ROADMAP:21,113 |
| P2 Dynamic predicates + multi-platform SDKs | ✅ DONE (reactive facade ADR-0024, sdk-e2e) | ROADMAP:46 |
| **P3 OSS launch** | 🚧 **GATED on operator** (code-complete; publication is operator calls) | ROADMAP:62,113 |
| P4 DX moat (write-back, tiered conflict, Cloud GA) | OPEN (future) | ROADMAP:74 |
| P5 Enterprise (SSO/SAML, SOC2, HIPAA, RBAC) | OPEN (future) | ROADMAP:86 |

v0.2 launch plan (`flutter-supabase-plug-and-play-launch.md`): W0a–W8 **all implemented + verified locally** (:168–171). Warm-cache dry-run passed; **cold stranger test never run** (:181–183).

---

## 2. What's left — ranked

### Tier A — blocks a credible launch claim
1. **Cold-cache stranger test ≤5:00 vs a real fresh Supabase project** — never run. 🟡 JWKS fetch + TLS heuristic are code-complete but **unverified against real Supabase**. This is the single hardest engineering-adjacent gate standing. *(launch plan :174–188, :196–197)*
2. **🟡 Client-side offline-delete orphan / WAL backfill — UNKNOWN.** Two subagents disagreed: the server-side oplog backfill is ✅ resolved (ADR-0025, all slices + F1 `a711df7`, `nostos-infra/oplog.rs`, `chaos_resume` 5/5), but the **client apply-engine** offline-delete reconciliation (`nostos-core`) was NOT opened this session. Memory records it as unsound (per-session sink doesn't survive reconnect; present-rows-only snapshot orphans hard-deletes in multi-user). **Resolve this before quoting "no data loss."** It also gates WS2's DataTrust P0.
3. **Operator publication steps (non-engineering):** fresh Supabase project; push `main` → `unfazed-dev/nostos`; pub.dev; `homebrew-tap` tap; tag `v0.2.0` (fires `release.yml`); launch-day benchmark; Show HN. *(launch plan :200–219)*

### Tier B — must be decided/closed before tag, but small
4. **ADR-0029 §Decision-2 (per-principal outbox retention) — OPEN.** `Outbox::clear` currently discards ALL pending writes (`sqlite.rs:1071`, `ponytail:` marker); awaiting operator ratification. This is the **only** open ADR-0029 decision and the reason ADR-0029 is still **Proposed** (the only non-Accepted ADR).
5. **ADR-0029 status → Accepted** once D2 is decided.
6. **Test-count truth.** Docs contradict: most-recent documented = **431 (2026-07-29)**, but others cite ~188 / ~250 / ~273. ❓ Run `make ci` for a ground-truth number before quoting one publicly.

### Tier C — honesty / hardening, not blockers
7. **Real-PG write-amp re-measurement (ADR-0025-mandated) — OPEN.** RESULTS.md unchanged since slice-2 (`4c892ad`). 833k/0% is honest *as the in-memory fan-out ceiling* (oplog attach is opt-in via `NOSTOS_BENCH_OPLOG=1`, channel-send cost invisible, `main.rs:161–168`); it is **not** end-to-end (FakeReplicator, loopback, no client apply). Never compare eval-only vs end-to-end numbers.
   > **Correction 2026-08-06:** this item's closing directive ("quote the 208× high multiple only") is retired — the N× vs PowerSync framing compared fan-out to replication-ingest (unit mismatch); see benches/results/RESULTS.md §Correction.
8. **Token-refresh hardening gap (disclosed):** P1 *fixed* via the `setToken` swap contract, auto-wired in `NostosDatabase.supabase` (`nostos.dart:486–495`) off `onAuthStateChange`. But a **live socket is NOT torn down mid-flight on token expiry** — refresh takes effect next reconnect. Raw `Nostos` users must wire refresh themselves.
9. **WS1 web durability — deferred past v0.1 by design** (ADR-0017 addendum, IndexedDB mirror rejected). Web is **live-only**, not just non-durable (`NostosSocket::write`, `ffi-wasm/lib.rs:496`). Open: a Worker landing Storage + Outbox together.
10. **WS3 CRDT tenant+OR-set — falls through to clobber** (no-tenant merge only). The community-row fixture exercising it is not built.
11. **F5 typed payloads:** PgReplicator delivers all values as JSON strings — decide map-types-now vs document-loudly.

---

## 3. Workstream + SDK summary

- **WS1 (web durability):** deferred (§2.9).
- **WS2 (reactive facade):** ✅ Flutter (ADR-0024) + ported to Swift/Kotlin/RN-iOS. DataTrust P0 gated on §2.2.
- **WS3 (CRDT, ADR-0030):** ✅ engine complete — counter, HLC+OR-set merge (`75e65bd`), storage apply-merge (`28df948`), **server WriteBack element-merge SHIPPED `317b4d1`** (real-PG e2e green), client HLC+optimistic (`45fdc70`), D7 bench gate (`7835af3`, `WireFrame` byte-unchanged → moat intact). *Corrects memory, which said slice-3 was deferred-to-fixture.*
- **WS4 (signOut, ADR-0029):** D1 ✅ (`b92222c`), D3 ✅ all 9 SDKs, D4 ✅ HS256 exp (`04360f6`), **D2 OPEN**.
- **9-SDK capability matrix:** all 9 (flutter, react_native, swift, kotlin, web, tauri, node, dotnet, capacitor) expose **signOut + setToken + watch/subscribe** at source level. Runtime parity: Flutter (2/2), Swift (8/8 host tests), RN-iOS (TurboModule smoke) verified; **others = source-presence only (🟡)**.

---

## 4. Claim list (Gate 4)

| Claim | Status | Evidence |
|---|---|---|
| Code-complete; P3 gated on operator, not engineering | ✅ | ROADMAP:113; launch plan :168–171 |
| Cold stranger test ≤5:00 never run vs real Supabase | ✅ | launch plan :181–183 |
| Server-side oplog backfill resolved (ADR-0025) | ✅ | `nostos-infra/oplog.rs`, `a711df7`, chaos_resume 5/5 |
| Client-side offline-delete orphan resolved | ❓ | not opened; subagents conflicted; memory says unsound |
| All 9 SDKs have signOut+setToken+watch (source) | ✅ | grep + WS4-D3 commit trail |
| All 9 SDKs runtime-correct | 🟡 | only 3 of 9 have runnable tests cited |
| 833k@1k / 0% drops valid as fan-out ceiling | ✅ | RESULTS.md:21–24, :33–43 caveat |
| 833k is end-to-end | ✅ **false** | FakeReplicator/loopback/no-client-apply (RESULTS.md caveat) |
| Real-PG write-amp re-measure published | ✅ **OPEN/not done** | ADR-0025 mandate; RESULTS.md unchanged since `4c892ad` |
| Token-refresh P1 fixed | ✅ | `setToken` contract; `nostos.dart:486–495`; `04360f6` |
| Live-socket mid-flight expiry drop implemented | ✅ **not done (disclosed)** | open hardening |
| WS3 server element-merge shipped | ✅ | `317b4d1` + real-PG e2e test |
| ADR-0029 D2 (per-principal outbox) decided | ❓ | OPEN; keeps ADR-0029 Proposed |
| `make ci` currently green @ N tests | ❓ | not run this session; docs cite 431 (2026-07-29) 🟡stale |

---

## 5. Sources consulted

- `docs/ROADMAP.md` · `docs/plans/flutter-supabase-plug-and-play-launch.md` · `docs/plans/launch-readiness-gap-list.md`
- `docs/adr/` (all 30: 0001–0030)
- `benches/results/RESULTS.md` · `nostos-bench/src/main.rs:161–168`
- `nostos-infra/src/oplog.rs` · `crates/nostos-client/src/client.rs` · `sdk/nostos_flutter/lib/src/nostos.dart`
- git log (last 20) · `.github/workflows/ci.yml`
