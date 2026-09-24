#!/usr/bin/env python3
"""Rewrite the cairn history into the nostos history (docs/ci/decisions.md
rows 2, 2a, 3, 3b, 3c, 14). Never touches --source; works on a clone it makes.

  rewrite.py --source PATH --work NEW_DIR --rules scripts/nostos_rename/rules.py
             [--titles scripts/nostos_rename/retro/titles.tsv] [--tags v0.1.0 v0.2.0]
  rewrite.py --source PATH --work DIR --rules ... --verify-only

Stage 1, git-filter-repo on a fresh `--no-local` clone of `main` + the kept tags
(so `pre-ads-move-2026-09-02` and every other branch never enter the new repo):
paths via rename_path, blobs via rename_content(ORIGINAL path, data) cached on
(blob id, original path, mode), symlink targets via rename_path (as apply.py
does), commit + tag messages via rename_text, every
author/committer/tagger -> noreply. Runs with --preserve-commit-hashes and
never prunes, so the old -> stage-1 map is 1:1 and cites are translated once.

Stage 2, raw objects through `git hash-object -w` (exact bytes, git validates
each one): an empty root dated 60 s before the first commit, then every commit
re-parented in topo order. A group's first commit gets the previous group's
merge (or the root) as first parent, which is tree-equal to its old parent, so
every diff is unchanged. After a group's last commit comes its --no-ff merge:
parents (previous merge, last commit), the last commit's tree, noreply, the
last commit's committer date, `[arxa-*] <date> — <title> (#N)` + summary. Commit
cites in messages go old -> final here. Kept tags are recreated on the merge
that closes their group (same tree as the tagged commit), with the original
tagger date and the renamed message.

Outputs in <work>/.git/nostos-retro/: commit-map (old final, composed),
groups.json (read by hashfix.py), message-cites.txt, verify.txt.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import retro  # noqa: E402

OUT = Path(".git") / "nostos-retro"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
IDENT = re.compile(rb"^(?P<who>.*) (?P<ts>\d+) (?P<tz>[+-]\d{4})$")
NOREPLY = f"{retro.NOREPLY_NAME} <{retro.NOREPLY_EMAIL}>".encode()
ROOT_MESSAGE = b"chore: empty root \xe2\x80\x94 every commit after this one lands through a retro PR\n"


def enc(s: str) -> bytes:
    return s.encode("utf-8", "surrogateescape")


def dec(b: bytes) -> str:
    return b.decode("utf-8", "surrogateescape")


# ---- raw objects ------------------------------------------------------------


def cat_batch(repo, names) -> dict[str, bytes]:
    out = retro.gitb(repo, "cat-file", "--batch", input=("\n".join(names) + "\n").encode())
    objs, pos = {}, 0
    for name in names:
        nl = out.index(b"\n", pos)
        sha, _typ, size = out[pos:nl].split()
        size = int(size)
        objs[name] = out[nl + 1 : nl + 1 + size]
        pos = nl + 1 + size + 1
        assert sha
    return objs


def parse_obj(raw: bytes) -> tuple[list[tuple[bytes, bytes]], bytes]:
    head, _, msg = raw.partition(b"\n\n")
    headers: list[tuple[bytes, bytes]] = []
    for line in head.split(b"\n"):
        if line.startswith(b" "):
            k, v = headers[-1]
            headers[-1] = (k, v + b"\n" + line)
        else:
            k, _, v = line.partition(b" ")
            headers.append((k, v))
    return headers, msg


def hv(headers, key) -> bytes:
    return next(v for k, v in headers if k == key)


def write_obj(repo, typ, headers, msg) -> str:
    raw = b"\n".join(k + b" " + v for k, v in headers) + b"\n\n" + msg
    return retro.gitb(repo, "hash-object", "-t", typ, "-w", "--stdin", input=raw).decode().strip()


def ident(ts: int, tz: bytes) -> bytes:
    return NOREPLY + b" %d %s" % (ts, tz)


# ---- stage 0: the clone -----------------------------------------------------


def clone(source, work, ref, tags):
    if Path(work).exists():
        raise SystemExit(f"{work} exists; the rewrite runs on a clone it makes itself")
    subprocess.run(
        ["git", "clone", "--quiet", "--no-local", "--single-branch", "--branch", ref, "--no-tags", str(source), str(work)],
        check=True,
    )
    if tags:
        retro.git(work, "fetch", "--quiet", "--no-tags", "origin", *[f"refs/tags/{t}:refs/tags/{t}" for t in tags])
    if ref != "main":
        retro.git(work, "branch", "-m", ref, "main")


# ---- stage 1: git-filter-repo -----------------------------------------------


def renamed_blob(rules, mode: str, path: str, data: bytes) -> bytes:
    """A symlink's target is a path (apply.py and the README callback agree)."""
    return enc(rules.rename_path(dec(data))) if mode == "120000" else rules.rename_content(path, data)


def stage1(work, rules):
    import git_filter_repo as fr

    blobs: dict[tuple[bytes, bytes], bytes] = {}

    def file_info(filename, mode, blob_id, value):
        new_name = enc(rules.rename_path(dec(filename)))
        if mode == b"160000":  # gitlink: the id is a commit, not a blob
            return new_name, mode, blob_id
        key = (blob_id, filename, mode)  # rules depend on the path: never key on the blob alone
        if key not in blobs:
            data = value.get_contents_by_identifier(blob_id)
            assert data is not None, (filename, blob_id)
            new = renamed_blob(rules, mode.decode(), dec(filename), data)
            blobs[key] = blob_id if new == data else value.insert_file_with_contents(new)
        return new_name, mode, blobs[key]

    def commit_cb(commit, _metadata):
        # file_info only sees modifications; deletions must name the renamed path too.
        for change in commit.file_changes:
            if change.type == b"D":
                change.filename = enc(rules.rename_path(dec(change.filename)))

    args = fr.FilteringOptions.parse_args(
        # --force: the only repo this runs on is the fresh clone made above.
        ["--force", "--quiet", "--prune-empty", "never", "--prune-degenerate", "never", "--preserve-commit-hashes"],
        error_on_empty=False,
    )
    cwd = os.getcwd()
    os.chdir(work)
    try:
        fr.RepoFilter(
            args,
            file_info_callback=file_info,
            commit_callback=commit_cb,
            message_callback=lambda m: enc(rules.rename_text(dec(m))),
            name_callback=lambda _n: enc(retro.NOREPLY_NAME),
            email_callback=lambda _e: enc(retro.NOREPLY_EMAIL),
        ).run()
    finally:
        os.chdir(cwd)
    pairs = [line.split() for line in (Path(work) / ".git/filter-repo/commit-map").read_text().splitlines()[1:]]
    return {old: new for old, new in pairs}


# ---- stage 2: topology replay -------------------------------------------------


def stage2(work, groups, titles, s1_of, tags, rules):
    old_of = {v: k for k, v in s1_of.items()}
    order = retro.git(work, "rev-list", "--topo-order", "--reverse", "main").split()
    raw = cat_batch(work, order)
    first_of = {s1_of[g.first]: i for i, g in enumerate(groups)}
    last_of = {s1_of[g.last]: i for i, g in enumerate(groups)}
    final: dict[str, str] = {}
    cites = retro.Cites(list(s1_of), lambda old: final.get(s1_of[old]))

    h0, _ = parse_obj(raw[s1_of[groups[0].first]])
    stamps = [IDENT.match(hv(h0, k)) for k in (b"author", b"committer")]
    root_ts = min(int(m["ts"]) for m in stamps) - 60
    root_id = ident(root_ts, stamps[1]["tz"])
    retro.gitb(work, "hash-object", "-t", "tree", "-w", "--stdin", input=b"")
    root = write_obj(work, "commit", [(b"tree", EMPTY_TREE.encode()), (b"author", root_id), (b"committer", root_id)], ROOT_MESSAGE)

    anchor, out_groups = root, []
    for c in order:
        headers, msg = parse_obj(raw[c])
        parents = [v.decode() for k, v in headers if k == b"parent"]
        rest = [(k, v) for k, v in headers if k not in (b"tree", b"parent")]
        if c in first_of:
            i = first_of[c]
            want = s1_of[groups[i - 1].last] if i else None
            if (parents[0] if parents else None) != want:
                raise SystemExit(f"group {i + 1} first commit {c} does not follow group {i}'s last")
            new_parents = [anchor] + [final[p] for p in parents[1:]]
        else:
            new_parents = [final[p] for p in parents]
        new_headers = [(b"tree", hv(headers, b"tree"))] + [(b"parent", p.encode()) for p in new_parents] + rest
        final[c] = write_obj(work, "commit", new_headers, cites.sub(msg, old_of[c][:12]))

        if c in last_of:
            g, t = groups[last_of[c]], titles[last_of[c]]
            tag_list = retro.parse_prefix(t["tags"])
            title = rules.rename_text(t["title"])
            mem = retro.members(work, final[c], anchor)
            recs = retro.commit_records(work, mem)
            commits = [recs[s] for s in mem]
            when = hv(headers, b"committer")
            m = IDENT.match(when)
            mid = ident(int(m["ts"]), m["tz"])
            message = retro.merge_message(g.n, tag_list, g.date, title, commits, g.tag)
            merge = write_obj(
                work,
                "commit",
                [(b"tree", hv(headers, b"tree")), (b"parent", anchor.encode()), (b"parent", final[c].encode()),
                 (b"author", mid), (b"committer", mid)],
                enc(message),
            )
            out_groups.append({
                "n": g.n, "date": g.date, "tags": tag_list, "title": title, "tag": g.tag,
                "base": anchor, "head": final[c], "merge": merge, "members": mem,
                "old_first": g.first, "old_last": g.last,
            })
            anchor = merge

    tag_out = {}
    for name in tags:
        ref = f"refs/tags/{name}"
        s1_tag = retro.git(work, "rev-parse", ref).strip()
        g = next(x for x in out_groups if x["tag"] == name)
        if retro.git(work, "cat-file", "-t", s1_tag).strip() == "tag":
            headers, msg = parse_obj(cat_batch(work, [s1_tag])[s1_tag])
            msg = cites.sub(msg, f"tag {name}")
            new = [(b"object", g["merge"].encode())] + [(k, v) for k, v in headers if k != b"object"]
            obj = write_obj(work, "tag", new, msg)
            tagger = IDENT.match(hv(headers, b"tagger"))
            tag_out[name] = {"object": obj, "merge": g["merge"], "old_target": g["old_last"],
                             "tagger_ts": int(tagger["ts"]), "tagger_tz": tagger["tz"].decode(),
                             "message": dec(msg)}
        else:
            obj = g["merge"]
            tag_out[name] = {"object": obj, "merge": obj, "old_target": g["old_last"], "message": ""}
        retro.git(work, "update-ref", ref, obj)

    retro.git(work, "update-ref", "refs/heads/main", anchor)
    keep = {"refs/heads/main", *[f"refs/tags/{t}" for t in tags]}
    for ref in retro.git(work, "for-each-ref", "--format=%(refname)").split():
        if ref not in keep:
            retro.git(work, "update-ref", "-d", ref)
    retro.git(work, "reset", "--quiet", "--hard", "main")
    retro.git(work, "reflog", "expire", "--expire=now", "--all")
    retro.git(work, "gc", "--quiet", "--prune=now")
    return root, final, out_groups, tag_out, cites


# ---- verification -------------------------------------------------------------


def blob_sha(data: bytes) -> str:
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def forward_tree(source, tip, rules, work) -> str:
    """The tip tree the forward rename produces: apply.py's plan (its own tree
    walk, symlink and collision handling) with these rules, hashed without
    filter-repo."""
    import apply

    apply.rename_path, apply.rename_content = rules.rename_path, rules.rename_content
    cwd = os.getcwd()
    os.chdir(source)  # apply.tracked() reads a ref from the current repo
    try:
        planned = apply.plan(tip)
    finally:
        os.chdir(cwd)
    lines = [f"{mode} {blob_sha(body)}\t{new}" for mode, _old, new, body in planned]
    with tempfile.TemporaryDirectory() as d:
        env = {**os.environ, "GIT_INDEX_FILE": str(Path(d) / "index")}
        run = lambda *a, **kw: subprocess.run(["git", "-C", str(work), *a], env=env, check=True, capture_output=True, **kw)
        run("update-index", "-z", "--index-info", input=enc("\0".join(lines) + "\0"))
        return run("write-tree", "--missing-ok").stdout.decode().strip()


def ls_tree(repo, commit) -> set[tuple[str, str, str]]:
    raw = retro.gitb(repo, "ls-tree", "-r", "-z", "--full-tree", commit)
    out = set()
    for entry in raw.split(b"\0"):
        if entry:
            meta, path = entry.split(b"\t", 1)
            mode, _typ, sha = meta.decode().split()
            out.add((mode, sha, dec(path)))
    return out


def deep_trees(source, work, rules, cmap) -> list[str]:
    """Every rewritten tree == the rules applied to its original tree."""
    olds = list(cmap)
    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        old_trees = dict(zip(olds, pool.map(lambda c: ls_tree(source, c), olds)))
        new_trees = dict(zip(olds, pool.map(lambda c: ls_tree(work, cmap[c]), olds)))
    triples = sorted({t for tree in old_trees.values() for t in tree if t[0] != "160000"})
    contents = cat_batch(source, sorted({sha for _, sha, _ in triples}))
    renamed = {(mode, sha, path): blob_sha(renamed_blob(rules, mode, path, contents[sha])) for mode, sha, path in triples}
    bad = []
    for c in olds:
        want = {(m, s if m == "160000" else renamed[(m, s, p)], rules.rename_path(p)) for m, s, p in old_trees[c]}
        if want != new_trees[c]:
            bad.append(f"tree differs at {c[:12]} ({len(want ^ new_trees[c])} entries)")
    return bad


def verify(source, work, rules, deep=True) -> tuple[list[str], list[str]]:
    base = Path(work) / OUT
    meta = json.loads((base / "groups.json").read_text())
    cmap = dict(line.split() for line in (base / "commit-map").read_text().splitlines()[1:])
    groups, root, tags = meta["groups"], meta["root"], meta["tags"]
    problems, facts = [], []

    fsck = subprocess.run(["git", "-C", str(work), "fsck", "--full", "--no-dangling"], capture_output=True, text=True)
    facts.append(f"fsck exit {fsck.returncode}, {len(fsck.stdout.splitlines()) + len(fsck.stderr.splitlines())} lines")
    if fsck.returncode or fsck.stdout.strip():
        problems.append(f"git fsck: {fsck.stdout.strip() or fsck.stderr.strip()}")

    refs = set(retro.git(work, "for-each-ref", "--format=%(refname)").split())
    want_refs = {"refs/heads/main", *[f"refs/tags/{t}" for t in tags]}
    facts.append(f"refs: {' '.join(sorted(refs))}")
    if refs != want_refs:
        problems.append(f"refs {sorted(refs)} != {sorted(want_refs)}")

    who = set(retro.git(work, "log", "--all", "--format=%an <%ae>%n%cn <%ce>").splitlines())
    who |= {x for x in retro.git(work, "for-each-ref", "refs/tags", "--format=%(taggername) %(taggeremail)").splitlines() if x.strip()}
    facts.append(f"identities: {sorted(who)}")
    if who != {NOREPLY.decode()}:
        problems.append(f"identities {sorted(who)}")

    old_all = retro.git(source, "rev-list", meta["source_tip"]).split()
    main_all = retro.git(work, "rev-list", "main").split()
    finals = [cmap.get(c) for c in old_all]
    facts.append(f"old commits {len(old_all)}, mapped {sum(f is not None for f in finals)}, distinct finals {len(set(finals))}, "
                 f"main commits {len(main_all)} = old + root + {len(groups)} merges"
                 + (" + post-tip" if len(main_all) > len(old_all) + 1 + len(groups) else ""))
    if None in finals or len(set(finals)) != len(old_all) or not set(finals) <= set(main_all):
        problems.append("commit map is not 1:1 onto main")

    # first-parent chain: root, then exactly the retro merges
    fp = retro.git(work, "rev-list", "--first-parent", "--reverse", meta.get("retro_tip", "main")).split()
    if fp != [root] + [g["merge"] for g in groups]:
        problems.append("first-parent chain is not root + retro merges")
    info = {c: p.split() for c, p in (x.split(" ", 1) if " " in x else (x, "") for x in
                                        retro.git(work, "log", "--format=%H %P", "main").splitlines())}
    if info[root] or retro.git(work, "rev-parse", f"{root}^{{tree}}").strip() != EMPTY_TREE:
        problems.append("root is not a parentless empty-tree commit")

    # DAG preserved: parents map through, except a group's first commit hangs off the previous merge
    old_parents = {c: p.split() for c, p in (x.split(" ", 1) if " " in x else (x, "") for x in
                                              retro.git(source, "log", "--format=%H %P", meta["source_tip"]).splitlines())}
    firsts = {g["old_first"]: (groups[i - 1]["merge"] if i else root) for i, g in enumerate(groups)}
    for c, ps in old_parents.items():
        want = [cmap[p] for p in ps]
        if c in firsts:
            want = [firsts[c]] + want[1:]
        if info[cmap[c]] != want:
            problems.append(f"parents of {c[:12]} not preserved")

    trees = {}

    def tree(x):
        if x not in trees:
            trees[x] = retro.git(work, "rev-parse", f"{x}^{{tree}}").strip()
        return trees[x]

    for g in groups:
        if info[g["merge"]] != [g["base"], g["head"]] or tree(g["merge"]) != tree(g["head"]) or g["head"] != cmap[g["old_last"]]:
            problems.append(f"merge #{g['n']} is not (base, head) with the head's tree")
    for name, t in tags.items():
        obj = retro.git(work, "rev-parse", f"refs/tags/{name}^{{commit}}").strip()
        if obj != t["merge"] or tree(obj) != tree(cmap[t["old_target"]]):
            problems.append(f"tag {name} is not on the merge with the tagged commit's tree")
        facts.append(f"tag {name} -> merge {obj[:10]} (tree == final of old {t['old_target'][:10]})")

    last_merge = groups[-1]["merge"]
    head = retro.git(work, "rev-parse", "main").strip()
    if head != last_merge:  # hashfix ran: one more PR merge on top, nothing else
        post = retro.git(work, "rev-list", f"{last_merge}..{head}").split()
        facts.append(f"post-tip PR: merge {head[:10]} + {len(post) - 1} commit(s)")
        if info[head][:1] != [last_merge] or len(info[head]) != 2 or tree(head) != tree(info[head][1]):
            problems.append("post-tip commit is not a --no-ff merge of the retro tip with its branch's tree")
    want_tip = forward_tree(source, meta["source_tip"], rules, work)
    facts.append(f"tip tree {tree(last_merge)} vs apply.py forward rename of the old tip {want_tip}")
    if tree(last_merge) != want_tip:
        problems.append("tip tree != forward rename of the original tip")

    if deep:
        bad = deep_trees(source, work, rules, cmap)
        facts.append(f"deep: {len(cmap)} commit trees == rules(original tree): {len(cmap) - len(bad)} ok")
        problems += bad[:20]

    leftover = sum("cairn" in m.lower() for m in retro.git(work, "log", "--format=%B%x00", "main").split("\0"))
    facts.append(f"messages still containing 'cairn' (holds, if any): {leftover}")
    facts.append(f"groups {len(groups)} over {len({g['date'] for g in groups})} days")
    return problems, facts


# ---- main -----------------------------------------------------------------------


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--source", required=True, help="the original cairn repo (read-only)")
    ap.add_argument("--work", required=True, help="new directory for the rewritten clone")
    ap.add_argument("--rules", required=True, help="rules module: scripts/nostos_rename/rules.py")
    ap.add_argument("--titles", default=str(retro.TITLES))
    ap.add_argument("--ref", default="main")
    ap.add_argument("--tags", nargs="*", default=list(retro.KEEP_TAGS))
    ap.add_argument("--verify-only", action="store_true")
    ap.add_argument("--shallow-verify", action="store_true", help="skip the every-commit tree proof")
    a = ap.parse_args(argv)
    rules = retro.load_rules(a.rules)
    work = Path(a.work).resolve()

    if not a.verify_only:
        clone(a.source, work, a.ref, a.tags)
        source_tip = retro.git(work, "rev-parse", "main").strip()
        groups = retro.compute_groups(work, "main", a.tags)
        titles = retro.titles_for(groups, a.titles)
        post = retro.post_row(a.titles, len(groups) + 1)
        for g, derived, row in zip(groups, retro.group_tags(work, groups), titles):
            if row["tags"] == retro.AUTO_TAGS:
                row["tags"] = retro.tag_prefix(derived)
            if retro.tag_prefix(derived) != row["tags"]:
                print(f"note: #{g.n} {g.date} uses signed-off tags {row['tags']} (derived {retro.tag_prefix(derived)})")
        old_all = retro.git(work, "rev-list", "main").split()
        print(f"stage 1: filter-repo over {len(old_all)} commits")
        s1_of = stage1(work, rules)
        missing = [c for c in old_all if s1_of.get(c, "0" * 40) == "0" * 40]
        if missing or len(s1_of) != len(old_all):
            raise SystemExit(f"filter-repo map is not 1:1: {len(s1_of)} entries, {len(missing)} missing")
        print(f"stage 2: {len(groups)} retro groups")
        root, final, out_groups, tag_out, cites = stage2(work, groups, titles, s1_of, a.tags, rules)
        base = work / OUT
        base.mkdir(parents=True, exist_ok=True)
        (base / "commit-map").write_text("old final\n" + "".join(f"{o} {final[s]}\n" for o, s in s1_of.items()))
        (base / "message-cites.txt").write_text(cites.report())
        (base / "groups.json").write_text(json.dumps(
            {"source": str(Path(a.source).resolve()), "source_tip": source_tip, "root": root,
             "retro_tip": out_groups[-1]["merge"], "groups": out_groups, "tags": tag_out,
             "post": {**post, "tags": retro.parse_prefix(post["tags"])}}, indent=1))
        print(f"message cites: {len(cites.done)} rewritten, {len(cites.skipped)} left (see {base / 'message-cites.txt'})")

    problems, facts = verify(a.source, work, rules, deep=not a.shallow_verify)
    (work / OUT / "verify.txt").write_text("\n".join(facts + [f"PROBLEM {p}" for p in problems]) + "\n")
    print("\n".join(facts))
    if problems:
        print("\n".join(f"PROBLEM {p}" for p in problems[:40]))
        return 1
    print("verify: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
