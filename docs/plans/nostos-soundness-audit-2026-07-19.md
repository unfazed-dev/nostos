# Nostos Project Soundness Audit — 2026-07-19

**Method:** fable-z 5-gate (Hard → frontier end-to-end). Gate 2 fanned out a
7-agent read-only swarm (engine, client-apply, server+cloud, CLI/FFI/bench,
Flutter SDK, docs/playbooks, industry web research). Gate 3 adversarial
synthesis. Gate 3.5 escalated the verdict to the consultant (GLM-5.2, conf HIGH),
which **overturned the priority order**. Gate 4 claim-list with honest
verified/assumed/unknown. No code changed (read-only; scope rule respected).

**Launch bar (the yardstick):** a stranger reaches a working offline
Flutter+Supabase todo app in **≤5 min** (v0.2 gate; v0.1 already tagged complete).
Stated launch posture: **Apache-2.0 self-host for early adopters**.

---

## TL;DR verdict

**Architecturally sound. Not launch-ready.** The architecture, ADR discipline,
security model, and moat are real and verified. Three gaps block launch; one of
them is existential and must be fixed first (not third, as I first thought —
the consultant corrected this, see §2).

| Dimension | Rating |
|---|---|
| Hexagonal invariants | ✅ Solid (verified, no outward deps, `forbid(unsafe_code)` clean) |
| ADR discipline | ✅ Solid (23 ADRs, heavy code-citation: 0013×45, 0012×37, 0018×29) |
| Security (write-back allowlist) | ✅ Solid (defense-in-depth ×2, structural injection boundary) |
| Moat (Rust throughput, Apache-2.0, collapsed-write, predicates) | ✅ Real |
| Replication correctness under operational stress | ❌ **P0 silent data-loss** |
| Demo / stranger-test readiness | ❌ **P0 watch bug** (empty lists) |
| Operability (playbooks/runbooks/debug docs) | ❌ **Systemic gap** — self-host posture breaks |

---

## 1. Consultant verdict (priority reversal — read this)

Consulted GLM-5.2 architecture advisor, confidence HIGH. **Verdict confirmed.**
**Priority corrected.** Direct quote (lightly formatted):

> Slot-invalidation must be P0 #1, not #3. Silent data-loss is an existential
> defect that poisons every downstream consumer; shipping it under any label
> (even "alpha") destroys operator trust permanently. The watch-bug merely
> blocks a demo; the slot bug destroys the product thesis. Demo-breakage is
> visible and recoverable; silent WAL skip is invisible and unrecoverable for
> any customer who hit it during the gap.

**Recommended order (consultant):**
1. **Slot-invalidation FIRST** — `wal_status=lost` detection, SQLSTATE 55000
   handling, snapshot-resync on slot recreation, regression test for the
   dropped-slot path. Gates v0.2 launch.
2. **Watch-bug SECOND** — fix the Dart `_mergeTriggers` replay race + the
   Rust-side emit-before-subscribe ordering (`rust/src/api/nostos.rs:310-315`).
   Gates v0.2 launch.
3. **Playbook THIRD** — minimal operator quickstart + the debug recipe (the
   slot-recreate steps already in `example/README.md`). Ship as v0.2.1 ≤72h.

> Author note (effort vs severity): the watch-bug is the *quick* unblock
> (≈1-line FFI swap + a Dart `_mergeTriggers` fix); the slot fix is substantial.
> Pragmatic sequencing may still do the watch-bug first to unblock the demo for
> continued development — but on **severity**, slot-invalidation is #1. Both
> are P0 hard launch blockers.

---

## 2. Bug catalog (priority order, consultant-weighted)

### P0 — hard launch blockers

| # | Bug | Location | Evidence |
|---|---|---|---|
| P0-1 | **Silent data-loss on slot drop/invalidation.** `ensure_slot_and_publication` treats a *missing* slot as fresh → re-creates, snapshots current state, resumes from current WAL. All offline changes silently skipped. Zero detection, zero alarm. | `crates/nostos-infra/src/replicator/pg.rs:331-345, 407-436` | slice 1 (file:line) |
| P0-2 | **No `wal_status=lost` / SQLSTATE 55000 handling anywhere in code** (grep returns 0 hits across infra). On invalidation the reconnect loop spins forever on a 2s cadence. The 7 e2e tests do not exercise DROP-slot mid-stream or `wal_status=lost`. | `crates/nostos-infra/{src,tests}` | slice 1 (grep) |
| P0-3 | **Watch bug — Dart side (the firing root cause).** `_mergeTriggers` eagerly subscribes to each `watch(t)` during `watchQuery` construction and forwards into a `StreamController.broadcast()` that **does not replay**, defeating the existing `_replayLatest` replay guarantee (whose comment at `nostos.dart:127-133` literally names the symptom "No providers yet."). The snapshot is pushed before `StreamBuilder` subscribes (next-frame `initState`) → event dropped. | `sdk/nostos_flutter/lib/src/nostos.dart:345-361` (merge), `:134` (watch), `:127-133` (replay comment) | slice 5 (file:line) |
| P0-4 | **Watch bug — Rust side (latent race).** `watch()` calls `emit_snapshot().await` (L310) **before** `subscribe_changes()` (L315). Broadcast channel is no-replay (`broadcast::channel(64)`). If a snapshot apply commits in that window the tick is permanently lost. This is the exact invariant encoded in `subscribe_changes_must_precede_apply_to_avoid_missed_snapshot` (`client.rs:1097-1120`); the symptom is named at `client.rs:1051` ("connected but lists render empty"). | `sdk/nostos_flutter/rust/src/api/nostos.rs:310-315`; `crates/nostos-client/src/client.rs:264,1097-1120` | slice 2 (file:line) |

**Watch-bug synthesis:** two independent timing holes at two layers, either
sufficient to cause empty lists. Given the user's flow (snapshot delivered +
ACKed during `_boot`, *then* app renders), **P0-3 (Dart) is the firing bug**;
P0-4 (Rust) is latent and fires under live-edit-during-construction timing.
**Fix both.** The local-write path's `changes.send` (`client.rs:351`) and all
three replicated commit paths (`:542`, `~780`, `:794`) DO broadcast correctly —
the engine is innocent.

### P1

| # | Bug | Location | Severity note |
|---|---|---|---|
| P1-1 | (folded into P0-2) — the no-handling spin is the P1 symptom of the P0-2 root | — | — |

### P2

| # | Bug | Location |
|---|---|---|
| P2-1 | **`FanOutService::run` is O(N×E)** — per-event full-store ack/eviction scans; full bench harness hangs at 10k teardown (worked around with `process::exit` probe). Production fan-out path carries the bottleneck. | `crates/nostos-bench/src/main.rs`, `src/bin/probe_10k.rs:48-67` |
| P2-2 | **10k clients ≈ 61.4% drops** (`RESULTS.md`). The "<1% drops @ 1k" moat is honestly scoped but **weakens above ~5k clients** — narrows the throughput moat at scale. | `benches/results/RESULTS.md` |
| P2-3 | **`rows_for` is NOT on the `Storage` trait** but the FFI `emit_snapshot` depends on it. Implicit contract; a future Storage impl silently breaks watch. | `crates/nostos-client/src/sqlite.rs:208`; `rust/src/api/nostos.rs:517-520` |
| P2-4 | **`watch(t)` fans out 6 Rust pumps per `watchMapped` × 6 callers = 36 pumps** — wasteful, widens the dropped-event window. | `sdk/nostos_flutter/lib/src/nostos.dart:134` |
| P2-5 | **Test gap** — watch tests always subscribe before emit; never exercise the construction→subscribe gap (the bug's regression is silent). | `nostos_test.dart:151`, `nostos_ws6_test.dart:29` |
| P2-6 | TLS path unverified against real Supabase (only `connect_plain` is e2e-tested). | `crates/nostos-cli/src/pg.rs:130-145` |
| P2-7 | nostos-ffi-wasm browser WS glue has **no automated test** (Node smoke + manual demo only). | `crates/nostos-ffi-wasm/src/lib.rs:22-27` |
| P2-8 | `resume()` doesn't cancel prior `.listen` → repeated resume leaks state-controller subscriptions. | `sdk/nostos_flutter/lib/src/nostos.dart:331-333` |

### P3 (low, catalog only)

- `/schema` unauthenticated by design (v1); leaks publication-wide metadata. Multi-tenant operator must gate before public exposure. `nostos-server/src/main.rs:486,546-553`
- pk column hardcoded `"id"`; SqlValue inferred from JSON shape, not column OID. `write_back.rs:178-181,710-711` (ponytail; awaits schema registry).
- Empty `NOSTOS_CORS_ORIGINS` → `CorsLayer::permissive()` + `allow_credentials(true)` (browser-rejected combo; explicit-origins is the documented prod path). `main.rs:461-462`
- Asymmetric JWT claim validation: JWKS checks `exp`; HS256 doesn't; `validate_aud=false` on both. `jwks.rs:90`, `auth.rs:74`
- `apply_local` `WriteOp::Patch` is a no-op (ponytail). `sqlite.rs:660`
- **Stale positioning docs**: CLAUDE.md + README headline say 142k/35.6×; actual is **833k @ 1k clients/0% drops** (208× a competitor's published ceiling). Project memory under-sells the moat. `CLAUDE.md`, `README.md:13,23,209` vs `benches/results/RESULTS.md`
  > **Correction 2026-08-06:** the N× competitor-comparison framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.
- frb 2.13.0-beta.5 dependency (beta tag adds risk; pin + migration path needed pre-launch). `sdk/nostos_flutter`

---

## 3. Playbook gap matrix (the user's concern — confirmed)

Only **`sdk/nostos_flutter`** has all four playbook dimensions. **Every Rust
crate lacks an explicit playbook.** P=present, T=thin, M=missing, N/A=not applicable.

| module | dev | test | debug | operate |
|---|---|---|---|---|
| nostos-domain | T | T | M | N/A |
| nostos-application | T | T | M | N/A |
| nostos-infra | T | P (15 files) | M | T |
| nostos-server | T | M (0 files) | **M** | **M** |
| nostos-core | P | T | M | N/A |
| nostos-client | P (README) | P (10 files) | M | T |
| nostos-ffi-wasm | T | M | M | M |
| nostos-cli | T | T | M | M |
| nostos-bench | **M** | M | M | T |
| nostos-cloud | T | M | M | M |
| nostos-license | T | M | M | N/A |
| **sdk/nostos_flutter** | **P** | **P** | **P** | **P** |

**Worst gaps (block the self-host posture):**
- **nostos-server** (most operationally critical): no operator runbook; env vars
  scattered across `main.rs:7-14`, Makefile, QUICKSTART. No deployment/log/
  metrics doc.
- **nostos-cli**: no command reference; subcommands undiscoverable (every call
  is `cargo run -p nostos-cli -- ...`).
- **nostos-bench**: no `lib.rs` AND no README.
- **nostos-cloud + nostos-license**: zero operate/debug docs despite being the
  monetization surface.
- **Debug dimension missing workspace-wide**: the slot-recreate recipe at
  `example/README.md:194-239` is the *only* dedicated debug recipe in the repo.
  No "if X breaks, do Y" doc for any Rust crate — directly explains why the
  watch bug took two sessions to diagnose.

**Substitutes that partly compensate** (workspace-level): `docs/ARCHITECTURE.md`
§2 (per-crate dev view), §5 (testing), §6 (add-a-port recipe); `Makefile`
targets; 9/11 crates have solid `lib.rs` doc-comments. These are enough for a
contributor, not for an operator.

**Onboarding verdict:** Rust/Flutter contributors ramp <30 min. **Operators
break** (no CLI binary until W6; no single-source operator runbook; Supabase
track pending live verification; no debug recipes for non-Flutter crates).

---

## 4. Industry benchmark (July 2026)

nostos's premise was re-checked: it **does** have a declarative per-user filter
layer — the **dynamic predicate engine** (ADR-0003/0011/0012) is the headline
differentiator, not a gap.

**What nostos does BETTER:**
1. **Rust throughput** — 142k ops/sec @ 1k clients e2e vs a competitor's published
   ~2–4k server replication ceiling ≈ **35× headroom** (833k current = ~208×).
   No other competitor publishes a comparable end-to-end fan-out number
   (Electric only benches its storage engine; everyone else is TS/Elixir/Go).
   > **Correction 2026-08-06:** the N× competitor-comparison framing compared fan-out to replication-ingest (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.
2. **Apache-2.0 today, no FSL/AGPL trap** — a leading competitor's server is FSL (2-yr wait,
   no-compete); Triplit is AGPL-3.0 (network-copyleft); Couchbase SG CE is
   source-available. nostos is the **only** Postgres-native 2-way offline sync
   engine that is Apache-2.0 on day one.
3. **Collapsed-apply write-back DX** — direct write-back (ADR-0013), no
   `uploadData()` to build/host (a common DX complaint about comparable sync SDKs), no split
   endpoint contract. Electric can't write at all.

**What nostos LACKS vs 2026 table-stakes (none are moats — all are buyer demands):**
1. **Operational instrumentation** — zero `opentelemetry`/`tracing`/`prometheus`
   symbols in server source; zero runbook/playbook/observability markdown.
   Electric/Couchbase all ship OTel + metrics + alerting. Infra teams
   will not sign without this.
2. **Managed cloud** — every commercial competitor has one (Electric Cloud,
   y-sweet.cloud, Triplit Cloud, Couchbase Capella). ADR-0006
   plans open-core cloud; nothing ships. Self-host only today.
3. **Compliance certs** (SOC 2 / HIPAA) — zero customers, zero certs. Hard
   blocker for healthcare/finance buyers.
4. **Per-row RLS fidelity** — nostos auth is one `auth_scope` tenant column
   (ADR-0018), coarser than a comparable sync SDK's passthrough to Supabase RLS or
   Couchbase's sync-function. STRATEGY.md §2 concedes.
5. **Client SDK breadth** — Flutter + WASM only. RN (ADR-0020) and broader
   surface still being built.

**Consultant-added risks (question c — none of my 7 agents surfaced these
prominently):**
- [HIGH] **Single-host 142k bench masks Postgres failover + replication-lag
  behavior** — the moat claim needs multi-node evidence before Show HN.
- [HIGH] **frb beta dependency** — version-lock wedge worsening over time; pin +
  migration path needed pre-launch.
- [MEDIUM] **Collapsed-apply write-back has no visible conflict-resolution
  story** for concurrent offline edits that survive the allowlist (one doc page
  closes the question).
- [MEDIUM] **FSL→Apache provenance** trust risk for enterprise self-host
  adopters if unclear.

**Industry verdict:** credible for a 2026 self-host launch; **not a drop-in
replacement for the leading hosted sync product**. Position honestly as "Apache-2.0 self-host for early
adopters who want Rust throughput and don't want to build `uploadData()`," with
cloud + SOC 2 on a published roadmap.

---

## 5. Claim list (Gate 4 — verified / assumed / unknown)

| Claim | Status | Evidence |
|---|---|---|
| Hexagonal invariants hold (no outward deps from domain/application; `forbid(unsafe_code)` clean) | **verified** | slice 1: grep + Cargo.toml (domain has zero async/tokio, depends only on serde/uuid/bytes/thiserror) |
| Write-back allowlist is defense-in-depth ×2 with structural injection boundary | **verified** | slice 3: `transport.rs:619`, `write_back.rs:255/437/544`, regex+quote_ident+parameterized, tests reject `a; DROP TABLE x` |
| Watch bug Dart root cause = `_mergeTriggers` eager/non-replay defeating `_replayLatest` | **verified** | slice 5: `nostos.dart:345-361,134,127-133` file:line |
| Watch bug Rust latent race = emit_snapshot before subscribe_changes | **verified** | slice 2: `rust/src/api/nostos.rs:310-315`, `client.rs:264,1097-1120` |
| Replicated apply path DOES broadcast to `changes` (engine innocent) | **verified** | slice 2: `client.rs:542,~780,794,351` |
| Slot-invalidation treats missing slot as fresh (silent skip) | **verified** | slice 1: `pg.rs:331-345,407-436` |
| Zero `wal_status=lost`/SQLSTATE 55000 handling in code | **verified** | slice 1: grep 0 hits across `crates/nostos-infra/{src,tests}` |
| Only Flutter SDK has a playbook; every Rust crate lacks one | **verified** | slice 6: playbook matrix (P/T/M per module) |
| 142k @ 1k reproducible; 833k current; 0% drops | **verified** | slice 4: `RESULTS.md` provenance |
| No caller anywhere distinguishes "fresh slot (first boot)" from "fresh slot (dropped)" | **assumed** | slice 1 honest unknown; consultant assumed same. Tried: grep in `ensure_slot_and_publication`; did not trace every call site |
| Slot bug fires for the target Supabase self-hoster (not an edge case) | **assumed** | reasoning: Supabase caps `max_slot_wal_keep_size`; nostos-server offline → slot goes `lost` (the documented failure mode in `example/README.md:194-237`). Not empirically reproduced this session |
| frb 2.13.0-beta.5 no-listener buffering behavior | **unknown** | slice 5: could not read frb internals; beta tag adds risk |
| Postgres failover / replication-lag at scale | **unknown** | consultant followup; single-host Apple-Silicon loopback bench only, never on cloud hardware or against real PgReplicator at scale |
| Conflict-resolution story for concurrent offline writes | **unknown** | consultant; not surveyed this pass (ADR-0013 = server-auth LWW, but no doc/example) |
| `make ci` is green right now | **unknown** | read-only audit; no agent ran it. Compile-cleanliness + `unsafe` counts are from grep |

The all-`verified` rows are balanced by four load-bearing `assumed`/`unknown` —
honest uncertainty on the claims that hold the verdict up.

---

## 6. Prioritized recommendations (consultant-ordered)

**Do not launch v0.2 until P0-1/P0-2 and P0-3/P0-4 are fixed + verified.**

1. **[P0] Slot-invalidation correctness.** Add `wal_status=lost` detection +
   SQLSTATE 55000 handling + snapshot-resync-on-recreation + a regression test
   against real PG that DROPs the slot mid-stream. Until this lands, nostos can
   silently lose data for any self-hoster who disconnects long enough — the
   documented failure mode. *Gates launch.*
2. **[P0] Watch bug (both sides).** Dart: make `_mergeTriggers` lazy (subscribe
   in `controller.onListen`) OR wrap its output in a replay buffer. Rust: swap
   `emit_snapshot`/`subscribe_changes` order in `rust/src/api/nostos.rs:310-315`.
   Add a regression test that mounts a real `StreamBuilder` against a fake
   engine emitting pre-subscribe (closes the P2-5 test gap). *Gates the stranger
   test.*
3. **[P0→v0.2.1] Operator playbook.** Ship a single `docs/OPERATING.md`
   covering: nostos-server env vars + startup-failure modes (incl. the Fix-A
   `NOSTOS_REPLICATOR != pg` bail at `main.rs:437`), the slot-recreate recipe,
   the "connected but lists empty" 5-line diagnostic checklist, a CLI command
   reference. *Blocks the self-host posture; ≤72h after launch per consultant.*
4. **[P2] Scalability honest-scoping.** Fix the `FanOutService` O(N×E)
   bottleneck OR explicitly scope the throughput moat to "≤5k clients" and
   re-run the bench multi-node (consultant: moat claim needs failover/replication-lag evidence).
5. **[P3] Refresh stale positioning.** Update CLAUDE.md + README headline to
   833k (from 142k). One-line fix; stops under-selling the moat.
6. **[P2] frb pin + migration path** before launch (consultant HIGH).
7. **[P2] One conflict-resolution doc page** for the collapsed-apply write-back
   (consultant MEDIUM — closes the "how do concurrent offline edits reconcile?"
   question).

---

## 7. What this audit did NOT cover (honest gaps)

- Did not run `make ci` (read-only); compile-cleanliness + `unsafe` counts are grep-level.
- Did not empirically reproduce the slot-invalidation silent-loss against real
  PG (reasoned from code + the documented failure mode).
- Did not run the Flutter app to capture a runtime trace of the watch bug
  (static analysis + the existing regression test invariant).
- Did not audit Postgres failover/replication-lag (consultant followup;
  single-host bench only).
- Did not survey the collapsed-apply conflict-resolution behavior in depth
  (flagged unknown).
- Did not verify which `launch-readiness-gap-list.md` items (B1 README, B5
  ADR-0012 status line, B6 STRATEGY §122) are closed vs still open.

These are the follow-ups. The audit is sufficient to support the verdict; it is
not sufficient to sign off on production deployment without the P0 fixes + the
multi-node bench.

---

## 8. Sources (swarm + consult)

- 7-agent swarm transcripts (engine, client-apply, server+cloud, CLI/FFI/bench,
  Flutter, docs, industry) — session `c4ae46b3-8467-427f-92d0-8409a30a9dc2`.
- Consultant ledger: `architecture` domain, qhash
  `497cb3df…`, GLM-5.2, conf HIGH (2026-07-19).
- Internal: `docs/adr/0001-0023`, `docs/ROADMAP.md`, `docs/STRATEGY.md`,
  `docs/SECURITY.md`, `docs/BENCHMARK-METHODOLOGY.md`,
  `docs/plans/flutter-supabase-plug-and-play-launch.md`,
  `docs/plans/launch-readiness-gap-list.md`, `benches/results/RESULTS.md`.
- Industry: Electric docs (shapes, writes, pivot), Replicache, Triplit, CR-SQLite, Y-Sweet, LiveStore,
  Couchbase Sync Gateway, PouchDB — full URLs in the slice-7 transcript.
