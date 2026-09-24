#!/usr/bin/env python3
"""Replay the retro PRs onto GitHub (docs/ci/decisions.md rows 3, 3c, 8). The USER runs this.

  replay.py --repo DIR                       # dry run: prints every command, touches nothing
  ARXA_ALLOW_MAIN_PUSH=1 replay.py --repo DIR --apply [--delete-branches]

DIR is the repo rewrite.py + hashfix.py produced; the plan is
DIR/.git/nostos-retro/replay.json. Per PR, in order: push `retro/NN-YYYY-MM-DD`
at the PR's last commit, `gh pr create` (base main), check GitHub numbered it
#NN, then fast-forward `main` to the PR's merge commit, which GitHub records as
an indirect merge. Then the tags (Actions is off, so release.yml stays quiet)
and the notes-only releases.

--apply refuses unless Actions is disabled on the repo and its `main` is the
empty root or one of the plan's merges (a rerun resumes after the last merge
on the remote). PR creation is paced under --per-min / --per-hour (GitHub's
secondary limits are 80/min and 500/hr for content-creating requests).
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

PLAN = Path(".git") / "nostos-retro" / "replay.json"


class Runner:
    def __init__(self, repo, apply, per_min, per_hour):
        self.repo, self.apply = str(repo), apply
        self.per_min, self.per_hour = per_min, per_hour
        self.writes: collections.deque[float] = collections.deque()

    def run(self, *cmd, write=False, check=True) -> str:
        """Prints the command; runs it only under --apply. `write` = paced GitHub write."""
        print("$ " + shlex.join(cmd))
        if not self.apply:
            return ""
        if write:
            self.pace()
        r = subprocess.run(cmd, cwd=self.repo, capture_output=True, text=True)
        if check and r.returncode:
            raise SystemExit(f"failed ({r.returncode}): {r.stderr.strip() or r.stdout.strip()}")
        return r.stdout if r.returncode == 0 else ""

    def pace(self):
        while True:
            now = time.time()
            while self.writes and now - self.writes[0] > 3600:
                self.writes.popleft()
            last_min = sum(now - t < 60 for t in self.writes)
            if last_min < self.per_min and len(self.writes) < self.per_hour:
                break
            time.sleep(5)
        self.writes.append(time.time())


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True, help="the rewritten repo (rewrite.py --work)")
    ap.add_argument("--gh-repo", default="unfazed-dev/nostos")
    ap.add_argument("--remote", default="origin", help="git remote that points at --gh-repo")
    ap.add_argument("--apply", action="store_true", help="really push and call the GitHub API")
    ap.add_argument("--delete-branches", action="store_true", help="delete the retro/* branches at the end")
    ap.add_argument("--per-min", type=int, default=60)
    ap.add_argument("--per-hour", type=int, default=450)
    ap.add_argument("--settle", type=float, default=3.0, help="seconds between pushing a branch and opening its PR")
    a = ap.parse_args(argv)
    repo = Path(a.repo).resolve()
    plan = json.loads((repo / PLAN).read_text())
    prs, root = plan["prs"], plan["root"]
    r = Runner(repo, a.apply, a.per_min, a.per_hour)
    print(f"# {'APPLY' if a.apply else 'DRY RUN (nothing is pushed, no API call)'}: "
          f"{len(prs)} PRs, {len(plan['tags'])} tags, {len(plan['releases'])} releases -> {a.gh_repo}")

    # ---- preflight
    if a.apply and os.environ.get("ARXA_ALLOW_MAIN_PUSH") != "1":
        raise SystemExit("refusing: pushes to main need ARXA_ALLOW_MAIN_PUSH=1")
    url = r.run("git", "remote", "get-url", a.remote).strip()
    if a.apply and a.gh_repo not in url:
        raise SystemExit(f"remote {a.remote} is {url}, not {a.gh_repo}")
    perms = r.run("gh", "api", f"repos/{a.gh_repo}/actions/permissions")
    if a.apply and json.loads(perms).get("enabled") is not False:
        raise SystemExit(f"refusing: Actions is enabled on {a.gh_repo}; disable it first (RETRO.md step 4)")
    head = r.run("git", "ls-remote", a.remote, "refs/heads/main").split("\t")[0]
    chain = [root] + [p["merge"] for p in prs]
    if a.apply and head not in chain:
        raise SystemExit(f"refusing: remote main is {head or 'missing'}, not the empty root {root[:10]} or a retro merge")
    done = chain.index(head) if a.apply else 0
    if done:
        print(f"# resuming: remote main is PR #{prs[done - 1]['n']}'s merge")
    # issues and PRs share one counter: anything else in the repo shifts every PR number
    top = r.run("gh", "api", f"repos/{a.gh_repo}/issues?state=all&per_page=1", "--jq", ".[0].number // 0").strip()
    want = prs[done - 1]["n"] if done else 0
    if a.apply and int(top or 0) not in (want, want + 1):
        raise SystemExit(f"refusing: the newest issue/PR is #{top}, the plan expects #{want}")

    # ---- the PRs
    for p in prs[done:]:
        r.run("git", "push", a.remote, f"{p['head']}:refs/heads/{p['branch']}")
        numbers = r.run("gh", "pr", "list", "--repo", a.gh_repo, "--head", p["branch"], "--state", "all",
                        "--json", "number", "--jq", ".[].number").split()
        if not numbers:
            if a.apply:
                time.sleep(a.settle)
            out = r.run("gh", "pr", "create", "--repo", a.gh_repo, "--base", "main", "--head", p["branch"],
                        "--title", p["title"], "--body-file", p["body"], write=True)
            numbers = re.findall(r"/pull/(\d+)", out)
        if a.apply and numbers[:1] != [str(p["n"])]:
            raise SystemExit(f"PR for {p['branch']} is {numbers}, the plan says #{p['n']}: numbering is off, stopping")
        number = p["n"]
        r.run("git", "push", a.remote, f"{p['merge']}:refs/heads/main")
        if a.apply:
            for _ in range(10):
                if r.run("gh", "pr", "view", str(number), "--repo", a.gh_repo, "--json", "state", "--jq", ".state").strip() == "MERGED":
                    break
                time.sleep(3)
            else:
                print(f"# warning: #{number} not shown as merged yet; GitHub may still be processing the push")

    # ---- tags + notes-only releases
    r.run("git", "push", a.remote, *[f"refs/tags/{t}" for t in plan["tags"]])
    for rel in plan["releases"]:
        if r.run("gh", "release", "view", rel["tag"], "--repo", a.gh_repo, "--json", "tagName", check=False):
            continue
        r.run("gh", "release", "create", rel["tag"], "--repo", a.gh_repo, "--verify-tag",
              "--title", rel["title"], "--notes-file", rel["notes"], write=True)

    if a.delete_branches:
        r.run("git", "push", a.remote, "--delete", *[p["branch"] for p in prs])
    print("# next (by hand, RETRO.md): re-enable Actions, then apply the main protection from docs/ci/setup.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
