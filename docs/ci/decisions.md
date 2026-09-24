# CI/CD decisions — nostos (formerly cairn)

The decision record for the arxa-cicd adopt pass + the cairn → nostos rename.
One row per decision, in the order they were grilled. Name map:
`docs/plans/nostos-name-map-2026-09-24.md`.

| # | decision | answer | why |
|---|---|---|---|
| 0 | mode | **adopt** — `ci.yml` (7 jobs, 8 with the `commits` gate) + `release.yml` existed; there was no `scripts/check.sh`, `docs/ci/`, PR template, pre-push hook, worktree verbs | skill §0 |
| 1 | product name | **nostos** — crates `nostos-*`, npm `@nostos-sync/*`, pub `nostos_flutter`, domain `nostos.run`, brew `unfazed-dev/tap/nostos` | crates.io/npm/pub free for every slot we publish; bare `nostos` crate + `@nostos` npm org are someone else's |
| 2 | history rewrite | **full rename through history** (git-filter-repo: content, paths, commit messages; binaries skipped). It uses the same rule table as the forward rename script, holds included. A scripted fixup, driven by `.git/filter-repo/commit-map` and kept in-repo, updates the ~228 short hashes cited in 43 docs. The rewrite goes to a **new** repo `unfazed-dev/nostos`. `unfazed-dev/cairn` is renamed `cairn-archive` and archived untouched; a mirror clone + `git bundle --all` backup is taken first | 0 stars / 0 forks, so it's the cheapest it will ever be. The archive is the source of truth and the rollback. GitHub's `cairn.git` → `cairn-archive` redirect keeps arxa-studio's pub pin `fabc1a16` resolving. The new repo gives no force-push and retro PRs numbered #1…N. Never create a new `unfazed-dev/cairn`: it kills the redirect |
| 2a | commit identity | author + committer → `117146850+unfazed-dev@users.noreply.github.com` | the current `founders@cairn.dev` is on a domain someone else registered (2023, Cloudflare) — they could claim all 672 commits |
| 2b | hold list | every persisted or on-the-wire identifier stays `cairn*` until its own migration ADR: PG schema, RPCs, triggers, tables, publication, slot, role and db user; applied migrations, `direct.sql` and the SQL in `direct.rs`; Edge Function `cairn-push` and the `{cairn:'ring'}` payload; SQLite files and tables, OPFS pool, checkpoint keys; ALPN, Tauri plugin id and config key, BroadcastChannel, `X-Cairn-Source`; metric names and docker db credentials. `NOSTOS_*` env and `nostos`/`.nostos` config are read first, falling back to `CAIRN_*` and `cairn`/`.cairn`; release artifacts ship `cairn-*` binary symlinks | renaming these is a data migration, not a rename. It would break the live Supabase project, installed atlet builds, and unsynced phone outboxes. Needs zero code. Research: `docs/plans/nostos-rename-research-2026-09-24.md` |
| 2c | local layout | `/Volumes/business_ssd/arxa_digital_solutions/nostos` (fresh clone of the rewrite) sits beside `…/cairn-archive` (today's `cairn/`, renamed once the rewrite is verified) | no sibling repo hard-codes the local `cairn/` path (checked: only the URL routes `/__cairn/*` and the pubspec git deps). In-repo, only 2 raw bench logs cite it |
| 3b | retro grouping + tags | **one PR per active day** (36 days → #1…#38 in date order with the 3c tag splits); the 6 existing merges stay as sub-branches inside their day. The `[arxa-*]` tag comes from the files touched: `.github/` + `check.sh` + `Makefile` + `deny.toml` → cicd; `docs/adr\|plans\|research` + ARCHITECTURE/ROADMAP → designer; tests/benches/e2e/conformance → tester; Docker/fly/deploy/packaging → deployer; everything else → builder. The primary tag is the area with the most file touches; any area ≥20% of it is added. The script drafts titles into `scripts/nostos_rename/retro/titles.tsv`; Claude polishes them and the user signs off before any API call. The body is git-cliff-style output by commit type (decision 12), plus the date, the `Pipeline stage / agent:` field and a "What is now true" section. One more PR, #39, carries the post-rename formatter/regen commit and the hash-cite fixup + retro records | per-week (13) is too coarse and day×scope (181) too noisy; 353 commits have no scope. The day is the historical unit, and 39 PRs sit well inside the rate limits. Dry run (stub rules, 2026-09-24): primary 35 builder, 2 tester, 1 designer |
| 3c | old tags | `v0.1.0` and `v0.2.0`: the day's PR is split at the tagged commit, so the tag lands on a `main` merge commit with an identical tree (38 PRs total). The original name, tagger date and message are kept (cairn→nostos). Each gets a **notes-only** GitHub Release (git-cliff, tag to tag, marked "historical — no binaries"). Tags are pushed while Actions is off. `pre-ads-move-2026-09-02` is not carried over; it stays in `cairn-archive`. No `CHANGELOG.md` yet | releases sit on `main`. `release.yml` (triggered by `v*`) has never run and would build old code. The pre-ads-move tag was a disk-move snapshot, not a release. Add a CHANGELOG at the first real publish |
| 3a | restructure + cleanup | **after** the rename, as forward `[arxa-*]` PRs in `nostos`, in this order: (1) delete stale docs/dirs, (2) refresh docs, (3) break the `cli`→`push` dependency, (4) split `main.rs`, (5) split `transport.rs` with bench numbers. Backlog: research doc §8 | mixing them in would void the proof that the rename is mechanical (reverse-map diff + tree equality) and would break git's rename detection. Lowest risk first; the only change that could affect performance goes last, with measurements |
| 3 | retro PR convention | **A+R.** The rewritten `main` gets one `--no-ff` merge commit per feature group, carrying the historical date, an `[arxa-<skill>]` title and a git-cliff summary. The summaries are mirrored in `docs/ci/retro/*.md`, and each group also gets a real GitHub PR page (#1…N in the new repo). The PRs are replayed in order: open the PR, then advance `main` to its merge commit, which GitHub counts as an indirect merge. Actions are off during the replay, and creation is paced under 80/min | the user wants the summaries visible on GitHub. A fresh repo numbers the PRs cleanly. The API has no date field, so the historical date lives in the title, the body and the merge commit. PR pages are permanent |

## Defaulted while the user was away (2026-09-24) — review

The rows below were not grilled. Each takes the recommendation that came with the
question; change any of them and the generated files follow.

| # | decision | answer | why |
|---|---|---|---|
| 4 | `scripts/check.sh` areas | one area per `ci.yml` job (`lint-test`, `e2e-pg`, `deny`, `sdk-e2e`, `flutter`, `benchmark`, `sdk-typecheck`) plus `all`; an area whose toolchain or inputs are absent is skipped green | local green = CI green, by the same names; green by absence |
| 5 | PR-title check | `[arxa-<skill>]` checked **warn-first** (annotation, job stays green) | flip it to required once the retro PRs and the first cleanup PRs have gone through |
| 6 | commit gate | conventional prefix on `git rev-list --no-merges base..head`, **required** | GitHub's synthetic merge commit false-positives a naive check (energize PR #1, 2026-08-16) |
| 7 | workflow token | top-level `permissions: contents: read` in `ci.yml`; `release.yml` keeps its own | least privilege; nothing in CI writes to the repo |
| 8 | trunk + protection | repo is **public**, GitHub-hosted runners. Protection on `main` (required contexts = the `ci.yml` job names, no force-push, no deletion) is prepared in `docs/ci/setup.md` as a `gh api` call **for the user to run** after the retro replay | protection applied before the replay would refuse the replay's pushes to `main` |
| 9 | registries | npm scope `@nostos-sync`, pub `nostos_flutter`; nothing registered or published | mirrors today's names; claiming names needs the user |
| 10 | fallback lifetime | `CAIRN_*` env, `cairn*` config files and `cairn-*` binary symlinks stay until the first major (1.0), with a one-time deprecation warning | Parallel Change: contract only after every consumer has moved |
| 11 | CD | stays tag-triggered (`release.yml` on `v*`); no deploy on merge | there is no deploy target CI should own; fly deploys stay manual |
| 12 | retro summaries | a small Python generator in git-cliff's grouping (commit type → section) instead of git-cliff | git-cliff isn't installed; one script, no new tool |
| 13 | branching | worktrees (the arxa default) | skill default; the pre-push hook enforces it |
| 14 | retro base | the retro `main` starts at an empty root commit dated just before the first real commit, so every real commit lands through a PR | otherwise the first commit would sit on `main` outside any PR |

## Tag map (SSOT)

The one list of PR-title stage tags. A title starts with one or more tags,
the first being the primary stage: `[arxa-cicd][arxa-builder] <what changed>`.
Row 3b says which tag the files touched call for. `scripts/check.sh pr-title`
reads its allowlist from this table. `.github/pull_request_template.md`
carries an identical copy, and the same check warns when the two drift.

| tag | skill | stage |
|---|---|---|
| `[arxa-orchestrator]` | arxa-orchestrator | Ø: front door, project init, stage dispatch |
| `[arxa-intake]` | arxa-intake | 1: client requirements into validated intake answers |
| `[arxa-story-mapper]` | arxa-story-mapper | 0: Epic → Feature → Story map, the brief |
| `[arxa-moodboarder]` | arxa-moodboarder | 0: reference-app moodboard |
| `[arxa-designer]` | arxa-designer | 2: design; here ADRs, plans, research, ARCHITECTURE/ROADMAP |
| `[arxa-scaffolder]` | arxa-scaffolder | 3: frozen design into the per-surface file set |
| `[arxa-builder]` | arxa-builder | 4: implementation; here everything row 3b maps nowhere else |
| `[arxa-tester]` | arxa-tester | 5: tests, benches, e2e, conformance |
| `[arxa-reviewer]` | arxa-reviewer | 6: pre-release QC gate |
| `[arxa-deployer]` | arxa-deployer | 9: releases and deploys; here Docker, fly, deploy/, packaging/ |
| `[arxa-lens]` | arxa-lens | 8: screenshots and visual evidence |
| `[arxa-lint]` | arxa-lint | 7: docs-vs-code consistency |
| `[arxa-cicd]` | arxa-cicd | 10: CI/CD; here .github/, scripts/check.sh, Makefile, deny.toml |
