# Plans index — what is live, what is history

23 plans accumulated here with no status markers, so a reader could not tell which were
live and which had been overtaken. This index is the answer (A8, 2026-07-30). **Start here
before opening any plan in this directory.**

Four classes:

- **CURRENT** — the present source of truth for its topic.
- **GATED-ON-GO** — ratified or drafted, deliberately deferred pending an explicit operator
  decision. **These are live, not dead.** Do not treat them as superseded.
- **DONE** — the work shipped; kept as the historical record of why and how.
- **SUPERSEDED** — a later document owns this topic now, or the premise was falsified. The
  superseding document is named. Read the replacement, not this.

**This index is authoritative; the plan files are not.** Only three plans carry an inline
banner — `HANDOFF.md`, `sdk-live-e2e-consolidation.md`, and the Flutter connection redesign —
because those three actively misdirect a reader who opens them cold. **Every other plan is
unmarked, including SUPERSEDED ones.** So an absent banner means nothing: check this table.

The **Basis** column is deliberate: `verified` means established from the repo or a run this
session; `inferred` means read off the plan's own header or cross-referenced but not
re-proven. Treat `inferred` rows as good-faith classification, not fact.

## CURRENT

| plan | topic | basis |
|---|---|---|
| `nostos-completion-assessment-2026-07-29.md` | Overall project state: what is done, what gates launch. Carries the A1–A10 addendum **and the 2026-07-30 A11 / README-drift addendum** (five defects in places no test executes: an unreachable Tauri command, a malformed csproj, a failing RN typecheck, and two READMEs that misdescribed shipped behaviour). Engineering column is empty; the SDKs are packaged but **not published**. | verified |
| `flutter-supabase-plug-and-play-launch.md` | The master plan — W0–W8 launch sequencing and the ≤5-min stranger-test gate. | inferred |
| `reconnect-glitch-fix-2026-07-19.md` | Reconnect UI glitch. Its own header says "Phase 1 implementing; Phase 2 tracked" — Phase 2 is still open. | inferred |

## GATED-ON-GO — live, awaiting an operator decision

| plan | topic | basis |
|---|---|---|
| `nostos-flutter-powersync-connection-redesign.md` | PowerSync-style Dart API (Schema/Connector/NostosDatabase). **See the caveat below** — the diagnosis that motivated it was falsified, but the API-shape decisions were separately ratified. | verified |
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
| `nostos-next-after-oplog-epoch-2026-07-20.md` | Its C-claims are closed: C6/C7/C10/C11 verified-fixed 2026-07-20; the last open one, **C9** (W5 empirical re-verify), is closed by the 10/10 strict run on 2026-07-30. | verified |
| `sync-strategy-analysis-2026-07-19.md` | Conclusion ratified: ONE strategy, no top-level `SyncStrategy` enum; per-field conflict tier is the seam. | verified |
| `powersync-sdk-parity-plan.md` | Parity breadth reached (10/10 platforms). | verified |
| `complete-nostos-fully-wired-operational.md` | v0.1 is code-complete: real-PG default, predicates, snapshot-on-subscribe. | inferred |
| `nostos-reference-demo-app.md` | Produced `sdk/nostos_flutter/example` (restored by A1; its integration test passes). | inferred |
| `w4-packaging-fallback.md` | Spike record — proved the Flutter↔Rust packaging path before W4 was built. | inferred |
| `supabase-flutter-smoke-results.md` | Live-Supabase smoke report, 2026-07-12. Point-in-time result. | inferred |
| `test-coverage-gap-analysis.md` | Static coverage snapshot, 2026-07-13. No coverage tooling installed, so it cannot self-refresh. | inferred |
| `flutter-pomodoro-persona-e2e-baseline.md` | Flutter test fixtures under `fixtures/flutter/`. | inferred |

## SUPERSEDED — read the replacement instead

| plan | superseded by | why | basis |
|---|---|---|---|
| `HANDOFF.md` | `nostos-completion-assessment-2026-07-29.md` + the reading order in `CLAUDE.md` | Says "start here / the planning phase is done, execute the committed plans". That was true in July; following it now sends a fresh agent at a stale plan list. **Most actively misleading file in this directory.** | verified |
| `launch-readiness-gap-list.md` | `nostos-completion-assessment-2026-07-29.md` | Both answer "what stands between us and launch"; the 07-29 assessment is 17 days newer and column-splits engineering vs operator work. | inferred |
| `sdk-live-e2e-consolidation.md` | `sdk-parity-final-three.md`, then the 10/10 strict run | Its bar was **7/7** platforms. There are now ten, and all ten pass. | verified |

### Caveat on the Flutter connection redesign

`nostos-flutter-powersync-connection-redesign.md` is filed GATED-ON-GO, not SUPERSEDED, and the
distinction matters. The **bug diagnosis that motivated it was falsified**: "add does nothing"
was a `PgWriteBack` TEXT-vs-`TIMESTAMPTZ` bind, and "5 rows → 1 shows" was a config bug
(`NOSTOS_REPLICATOR != pg`, so the snapshotter was `None`). Neither is fixed by the redesign.
But the seven API-shape decisions in it were ratified separately on 2026-07-13, so the plan is
live **as an API proposal** and dead **as a bug fix**. Do not cite its problem statement.

---

Adding this index rather than stamping 23 individual headers was deliberate: one file to keep
accurate beats 23 that drift independently, and it avoids editing plans in place — mislabeling
a GATED-ON-GO plan as SUPERSEDED would destroy live strategy work. Three files did get an
inline header anyway, because opening them directly (without this index) leads you wrong:
`HANDOFF.md`, `sdk-live-e2e-consolidation.md`, and the connection redesign.
