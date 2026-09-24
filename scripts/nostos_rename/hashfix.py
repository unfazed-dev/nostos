#!/usr/bin/env python3
"""The last retro PR: post-rename style commit, hash-cite fixups + retro PR summaries.

  hashfix.py --work DIR [--style] [--date 2026-09-25T09:00:00+10:00] [--keep 224ccef fabc1a16]

`--style` first commits the worktree's uncommitted changes (postfix.sh's
formatter + uniffi regen output) as its own commit; without it a dirty
worktree is refused.

Runs on the repo rewrite.py produced, once, while `main` is still the retro
tip. Every >=7-hex token in a tracked text file that is the unambiguous prefix
of exactly one OLD commit becomes the same-length prefix of its FINAL commit
(composed map in .git/nostos-retro/commit-map). `--keep` prefixes stay as they
are: they cite the archive on purpose (arxa-studio's pins). Tokens that look
like a cite but resolve to nothing are listed in hashfix-report.txt.

The fixup commit also adds docs/ci/retro/NN-YYYY-MM-DD.md per retro PR and the
two notes-only release bodies. Both commits land as retro PR #N+1 (one --no-ff
merge, title + tags from titles.tsv's `-` row),
so every commit after the root still arrives through a PR. That PR's body
cites its own merge, so it lives outside the tree, beside replay.json, which
is everything replay.py needs.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import retro  # noqa: E402
import rewrite  # noqa: E402

RETRO_DIR = "docs/ci/retro"
KEEP = ("224ccef", "fabc1a16")
POST_SUBJECT = "docs(ci): retro PR summaries and final-history hash fixups"
STYLE_SUBJECT = "style: formatters and regenerated uniffi bindings after the nostos rename"


def text_files(work, commit) -> dict[str, tuple[str, bytes]]:
    """path -> (mode, content) for every regular file that is not binary."""
    entries = []
    for e in retro.gitb(work, "ls-tree", "-r", "-z", commit).split(b"\0"):
        if e:
            meta, path = e.split(b"\t", 1)
            mode, _typ, sha = meta.decode().split()
            if mode in ("100644", "100755"):
                entries.append((rewrite.dec(path), mode, sha))
    blobs = rewrite.cat_batch(work, sorted({sha for _, _, sha in entries}))
    return {p: (mode, blobs[sha]) for p, mode, sha in entries if b"\0" not in blobs[sha][:8192]}


def stamp(when: datetime.datetime) -> bytes:
    off = int(when.utcoffset().total_seconds() // 60)
    tz = b"%s%02d%02d" % (b"-" if off < 0 else b"+", abs(off) // 60, abs(off) % 60)
    return rewrite.ident(int(when.timestamp()), tz)


def history_commits(work, head, base, skip) -> list[retro.Commit]:
    shas = [s for s in retro.members(work, head, base) if s not in skip]
    recs = retro.commit_records(work, shas)
    return [recs[s] for s in shas]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--work", required=True, help="the repo rewrite.py produced")
    ap.add_argument("--style", action="store_true", help="commit the dirty worktree first (postfix.sh output)")
    ap.add_argument("--date", help="ISO 8601 date for the post-tip commit + merge (default: now)")
    ap.add_argument("--keep", nargs="*", default=list(KEEP), help="hash prefixes that cite the archive on purpose")
    a = ap.parse_args(argv)
    work = Path(a.work).resolve()
    base = work / rewrite.OUT
    meta = json.loads((base / "groups.json").read_text())
    groups, tip = meta["groups"], meta["retro_tip"]
    if retro.git(work, "rev-parse", "main").strip() != tip:
        raise SystemExit("main is not the retro tip: hashfix has run already, or the repo moved on")
    dirty = bool(retro.git(work, "status", "--porcelain").strip())
    if dirty != a.style:
        raise SystemExit(f"{work}: --style needs uncommitted changes to commit" if a.style else f"{work} has uncommitted changes")
    when = datetime.datetime.fromisoformat(a.date) if a.date else datetime.datetime.now().astimezone()
    if when.tzinfo is None:
        raise SystemExit("--date needs a UTC offset")
    post = meta.get("post") or {"tags": retro.parse_prefix(retro.POST_TAGS), "title": retro.POST_TITLE}
    who = stamp(when)
    base_commit = tip
    if a.style:  # 0. postfix.sh's output, committed as is
        with tempfile.TemporaryDirectory() as d:
            env = {**os.environ, "GIT_INDEX_FILE": str(Path(d) / "index")}
            subprocess.run(["git", "-C", str(work), "read-tree", tip], env=env, check=True)
            subprocess.run(["git", "-C", str(work), "add", "-A"], env=env, check=True)
            style_tree = subprocess.run(["git", "-C", str(work), "write-tree"], env=env, check=True,
                                        capture_output=True, text=True).stdout.strip()
        base_commit = rewrite.write_obj(work, "commit", [(b"tree", style_tree.encode()), (b"parent", tip.encode()),
                                                         (b"author", who), (b"committer", who)], rewrite.enc(STYLE_SUBJECT + "\n"))

    # 1. cites in the tip's text files
    cmap = dict(line.split() for line in (base / "commit-map").read_text().splitlines()[1:])
    cites = retro.Cites(list(cmap), cmap.get, keep=a.keep)
    changed: dict[str, tuple[str, bytes]] = {}
    files = text_files(work, base_commit)
    for path, (mode, data) in files.items():
        new = cites.sub(data, path)
        if new != data:
            assert len(new) == len(data), path  # same-length swaps only
            changed[path] = (mode, new)

    # 2. retro PR summaries + release bodies
    docs: dict[str, str] = {}
    for g in groups:
        recs = retro.commit_records(work, g["members"])
        commits = [recs[s] for s in g["members"]]
        docs[f"{RETRO_DIR}/{retro.doc_name(g['n'], g['date'])}"] = retro.group_md(
            g["n"], g["tags"], g["date"], g["title"], commits, g["merge"], retro.branch_name(g["n"], g["date"]), g["tag"])
    retro_merges = {g["merge"] for g in groups} | {meta["root"]}
    releases, prev_merge, prev_n, prev_tag = [], None, 0, None
    for name, t in meta["tags"].items():
        n_tag = next(g["n"] for g in groups if g["tag"] == name)
        commits = history_commits(work, t["merge"], prev_merge, retro_merges)
        if "tagger_ts" in t:
            tz = t["tagger_tz"]
            off = datetime.timedelta(hours=int(tz[1:3]), minutes=int(tz[3:])) * (-1 if tz[0] == "-" else 1)
            tag_date = datetime.datetime.fromtimestamp(t["tagger_ts"], datetime.timezone(off)).date().isoformat()
        else:  # lightweight tag: the merge's date
            tag_date = retro.git(work, "log", "-1", "--format=%cs", t["merge"]).strip()
        path = f"{RETRO_DIR}/release-{name}.md"
        docs[path] = retro.release_md(name, tag_date, t["message"], prev_tag, list(range(prev_n + 1, n_tag + 1)), commits)
        releases.append({"tag": name, "title": f"{name} (historical — no binaries)", "notes_in_tree": path})
        prev_merge, prev_n, prev_tag = t["merge"], n_tag, name
    docs[f"{RETRO_DIR}/commit-map.tsv"] = (  # the archive's cites resolve here after the rename
        "# cairn-archive commit\tnostos commit (composed through the rename + retro rewrite)\n"
        + "".join(f"{o}\t{f}\n" for o, f in cmap.items()))
    clash = sorted(set(docs) & set(files))
    if clash:
        raise SystemExit(f"generated docs would overwrite tracked files: {clash}")

    # 3. tree = tip + fixups + docs, through a scratch index
    with tempfile.TemporaryDirectory() as d:
        env = {**os.environ, "GIT_INDEX_FILE": str(Path(d) / "index")}
        run = lambda *x, **kw: subprocess.run(["git", "-C", str(work), *x], env=env, check=True, capture_output=True, **kw)
        run("read-tree", base_commit)
        lines = []
        for path, (mode, data) in [*changed.items(), *((p, ("100644", rewrite.enc(t))) for p, t in docs.items())]:
            sha = retro.gitb(work, "hash-object", "-w", "--stdin", input=data).decode().strip()
            lines.append(f"{mode} {sha}\t{path}")
        run("update-index", "-z", "--index-info", input=rewrite.enc("\0".join(lines) + "\0"))
        tree = run("write-tree").stdout.decode().strip()

    body = (f"{len(cites.done)} commit-hash cites in {len({w for w, _, _ in cites.done})} files now name the nostos "
            f"history (old -> final through the rewrite's commit map); cites of the archive "
            f"({', '.join(a.keep)}) stay. Adds {RETRO_DIR}/: one summary per retro PR, the notes-only release bodies "
            f"and commit-map.tsv (archive commit -> nostos commit).\n")
    fix = rewrite.write_obj(work, "commit", [(b"tree", tree.encode()), (b"parent", base_commit.encode()),
                                             (b"author", who), (b"committer", who)], rewrite.enc(f"{POST_SUBJECT}\n\n{body}"))
    n, date = groups[-1]["n"] + 1, when.date().isoformat()
    mine = [base_commit, fix] if a.style else [fix]
    recs = retro.commit_records(work, mine)
    commits = [recs[s] for s in mine]
    merge = rewrite.write_obj(work, "commit", [(b"tree", tree.encode()), (b"parent", tip.encode()), (b"parent", fix.encode()),
                                               (b"author", who), (b"committer", who)],
                              rewrite.enc(retro.merge_message(n, post["tags"], date, post["title"], commits)))
    retro.git(work, "update-ref", "refs/heads/main", merge, tip)
    retro.git(work, "reset", "--quiet", "--hard", "main")

    # 4. the replay plan: PR bodies are the in-tree docs, plus #n's own
    bodies = base / "bodies"
    bodies.mkdir(exist_ok=True)
    prs = []
    for g in groups:
        f = bodies / retro.doc_name(g["n"], g["date"])
        f.write_text(docs[f"{RETRO_DIR}/{retro.doc_name(g['n'], g['date'])}"], encoding="utf-8")
        prs.append({"n": g["n"], "branch": retro.branch_name(g["n"], g["date"]), "head": g["head"], "merge": g["merge"],
                    "title": retro.pr_title(g["tags"], g["date"], g["title"]), "body": str(f.relative_to(work))})
    f = bodies / retro.doc_name(n, date)
    f.write_text(retro.group_md(n, post["tags"], date, post["title"], commits, merge, retro.branch_name(n, date)), encoding="utf-8")
    prs.append({"n": n, "branch": retro.branch_name(n, date), "head": fix, "merge": merge,
                "title": retro.pr_title(post["tags"], date, post["title"]), "body": str(f.relative_to(work))})
    for r in releases:
        r["notes"] = str((bodies / Path(r["notes_in_tree"]).name).relative_to(work))
        (work / r["notes"]).write_text(docs[r["notes_in_tree"]], encoding="utf-8")
    (base / "replay.json").write_text(json.dumps(
        {"root": meta["root"], "prs": prs, "tags": list(meta["tags"]), "releases": releases}, indent=1))

    left = collections.defaultdict(set)
    source = meta.get("source")
    for w, t, why in cites.skipped:
        if why == "unresolved" and source and Path(source).exists() and subprocess.run(
                ["git", "-C", source, "cat-file", "-e", f"{t}^{{commit}}"], capture_output=True).returncode == 0:
            why = "off-main"  # a commit in the source, but not on main (unreachable or another branch)
        left[(t, why)].add(w)
    report = [f"rewritten {len(cites.done)} cites in {len({w for w, _, _ in cites.done})} files "
              f"({sum(w.endswith('.md') for w, _, _ in cites.done)} in .md)"]
    report += [f"  {w}\t{t} -> {x}" for w, t, x in cites.done]
    report += [f"left {len(cites.skipped)} (token, why, files) — review: a real cite here names a commit outside main"]
    report += [f"  {t}\t{why}\t{len(ws)}\t{', '.join(sorted(ws)[:3])}{' …' if len(ws) > 3 else ''}"
               for (t, why), ws in sorted(left.items(), key=lambda kv: (kv[0][1], kv[0][0]))]
    (base / "hashfix-report.txt").write_text("\n".join(report) + "\n")
    print(report[0])
    print(f"left {len(cites.skipped)} tokens ({len(left)} distinct): see {base / 'hashfix-report.txt'}")
    print(f"docs: {len(docs)} files in {RETRO_DIR}/; post-tip PR #{n}: {len(mine)} commit(s) {' '.join(c[:10] for c in mine)}, merge {merge[:10]}")
    print(f"replay plan: {base / 'replay.json'} ({len(prs)} PRs, {len(releases)} releases)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
