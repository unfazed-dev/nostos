#!/usr/bin/env bash
# WorktreeCreate hook (.claude/settings.json): routes Claude Code's worktrees
# through `make worktree`, so agents land in .worktrees/<name> like humans do
# (without it Claude Code uses .claude/worktrees/). Contract: the JSON on stdin
# carries `name`; the last stdout line is the created path; everything else
# goes to stderr. Removal needs no hook: Claude Code runs `git worktree remove`
# on git worktrees itself.
set -euo pipefail

name=$(jq -r .name)
# The slug becomes a branch and a directory: refuse anything path-like.
[[ $name =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "worktree-create: bad name '$name'" >&2; exit 1; }

# The main clone, even when the session itself runs inside a worktree.
root=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
make -C "$root" worktree NAME="$name" >&2
echo "$root/.worktrees/$name"
