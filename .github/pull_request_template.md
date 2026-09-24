<!--
Title: [arxa-<skill>] <what changed>. The first tag is the primary stage; add a
tag for each other stage touched (tag map below; SSOT: docs/ci/decisions.md).
-->

**Pipeline stage / agent:** `[arxa-…]` arxa-…

## What is now true

<!--
The WHOLE branch as it stands: regenerate from `git log origin/main..HEAD`
when the PR opens AND again before merge. A body left from an earlier
checkpoint is a defect.
-->

-

## Verified

- [ ] `scripts/check.sh <area>` green locally for every area this touches (`make check` runs them all)

---

<details>
<summary>Stage tag map (SSOT: docs/ci/decisions.md)</summary>

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

</details>

🤝 Collaborated with <model> via [Claude Code](https://claude.com/claude-code)
