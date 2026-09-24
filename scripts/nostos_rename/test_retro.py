#!/usr/bin/env python3
"""Invariants of the retro rewrite on a synthetic repo, plus the pure helpers.

  python3 scripts/nostos_rename/test_retro.py

The repo: 3 days of commits, a side branch merged on day 2, an annotated
`v0.1.0` mid-day 2 (so day 2 splits in two), a deletion of a `cairn` path, a
binary, and a junk tag + branch that must not survive.
"""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import hashfix  # noqa: E402
import replay  # noqa: E402
import retro  # noqa: E402
import rewrite  # noqa: E402

STUB = str(HERE / "rules_stub.py")
OLD_ID = {"GIT_AUTHOR_NAME": "Old Founder", "GIT_AUTHOR_EMAIL": "founders@cairn.dev",
          "GIT_COMMITTER_NAME": "Old Founder", "GIT_COMMITTER_EMAIL": "founders@cairn.dev"}


def sh(repo, *args, when=None):
    env = {**os.environ, **OLD_ID, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    if when:
        env.update(GIT_AUTHOR_DATE=when, GIT_COMMITTER_DATE=when)
    return subprocess.run(["git", "-C", str(repo), *args], env=env, check=True, capture_output=True, text=True).stdout.strip()


def commit(repo, when, message, files=(), rm=()):
    for path, data in files:
        p = Path(repo) / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data if isinstance(data, bytes) else data.encode())
        sh(repo, "add", path)
    for path in rm:
        sh(repo, "rm", "-q", path)
    sh(repo, "commit", "-q", "-m", message, when=when)
    return sh(repo, "rev-parse", "HEAD")


def build(src):
    d1, d2, d3 = "2026-01-01T{}:00+10:00", "2026-01-02T{}:00+10:00", "2026-01-03T{}:00+10:00"
    sh(src.parent, "init", "-q", "-b", "main", src.name)
    c = {}
    c["c1"] = commit(src, d1.format("10:00"), "feat(cairn): init cairn",
                     [("cairn.txt", "hello cairn\n"), ("docs/adr/0001-cairn.md", "# Cairn ADR\n")])
    c["c2"] = commit(src, d1.format("11:00"), "fix: tweak cairn", [("cairn.txt", "hello cairn, tweaked\n")])
    sh(src, "checkout", "-q", "-b", "side")
    c["b1"] = commit(src, d2.format("09:00"), "feat: side work", [("side.txt", "side cairn\n")])
    sh(src, "checkout", "-q", "main")
    c["c3"] = commit(src, d2.format("09:30"), "test: cairn tests", [("tests/cairn_test.rs", "// cairn test\n")])
    sh(src, "merge", "-q", "--no-ff", "side", "-m", "feat: merge side", when=d2.format("10:00"))
    c["m"] = sh(src, "rev-parse", "HEAD")
    c["c4"] = commit(src, d2.format("11:00"), f"docs: notes citing {c['c1'][:7]}",
                     [("docs/notes.md", f"see {c['c1'][:7]} and {c['c2']}; not a cite: 1234567\n")])
    sh(src, "tag", "-a", "v0.1.0", "-m", f"v0.1.0 cairn, cut at {c['c4'][:8]}", when=d2.format("11:30"))
    c["c5"] = commit(src, d2.format("12:00"), "ci: cairn workflow", [(".github/workflows/ci.yml", "name: cairn\n")])
    c["c6"] = commit(src, d3.format("09:00"), "chore: drop cairn.txt", rm=["cairn.txt"])
    c["c7"] = commit(src, d3.format("10:00"), "feat: binary", [("bin.dat", b"cairn\0binary")])
    sh(src, "tag", "junk")
    sh(src, "branch", "other", c["c2"])
    return c


class Pure(unittest.TestCase):
    def test_area(self):
        cases = {
            ".github/workflows/ci.yml": "arxa-cicd", "Makefile": "arxa-cicd", "scripts/check.sh": "arxa-cicd",
            "docs/adr/0001-x.md": "arxa-designer", "docs/ROADMAP.md": "arxa-designer",
            "docker/docker-compose.yml": "arxa-deployer", "Dockerfile": "arxa-deployer", "fly.toml": "arxa-deployer",
            "crates/a/tests/x.rs": "arxa-tester", "sdk/web/src/x.test.ts": "arxa-tester", "e2e-pg/run.sh": "arxa-tester",
            "crates/nostos-bench/src/main.rs": "arxa-tester", "crates/a/src/lib.rs": "arxa-builder", "docs/api/x.md": "arxa-builder",
        }
        for path, want in cases.items():
            self.assertEqual(retro.area(path), want, path)

    def test_derive_tags(self):
        self.assertEqual(retro.derive_tags([]), ["arxa-builder"])
        # 10 builder; 2 tester is 20% of that, kept; 1 cicd is not
        self.assertEqual(retro.derive_tags(["a.rs"] * 10 + ["tests/t.rs"] * 2 + [".github/x"]), ["arxa-builder", "arxa-tester"])
        # tie on count -> AREAS order
        self.assertEqual(retro.derive_tags(["tests/t.rs", "a.rs"]), ["arxa-builder", "arxa-tester"])

    def test_cliff_entry(self):
        c = retro.Commit("a" * 40, "feat(core)!: drop v1", False)
        self.assertEqual(retro._entry(c, 7), ("Features", "- [**breaking**] *(core)* drop v1 (`aaaaaaa`)"))
        self.assertEqual(retro._entry(retro.Commit("b" * 40, "Merge branch x", True), 7)[0], "Other")
        self.assertEqual(retro._entry(retro.Commit("c" * 40, "ci: x", False), 7)[0], "Miscellaneous Tasks")

    def test_cites(self):
        olds = ["abcdef1" + "0" * 33, "abcdef1" + "1" * 33, "1234567" + "a" * 33]
        new = {o: o[::-1] for o in olds}
        c = retro.Cites(olds, new.get, keep=("12345678",))
        out = c.sub(b"x abcdef10 abcdef1 1234567 12345678 1234567aa-b", "f")
        # unique 8-char prefix -> 8-char new prefix; 7-char ambiguous stays; kept stays; 1234567 unique
        self.assertEqual(out, b"x " + new[olds[0]][:8].encode() + b" abcdef1 " + new[olds[2]][:7].encode()
                         + b" 12345678 " + new[olds[2]][:9].encode() + b"-b")
        self.assertEqual({why for _, _, why in c.skipped}, {"ambiguous", "kept"})


class EndToEnd(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        cls.src, cls.work, cls.titles = root / "cairn", root / "nostos", root / "titles.tsv"
        cls.c = build(cls.src)
        cls.tags = ["v0.1.0"]
        with contextlib.redirect_stdout(io.StringIO()):
            retro.main(["titles", "--repo", str(cls.src), "--rules", STUB, "--out", str(cls.titles), "--tags", *cls.tags])
            rows = retro.read_titles(cls.titles)
            rows[3]["tags"] = retro.AUTO_TAGS  # today's group, still growing: re-derived at rewrite time
            retro.write_titles(cls.titles, rows)
            cls.rc = rewrite.main(["--source", str(cls.src), "--work", str(cls.work), "--rules", STUB,
                                   "--titles", str(cls.titles), "--tags", *cls.tags])
        cls.meta = json.loads((cls.work / rewrite.OUT / "groups.json").read_text())
        cls.cmap = dict(x.split() for x in (cls.work / rewrite.OUT / "commit-map").read_text().splitlines()[1:])
        cls.retro_tip = sh(cls.work, "rev-parse", "main")
        (cls.work / "src").mkdir()
        (cls.work / "src/lib.rs").write_text("// what postfix.sh leaves behind\n")
        with contextlib.redirect_stdout(io.StringIO()):
            try:  # a dirty worktree needs --style
                hashfix.main(["--work", str(cls.work)])
                raise AssertionError("hashfix accepted a dirty worktree without --style")
            except SystemExit:
                pass
            hashfix.main(["--work", str(cls.work), "--style", "--date", "2026-01-04T09:00:00+10:00"])
            cls.rc_after = rewrite.main(["--source", str(cls.src), "--work", str(cls.work), "--rules", STUB,
                                         "--tags", *cls.tags, "--verify-only"])

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def tree(self, rev):
        return sh(self.work, "rev-parse", f"{rev}^{{tree}}")

    def test_groups_and_tags(self):
        groups = retro.compute_groups(self.src, "main", self.tags)
        self.assertEqual([(g.n, g.date, len(g.fp), g.tag) for g in groups],
                         [(1, "2026-01-01", 2, None), (2, "2026-01-02", 3, "v0.1.0"), (3, "2026-01-02", 1, None), (4, "2026-01-03", 2, None)])
        self.assertEqual(retro.group_tags(self.src, groups),
                         [["arxa-builder", "arxa-designer"], ["arxa-builder", "arxa-tester"], ["arxa-cicd"], ["arxa-builder"]])

    def test_verify_passes(self):
        self.assertEqual(self.rc, 0)
        self.assertEqual(self.rc_after, 0)

    def test_each_original_maps_once(self):
        olds = sh(self.src, "rev-list", "main").split()
        self.assertEqual(sorted(self.cmap), sorted(olds))
        self.assertEqual(len(set(self.cmap.values())), len(olds))
        self.assertTrue(set(self.cmap.values()) <= set(sh(self.work, "rev-list", "main").split()))

    def test_trees_preserved(self):
        # the forward rename of every original tree is the rewritten tree (the deep verify, spot-checked here)
        self.assertEqual(sh(self.work, "ls-tree", "-r", "--name-only", self.cmap[self.c["c2"]]).split(),
                         ["docs/adr/0001-nostos.md", "nostos.txt"])
        self.assertEqual(sh(self.work, "show", f"{self.cmap[self.c['c2']]}:nostos.txt"), "hello nostos, tweaked")
        self.assertNotIn("nostos.txt", sh(self.work, "ls-tree", "-r", "--name-only", self.cmap[self.c["c6"]]))
        self.assertEqual(sh(self.work, "cat-file", "-s", f"{self.cmap[self.c['c7']]}:bin.dat"), str(len(b"cairn\0binary")))

    def test_merges_carry_group_last_tree(self):
        for g in self.meta["groups"]:
            self.assertEqual(sh(self.work, "log", "-1", "--format=%P", g["merge"]).split(), [g["base"], g["head"]])
            self.assertEqual(self.tree(g["merge"]), self.tree(g["head"]))
            self.assertIn(f"(#{g['n']})", sh(self.work, "log", "-1", "--format=%s", g["merge"]))

    def test_first_parent_chain(self):
        fp = sh(self.work, "rev-list", "--first-parent", "--reverse", self.retro_tip).split()
        self.assertEqual(fp, [self.meta["root"]] + [g["merge"] for g in self.meta["groups"]])
        self.assertEqual(self.tree(self.meta["root"]), rewrite.EMPTY_TREE)
        after = sh(self.work, "rev-list", "--first-parent", "--reverse", "main").split()
        self.assertEqual(after[:-1], fp)  # hashfix adds one more PR merge, nothing else on the chain
        self.assertEqual(len(sh(self.work, "log", "-1", "--format=%P", "main").split()), 2)

    def test_tag_on_equal_tree_merge(self):
        self.assertEqual(sh(self.work, "cat-file", "-t", "v0.1.0"), "tag")
        target = sh(self.work, "rev-parse", "v0.1.0^{commit}")
        g = next(g for g in self.meta["groups"] if g["tag"] == "v0.1.0")
        self.assertEqual(target, g["merge"])
        self.assertEqual(self.tree(target), self.tree(self.cmap[self.c["c4"]]))
        self.assertEqual(sh(self.work, "for-each-ref", "--format=%(taggerdate:iso-strict) %(contents:subject)", "refs/tags/v0.1.0"),
                         f"2026-01-02T11:30:00+10:00 v0.1.0 nostos, cut at {self.cmap[self.c['c4']][:8]}")

    def test_refs_and_identities(self):
        self.assertEqual(sh(self.work, "for-each-ref", "--format=%(refname)").split(), ["refs/heads/main", "refs/tags/v0.1.0"])
        who = set(sh(self.work, "log", "--all", "--format=%an <%ae>%n%cn <%ce>").splitlines())
        self.assertEqual(who, {rewrite.NOREPLY.decode()})

    def test_cites_translated(self):
        new1, new2 = self.cmap[self.c["c1"]], self.cmap[self.c["c2"]]
        self.assertEqual(sh(self.work, "log", "-1", "--format=%s", self.cmap[self.c["c4"]]), f"docs: notes citing {new1[:7]}")
        self.assertEqual(sh(self.work, "show", "main:docs/notes.md"), f"see {new1[:7]} and {new2}; not a cite: 1234567")
        # history itself is untouched by hashfix: the old blob stays in the old commit
        self.assertIn(self.c["c1"][:7], sh(self.work, "show", f"{self.cmap[self.c['c4']]}:docs/notes.md"))

    def test_post_pr(self):
        self.assertEqual(self.meta["groups"][3]["tags"], ["arxa-builder"])
        self.assertEqual(retro.read_titles(self.titles)[-1]["date"], "-")
        self.assertEqual(sh(self.work, "log", "--topo-order", "--format=%s", "main^1..main^2").splitlines(),
                         [hashfix.POST_SUBJECT, hashfix.STYLE_SUBJECT])
        self.assertEqual(sh(self.work, "log", "-1", "--format=%s", "main"),
                         f"[arxa-builder] 2026-01-04 — {retro.POST_TITLE} (#5)")
        self.assertEqual(sh(self.work, "show", "main:src/lib.rs"), "// what postfix.sh leaves behind")

    def test_retro_docs_and_replay_plan(self):
        names = sh(self.work, "ls-tree", "--name-only", "main", "docs/ci/retro/").split()
        self.assertEqual(names, [f"docs/ci/retro/{x}" for x in
                                 ("01-2026-01-01.md", "02-2026-01-02.md", "03-2026-01-02.md", "04-2026-01-03.md", "release-v0.1.0.md")])
        doc = sh(self.work, "show", "main:docs/ci/retro/02-2026-01-02.md")
        self.assertIn("**Pipeline stage / agent:**", doc)
        self.assertIn("## What is now true", doc)
        self.assertIn(f"`{self.cmap[self.c['b1']][:7]}`", doc)  # the side branch rides in its day's PR
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(replay.main(["--repo", str(self.work)]), 0)
        text = out.getvalue()
        self.assertIn("DRY RUN", text)
        self.assertEqual(text.count("gh pr create"), 5)
        self.assertEqual(text.count("gh release create"), 1)


if __name__ == "__main__":
    unittest.main(warnings="ignore")  # filter-repo leaves ResourceWarnings behind
