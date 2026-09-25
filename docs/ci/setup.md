# CI setup — nostos

How the pipeline runs and how a change gets to `main`. The decisions behind it
are in [`decisions.md`](decisions.md); the plain-language version is
[`explainer.html`](explainer.html).

## Runners

GitHub-hosted only. The repo is public, and a self-hosted runner would run any
stranger's PR code on your machine (decision row 8). Every job runs on
`ubuntu-latest` except `flutter`, which runs on `macos-latest` because macOS is
the only host the Flutter glue crate is verified on (see the job's comment in
`ci.yml`). There is nothing to register or wake up. A job stuck in "queued" is
waiting for GitHub capacity. Changing code never fixes that.

| workflow | trigger | jobs |
|---|---|---|
| `ci.yml` | push to `main`, PR into `main` | `commits`, `lint-test`, `e2e-pg`, `deny`, `sdk-e2e`, `flutter`, `benchmark`, `sdk-typecheck` |
| `pr.yml` | PR opened, edited, synchronized, reopened | `pr-title` (required, row 5) |
| `release.yml` | `v*` tag | release builds. There is no deploy on merge (row 11) |

## Once per clone

```sh
make hooks    # core.hooksPath = scripts/hooks; the pre-push hook refuses pushes to main
```

`core.hooksPath` lives in `.git/config`, which git does not version, so every
fresh clone needs this once. All worktrees of a clone share it.

## The solo dev loop

1. `git fetch origin && make worktree NAME=<task>` creates `.worktrees/<task>`
   on branch `<task>`, from `origin/main`. Agents use `EnterWorktree`, which the
   `WorktreeCreate` hook in `.claude/settings.json` (`scripts/worktree-create.sh`)
   routes through the same verb. Never check out a branch in the main clone.
2. Work and commit in the worktree. Subjects are single-line with a
   conventional prefix: `feat:`, `fix(scope):`, `docs:`, `bench:`, and so on.
3. `scripts/check.sh <area>` for what you touched, or `make check` for
   everything. Each area runs the steps of the CI job with the same name, so
   local green = CI green. An area whose toolchain is missing is skipped green
   with a note (row 4). CI always has the toolchains.
4. `git push -u origin <task>`.
5. `gh pr create`. The title is `[arxa-<skill>] <what changed>` (tag map in
   `decisions.md`). The body comes from `.github/pull_request_template.md`.
6. `gh pr checks <n> --watch`. If a check goes red, reproduce it with
   `scripts/check.sh <area>` and fix it on the branch.
7. Once green, merge on GitHub. A merge commit keeps the branch's conventional
   subjects on `main`; a squash merge would put the `[arxa-…]` title there
   instead. Then, from the main clone:
   `git pull --ff-only && make worktree-rm NAME=<task>`.

## Branch protection on `main` (you run this AFTER the retro replay)

Run it once the retro replay (rows 3 and 14) has finished. Until then,
protection would refuse the replay's pushes to `main` (row 8).

```sh
gh api 'repos/{owner}/{repo}/branches/main/protection' --method PUT \
  -H "Accept: application/vnd.github+json" --input - <<'JSON'
{
  "required_status_checks": {
    "strict": false,
    "contexts": [
      "conventional commit subjects",
      "fmt + clippy + test",
      "real-Postgres logical-replication e2e",
      "cargo-deny (licenses, advisories, bans)",
      "SDK live-replication e2e (host slices)",
      "nostos_flutter — analyze + test",
      "throughput benchmark (smoke)",
      "nostos_react_native — typecheck",
      "nostos_capacitor — typecheck"
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
```

- `gh` fills in `{owner}/{repo}` from this clone's remote.
- `strict` is off (2026-09-24). With it on, every merge puts the other open PRs
  behind `main`, and each needs an update plus a full CI rerun before it can merge.
  For a solo repo that serialises every merge.
- The contexts are the check names GitHub reports: each `ci.yml` job's `name:`,
  once per matrix entry for `sdk-typecheck`.
- `PR title stage tag` (pr.yml) is required since row 5 flipped (2026-09-26):
  add it to `contexts` and re-run the PUT.
- When you rename a job, edit this list and re-run the PUT. A stale context
  blocks every merge.
- Check what is applied:
  `gh api 'repos/{owner}/{repo}/branches/main/protection' --jq '.required_status_checks.contexts'`.

## Escape hatch: pushing `main`

`scripts/hooks/pre-push` refuses every push to `main`, because `main` moves
only when a PR is merged on GitHub. The one exception is the one-time retro
replay (row 3), which pushes `main` forward to each PR's merge commit:

```sh
ARXA_ALLOW_MAIN_PUSH=1 git push origin main
```

Use it for the replay and nothing else.
