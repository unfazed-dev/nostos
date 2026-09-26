# Plans index — what is live, what is history

This index classified 23 plans on 2026-07-30 so readers could tell which were
live and which had been overtaken. More plans have arrived since then; their
status must be read from the individual document until this index is expanded.
**Start here before opening one of the plans listed below.**

Four classes:

- **CURRENT** — the present source of truth for its topic.
- **GATED-ON-GO** — ratified or drafted, deliberately deferred pending an explicit operator
  decision. **These are live, not dead.** Do not treat them as superseded.
- **DONE** — the work shipped; kept as the historical record of why and how.
- **SUPERSEDED** — a later document owns this topic now, or the premise was falsified. The
  superseding document is named. Read the replacement, not this.

**For the plans listed here, this index is authoritative; the plan files are not.** Only one plan carries an inline
banner — `sdk-live-e2e-consolidation.md` — because it actively misdirects a reader who opens
it cold. **Every other plan is unmarked, including SUPERSEDED ones.** So an absent banner
means nothing: check this table.

**Cleanup 2026-09-24:** executed or superseded plans that nothing cites as evidence were
deleted (removed in cleanup; see git history). Among them: `HANDOFF.md`,
`nostos-next-after-oplog-epoch-2026-07-20.md`, `supabase-flutter-smoke-results.md`,
`test-coverage-gap-analysis.md` and `flutter-pomodoro-persona-e2e-baseline.md`. Plans an
ADR, ROADMAP, RESULTS, code, CI or a script cites stay, even when DONE or SUPERSEDED.

**Cleanup 2026-09-26:** the Supabase Realtime adopter playbook moved from the
stray top-level `plans/` directory to `docs/guides/supabase-realtime/`, with
the `make playbook` target updated. Cited historical plans stay until their
references can be revised safely; broader pruning awaits an agreed keep list.

The **Basis** column is deliberate: `verified` means established from the repo or a run this
session; `inferred` means read off the plan's own header or cross-referenced but not
re-proven. Treat `inferred` rows as good-faith classification, not fact.

## CURRENT

| plan | topic | basis |
|---|---|---|
| `adr-and-docs-completion-audit-2026-07-30.md` | **Read alongside the assessment below.** Audits all 28 ADRs + `docs/` against code. Finding: the engine is **more complete than its own ADRs claim** — six status lines understate reality, including ADR-0013 (direct write-back, the headline moat, filed "Deferred" long after shipping). All six corrected. Four genuine gaps; only token-refresh and web durability plausibly block a launch. | verified |
| `nostos-completion-assessment-2026-07-29.md` | Overall project state: what is done, what gates launch. Carries the A1–A10 addendum **and the 2026-07-30 A11 / README-drift addendum** (five defects in places no test executes: an unreachable Tauri command, a malformed csproj, a failing RN typecheck, and two READMEs that misdescribed shipped behaviour). Engineering column is empty; the SDKs are packaged but **not published**. | verified |
| `flutter-supabase-plug-and-play-launch.md` | The master plan — W0–W8 launch sequencing and the ≤5-min stranger-test gate. | inferred |
| `reconnect-glitch-fix-2026-07-19.md` | Reconnect UI glitch. Phase 2 (the epoch resume-gate) has SHIPPED — `transport.rs` skips the snapshot on epoch+checksum match (`snapshot_on_epoch_mismatch` test); this row's "still open" note was stale (corrected 2026-08-17). | verified |

## GATED-ON-GO — live, awaiting an operator decision

| plan | topic | basis |
|---|---|---|
| `nostos-ai-privacy-and-runner-roadmap.md` | AI-privacy moat: zero-knowledge E2EE + WYSIWYS egress, decoupled nostos-AI layer. | inferred |
| `dart-dev-api-reactive-facade-2026-07-19.md` | `Collection<T>` + `SyncStatus` reactive facade (ADR-0024). Header: "Proposed (awaiting go)". | inferred |
| `nostos-provider-dashboard-multitable.md` | Multi-table offline-first demo; would supersede the single-table Tasks example. Header: "proposed (awaiting operator sign-off)". | inferred |
| `nostos-cloud-trust-and-coverage.md` | Cloud licence trust + e2e coverage. Header: "PLAN — no implementation without explicit operator go". | inferred |

## DONE — shipped; kept as the record

| plan | outcome | basis |
|---|---|---|
| `sdk-parity-final-three.md` | RN + Capacitor + .NET landed. Its "→ 10/10" bar is **met**: all ten slices pass a live round-trip in strict mode (2026-07-30). Read that as *functional* parity only — every SDK is now packaged (v0.1.0, LICENSE, repository, README) but **none is published to a registry**. | verified |
| `nostos-persisted-oplog-backfill-2026-07-19.md` | ADR-0025 — all 7 slices + F1/F2/F3 shipped; real-PG e2e green. | verified |
| `nostos-soundness-audit-2026-07-19.md` | 3 P0s all resolved (slot invalidation, watch bug, the OPERATING.md playbook gap). | verified |
| `sync-strategy-analysis-2026-07-19.md` | Conclusion ratified: ONE strategy, no top-level `SyncStrategy` enum; per-field conflict tier is the seam. | verified |
| `complete-nostos-fully-wired-operational.md` | v0.1 is code-complete: real-PG default, predicates, snapshot-on-subscribe. | inferred |
| `nostos-reference-demo-app.md` | Produced `sdk/nostos_flutter/example` (restored by A1; its integration test passes). | inferred |
| `w4-packaging-fallback.md` | Spike record — proved the Flutter↔Rust packaging path before W4 was built. | inferred |

## SUPERSEDED — read the replacement instead

| plan | superseded by | why | basis |
|---|---|---|---|
| `launch-readiness-gap-list.md` | `nostos-completion-assessment-2026-07-29.md` | Both answer "what stands between us and launch"; the 07-29 assessment is 17 days newer and column-splits engineering vs operator work. | inferred |
| `sdk-live-e2e-consolidation.md` | `sdk-parity-final-three.md`, then the 10/10 strict run | Its bar was **7/7** platforms. There are now ten, and all ten pass. | verified |

---

Adding this index rather than stamping 23 individual headers was deliberate: one file to keep
accurate beats 23 that drift independently, and it avoids editing plans in place — mislabeling
a GATED-ON-GO plan as SUPERSEDED would destroy live strategy work. One file did get an
inline header anyway, because opening it directly (without this index) leads you wrong:
`sdk-live-e2e-consolidation.md`.
