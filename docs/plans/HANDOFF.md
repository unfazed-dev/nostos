> **SUPERSEDED (2026-07-30) — do NOT start here.** This was the entrypoint in July 2026;
> its "execute the committed plans task-by-task" list is stale. Current front door:
> [`nostos-completion-assessment-2026-07-29.md`](nostos-completion-assessment-2026-07-29.md)
> for project state, the reading order in `CLAUDE.md` for orientation, and
> [`README.md`](README.md) for which plans in this directory are still live.
> Kept for the historical record of the planning→implementation transition.

# Implementation Handoff — start here <sub>(historical)</sub>

You are the **implementation agent** for nostos. The planning phase is done; your job is to execute the committed plans task-by-task. Do not re-plan, do not re-litigate ADRs (architectural changes need a new ADR or an addendum, flagged to the operator). The human operator oversees only — nothing you do may publish, deploy, spend money, or leave this repository's directory tree.

## Ground rules (non-negotiable)

1. **Everything happens inside this repo** (`/Volumes/developer_ssd/Developer/nostos`). Flutter apps included — they are fixtures under `fixtures/flutter/`. Never create files outside the tree.
2. **Execution method:** superpowers:executing-plans (inline) or superpowers:subagent-driven-development (fresh subagent per task). Tick the `- [ ]` checkboxes in the plan files as you complete steps; commit per task.
3. **Commits:** single line, conventional prefix (`feat:`/`fix:`/`test:`/`docs:`/`chore:`), no author mentions, no trailers.
4. **Verification gates:** `make ci` after every task (Rust); `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg` for replication tasks (without `NOSTOS_E2E_PG=1` the real-PG tests self-skip and report a false-positive pass; compose file: `docker/docker-compose.yml`); `flutter test` / `flutter test integration_test -d macos` for fixtures. Report real output, never "should pass".
5. **Perf changes** ship with before/after numbers or get reverted (Tier-5 precedent in `docs/ROADMAP.md`). Deliberate shortcuts carry `ponytail:` comments naming the ceiling.

## The plans (execute in this order)

| # | Plan | Scope | Status |
|---|------|-------|--------|
| 1 | `docs/plans/complete-nostos-fully-wired-operational.md` **Phase A** (Tasks A1–A6) | Agent operating layer: real CLAUDE.md project memory + AGENTS.md, `.claude/settings.json` permissions, four personas, verify-nostos skill, cargo-deny/CONTRIBUTING/editorconfig, docs truth sweep + competitive repositioning | Do FIRST — it makes every later session (including yours) faster. Note: root `CLAUDE.md` is currently untracked; Task A1 modifies and commits it. |
| 2 | `docs/plans/flutter-pomodoro-persona-e2e-baseline.md` (Parts I + II) | Pomodoro fixture (personas + smoke + E2E) and todo fixture (Supabase auth dual-mode smoke, **mock mode only for now**) | Independent of product phases; may run in parallel with #3 (worktree or second agent) |
| 3 | `docs/plans/complete-nostos-fully-wired-operational.md` **Phases B→F** | B real-PG default + snapshot → C predicate wire + WS batching → D write-back v1 → E browser transport → F v0.1 gate | Strictly in phase order; wire-touching tasks C1→C3→D2 land sequentially, never parallel |

Use the personas created in Phase A (`pg-integrator` for B, `bench-runner` for C3, `domain-guardian` on every `nostos-domain`/`nostos-application` merge, `docs-curator` for A6/F2).

## Blocked on the operator (park these, don't improvise)

- **Todo fixture live mode:** needs Supabase project + keys in `fixtures/flutter/todo/env.json` (Part II "Operator handoff" checklist in the fixtures plan). Mock mode is fully executable today.
- **Task B3 (CI e2e job):** validating the GitHub Actions job requires a push to a remote — confirm one exists (`git remote -v`); if not, implement the job file and flag validation as pending.
- **Task F2:** launch materials are drafts only; tagging beyond a local `v0.1.0` and any publishing is the operator's call.
- **Design decision open to veto:** write-back v1 rides the sync WebSocket (ADR-0013 addendum, plan Part V) — the operator may override to HTTP POST; check before starting Phase D if any doubt.

## Environment facts (verified 2026-07-04, don't re-derive)

- Rust 1.95.0 pinned (`rust-toolchain.toml`); `make ci` = fmt-check + clippy `-D warnings` + workspace tests; ~188 tests green at handoff.
- Flutter 3.44.0 stable via fvm (`~/fvm/default/bin/flutter`); `patrol_cli 4.4.0` installed; macOS desktop target builds on this machine (app-example precedent).
- Postgres e2e env convention: `NOSTOS_PG_URL` (see `crates/nostos-infra/tests/e2e_pg_replication.rs:43`).
- Advisor tool: `~/.agents/skills/consultant/scripts/consult.sh --domain <architecture|testing|…> --question "…"` — consult before substantive work and before declaring done (operator convention).

## State at handoff

- Branch `main`, clean tree except untracked root `CLAUDE.md` (handled by Task A1).
- Last planning commits: `598fff0` (master plan) → `7ba06e1` (advisor amendments) → `97fd09e` (fixtures plan) → `0b6be48` (todo fixture Part II).
- Nothing from any plan has been implemented yet. Zero fixture code exists; `fixtures/` does not exist yet.
- Tech-lead assessment of the codebase (what's real vs missing, with evidence) is Part I of the master plan — read it before touching code.

## Definition of done for this handoff

Phase A complete + fixtures plan complete (mock mode) + master plan Phases B–D complete, all gates green, plan checkboxes ticked, per-task commits on `main`. Phases E–F and live-mode Supabase follow operator input. When you believe a plan is complete: run its full verification ladder, consult the advisor, then report with evidence.
