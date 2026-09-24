#!/usr/bin/env python3
"""Retro PR model for the nostos history: day groups, [arxa-*] tags, titles, bodies.

Spec: docs/ci/decisions.md rows 3, 3b, 3c, 12, 14. rewrite.py and hashfix.py
import this module; the CLI prints the grouping and drafts the titles file.

  retro.py groups --repo PATH [--ref main] [--tags v0.1.0 v0.2.0]
  retro.py titles --repo PATH --rules RULES.py [--out retro/titles.tsv]

A group is a run of first-parent commits of `main` that share a COMMITTER date
(YYYY-MM-DD in the commit's own timezone offset), so it is the day the work
landed on main. A kept tag closes its group at the tagged commit (decision 3c),
so the tag can later sit on a merge whose tree equals the tagged commit's.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import importlib.util
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
TITLES = HERE / "retro" / "titles.tsv"
KEEP_TAGS = ("v0.1.0", "v0.2.0")
NOREPLY_NAME = "unfazed-dev"
NOREPLY_EMAIL = "117146850+unfazed-dev@users.noreply.github.com"
# The historical sessions did not record which model they ran on, so the footer
# names the family, not a version.
FOOTER = "🤝 Collaborated with Claude via [Claude Code](https://claude.com/claude-code)"

AREAS = ("arxa-builder", "arxa-designer", "arxa-tester", "arxa-cicd", "arxa-deployer")
TEST_SEGMENTS = {"tests", "test", "benches", "bench", "integration_test", "test_driver"}
TEST_FILE = re.compile(r"(_test|\.test|\.spec)\.[A-Za-z]+$|^test_.*\.py$")

CONV = re.compile(r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^()]*)\))?(?P<bang>!)?:\s+(?P<desc>.+)$")
# git-cliff's default commit_parsers, in its section order.
SECTIONS = (
    ("Features", ("feat",)),
    ("Bug Fixes", ("fix",)),
    ("Refactor", ("refactor",)),
    ("Documentation", ("docs", "doc")),
    ("Performance", ("perf",)),
    ("Styling", ("style",)),
    ("Testing", ("test", "tests")),
    ("Miscellaneous Tasks", ("chore", "ci", "build")),
    ("Revert", ("revert",)),
    ("Other", ()),
)
MERGE_SUBJECT = re.compile(
    r"^(?P<tags>(?:\[arxa-[a-z-]+\])+) (?P<date>\d{4}-\d{2}-\d{2}) — (?P<title>.+) \(#(?P<n>\d+)\)$"
)


def git(repo, *args, input=None, check=True) -> str:
    r = subprocess.run(
        ["git", "-C", str(repo), *args],
        input=input,
        capture_output=True,
        encoding="utf-8",
        errors="surrogateescape",
    )
    if check and r.returncode:
        raise SystemExit(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


def gitb(repo, *args, input: bytes | None = None) -> bytes:
    r = subprocess.run(["git", "-C", str(repo), *args], input=input, capture_output=True)
    if r.returncode:
        raise SystemExit(f"git {' '.join(args)}: {r.stderr.decode(errors='replace').strip()}")
    return r.stdout


def load_rules(path):
    """Load a rules module (rules.py or rules_stub.py) by file path."""
    spec = importlib.util.spec_from_file_location("nostos_rules", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    for fn in ("rename_path", "rename_content", "rename_text"):
        if not callable(getattr(mod, fn, None)):
            raise SystemExit(f"{path}: rules module lacks {fn}()")
    return mod


# ---- grouping ---------------------------------------------------------------


@dataclasses.dataclass
class Group:
    n: int
    date: str
    fp: list[str]
    tag: str | None = None  # kept tag that closes this group

    @property
    def first(self) -> str:
        return self.fp[0]

    @property
    def last(self) -> str:
        return self.fp[-1]


def tag_targets(repo, tags) -> dict[str, str]:
    """commit sha -> tag name, for the kept tags."""
    out = {}
    for t in tags:
        out[git(repo, "rev-parse", "--verify", f"refs/tags/{t}^{{commit}}").strip()] = t
    return out


def compute_groups(repo, ref="main", tags=KEEP_TAGS) -> list[Group]:
    rows = [line.split(" ") for line in git(repo, "log", "--first-parent", "--reverse", "--format=%H %cI", ref).splitlines()]
    at = tag_targets(repo, tags)
    on_chain = {sha for sha, _ in rows}
    off = sorted(t for sha, t in at.items() if sha not in on_chain)
    if off:
        raise SystemExit(f"tags not on the first-parent chain of {ref}: {off}")
    groups: list[Group] = []
    for sha, when in rows:
        day = when[:10]
        if not groups or groups[-1].date != day or groups[-1].tag:
            groups.append(Group(len(groups) + 1, day, []))
        groups[-1].fp.append(sha)
        if sha in at:
            groups[-1].tag = at[sha]
    return groups


def members(repo, head, base) -> list[str]:
    """Commits a PR from `head` into `base` carries, oldest first (base=None: all)."""
    args = ["rev-list", "--topo-order", "--reverse", head] + ([f"^{base}"] if base else [])
    return git(repo, *args).split()


# ---- [arxa-*] tags (decision 3b) --------------------------------------------


def area(path: str) -> str:
    parts = path.split("/")
    name = parts[-1]
    if path.startswith(".github/") or path in ("scripts/check.sh", "Makefile", "deny.toml"):
        return "arxa-cicd"
    if path.startswith(("docs/adr/", "docs/plans/", "docs/research/")) or name in ("ARCHITECTURE.md", "ROADMAP.md"):
        return "arxa-designer"
    if (
        parts[0] in ("docker", "deploy", "packaging")
        or name.startswith(("Dockerfile", "fly."))
        or name == ".dockerignore"
    ):
        return "arxa-deployer"
    if TEST_FILE.search(name) or any(
        p in TEST_SEGMENTS or "e2e" in p or "conformance" in p or p.endswith("-bench") for p in parts
    ):
        return "arxa-tester"
    return "arxa-builder"


def touched_files(repo, ref="main") -> dict[str, list[str]]:
    """sha -> paths changed, for every non-merge commit (a merge would recount its branch)."""
    out: dict[str, list[str]] = {}
    raw = git(repo, "-c", "core.quotePath=false", "log", "--no-merges", "--name-only", "--format=%x01%H", ref)
    for chunk in raw.split("\x01")[1:]:
        lines = [x for x in chunk.split("\n") if x]
        out[lines[0]] = lines[1:]
    return out


def derive_tags(paths) -> list[str]:
    """Primary = area with the most file touches; add any area with >=20% of it."""
    counts = collections.Counter(area(p) for p in paths)
    if not counts:
        return ["arxa-builder"]
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], AREAS.index(kv[0])))
    top = ranked[0][1]
    return [a for a, n in ranked if n * 5 >= top]


def tag_prefix(tags) -> str:
    return "".join(f"[{t}]" for t in tags)


def parse_prefix(prefix: str) -> list[str]:
    return re.findall(r"\[(arxa-[a-z-]+)\]", prefix)


# ---- titles file ------------------------------------------------------------


def read_titles(path) -> list[dict]:
    path = Path(path)
    if not path.exists():
        return []
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines()[1:]:
        if line.strip():
            n, date, tags, title = line.split("\t")
            rows.append({"n": int(n), "date": date, "tags": tags, "title": title})
    return rows


def write_titles(path, rows):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = ["N\tdate\ttags\ttitle"] + [f"{r['n']}\t{r['date']}\t{r['tags']}\t{r['title']}" for r in rows]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _day_keys(rows):
    """(date, k-th group of that date) so a new day appended later keeps old rows."""
    seen = collections.Counter()
    keys = []
    for r in rows:
        seen[r["date"]] += 1
        keys.append((r["date"], seen[r["date"]]))
    return keys


def titles_for(groups: list[Group], path) -> list[dict]:
    """The signed-off titles, checked against the groups the history yields now."""
    rows = read_titles(path)
    want = [(g.n, g.date) for g in groups]
    have = [(r["n"], r["date"]) for r in rows]
    if want != have:
        extra = sorted(set(have) ^ set(want))
        raise SystemExit(
            f"{path}: rows {len(have)} do not match the {len(want)} groups (first difference: {extra[:3]}); "
            "run `retro.py titles` to add drafts for new groups, polish them, get sign-off"
        )
    return rows


def draft_title(subjects) -> str:
    parsed = [CONV.match(s) for s in subjects]
    scopes = collections.Counter(m["scope"] for m in parsed if m and m["scope"])
    feats = [m["desc"] for m in parsed if m and m["type"] == "feat"] or [m["desc"] if m else s for m, s in zip(parsed, subjects)]
    head = re.split(r" — |; |: ", feats[0])[0][:72]
    top = ", ".join(s for s, _ in scopes.most_common(3))
    return f"{top}: {head}" if top else head


# ---- bodies (git-cliff grouping, decision 12) -------------------------------


@dataclasses.dataclass
class Commit:
    sha: str
    subject: str
    merge: bool


def commit_records(repo, shas) -> dict[str, Commit]:
    if not shas:
        return {}
    raw = git(repo, "log", "--no-walk=unsorted", "--stdin", "--format=%H%x00%P%x00%s%x01", input="\n".join(shas) + "\n")
    out = {}
    for rec in raw.split("\x01"):
        rec = rec.strip("\n")
        if rec:
            sha, parents, subject = rec.split("\x00")
            out[sha] = Commit(sha, subject, len(parents.split()) > 1)
    return out


def _entry(c: Commit, short: int) -> tuple[str, str]:
    m = CONV.match(c.subject)
    typ = m["type"].lower() if m else ""
    section = next((name for name, types in SECTIONS if typ in types), "Other")
    text = c.subject
    if m:
        text = (f"*({m['scope']})* " if m["scope"] else "") + m["desc"]
        if m["bang"]:
            text = "[**breaking**] " + text
    return section, f"- {text} (`{c.sha[:short]}`)"


def cliff_sections(commits, short=7, heading="### ") -> list[str]:
    by = {name: [] for name, _ in SECTIONS}
    for c in commits:
        section, line = _entry(c, short)
        by[section].append(line)
    out = []
    for name, lines in by.items():
        if lines:
            out += [f"{heading}{name}", "", *lines, ""]
    return out


def now_true(commits, cap=10) -> list[str]:
    """'What is now true': the feat/fix/perf outcomes of the branch, newest last."""
    seen, out = set(), []
    for c in commits:
        m = CONV.match(c.subject)
        if not m or m["type"] not in ("feat", "fix", "perf") or m["desc"] in seen:
            continue
        seen.add(m["desc"])
        out.append(f"- {m['scope'] + ': ' if m['scope'] else ''}{m['desc']}")
    if not out:
        out = [f"- {c.subject}" for c in commits if not c.merge][:cap]
    if len(out) > cap:
        out = out[:cap] + [f"- …and {len(out) - cap} more — see Commits"]
    return out


def stage_field(tags) -> str:
    return " · ".join(f"`[{t}]` {t}" for t in tags)


def pr_title(tags, date, title) -> str:
    return f"{tag_prefix(tags)} {date} — {title}"


def merge_message(n, tags, date, title, commits, tag=None) -> str:
    k = sum(not c.merge for c in commits)
    lines = [
        f"{pr_title(tags, date, title)} (#{n})",
        "",
        f"Historical date: {date}. Pipeline stage / agent: {', '.join(tags)}.",
        f"Retro PR #{n}: {k} commit{'' if k == 1 else 's'}"
        + (f", closes at tag {tag}." if tag else "."),
        "",
        *cliff_sections(commits, heading=""),
    ]
    return "\n".join(lines).rstrip("\n") + "\n"


def group_md(n, tags, date, title, commits, merge_sha, branch, tag=None) -> str:
    lines = [
        f"# {pr_title(tags, date, title)}",
        "",
        f"**Pipeline stage / agent:** {stage_field(tags)}",
        "",
        f"**Date:** {date} (historical — replayed as retro PR #{n}; the PR page shows the replay date)",
        "",
        f"**Branch:** `{branch}` · **merge:** `{merge_sha[:7]}` · **commits:** {sum(not c.merge for c in commits)}"
        + (f" · **closes at tag:** `{tag}`" if tag else ""),
        "",
        "## What is now true",
        "",
        *now_true(commits),
        "",
        "## Commits",
        "",
        *cliff_sections(commits),
        "---",
        "",
        FOOTER,
        "",
    ]
    return "\n".join(lines)


def release_md(tag, tag_date, tag_message, prev, prs, commits) -> str:
    rng = f"`{prev}`…`{tag}`" if prev else f"first commit…`{tag}`"
    quoted = [("> " + x).rstrip() for x in tag_message.rstrip("\n").split("\n")]
    lines = [
        f"# {tag} (historical — no binaries)",
        "",
        f"Historical — no binaries. A notes-only release recreated for the tag created on {tag_date};"
        " `release.yml` never ran for it, and it is not re-run for old code.",
        "",
        f"**Range:** {rng} · **retro PRs:** #{prs[0]}–#{prs[-1]} · **commits:** {sum(not c.merge for c in commits)}",
        "",
        "## Tag message",
        "",
        *quoted,
        "",
        "## Commits",
        "",
        *cliff_sections(commits),
        "---",
        "",
        FOOTER,
        "",
    ]
    return "\n".join(lines)


# ---- commit-hash cites --------------------------------------------------------

HEX = re.compile(rb"\b[0-9a-f]{7,40}\b")


class Cites:
    """Rewrites commit-hash cites: a >=7-hex token that is the unambiguous prefix
    of exactly one OLD commit becomes the same-length prefix of its new commit
    (git-filter-repo's rule). Everything else is left alone; tokens that look
    like a cite (letters+digits, 7-12 or 40 long, not a UUID part) but resolve
    to nothing are recorded for review."""

    def __init__(self, olds, resolve, keep=()):
        self.by7 = collections.defaultdict(list)
        for o in olds:
            self.by7[o[:7]].append(o)
        self.resolve = resolve  # old full sha -> new full sha, or None
        self.keep = tuple(keep)
        self.done: list[tuple[str, str, str]] = []
        self.skipped: list[tuple[str, str, str]] = []

    def sub(self, data: bytes, where: str) -> bytes:
        def rep(m):
            tok = m.group(0).decode()
            if self.keep and tok.startswith(self.keep):
                self.skipped.append((where, tok, "kept"))
                return m.group(0)
            cands = [o for o in self.by7.get(tok[:7], ()) if o.startswith(tok)]
            if len(cands) != 1:
                around = data[max(m.start() - 1, 0) : m.start()] + data[m.end() : m.end() + 1]
                citey = (len(tok) <= 12 or len(tok) == 40) and re.search("[a-f]", tok) and re.search("[0-9]", tok)
                if cands or (citey and b"-" not in around):
                    self.skipped.append((where, tok, "ambiguous" if cands else "unresolved"))
                return m.group(0)
            new = self.resolve(cands[0])
            if new is None:
                self.skipped.append((where, tok, "unmapped"))
                return m.group(0)
            self.done.append((where, tok, new[: len(tok)]))
            return new[: len(tok)].encode()

        return HEX.sub(rep, data)

    def report(self) -> str:
        lines = [f"rewritten {len(self.done)}"] + [f"  {w}\t{t} -> {n}" for w, t, n in self.done]
        lines += [f"left {len(self.skipped)}"] + [f"  {w}\t{t}\t{why}" for w, t, why in self.skipped]
        return "\n".join(lines) + "\n"


def branch_name(n, date) -> str:
    return f"retro/{n:02d}-{date}"


def doc_name(n, date) -> str:
    return f"{n:02d}-{date}.md"


# ---- CLI --------------------------------------------------------------------


def group_tags(repo, groups, ref="main"):
    """[arxa-*] tags per group from the files its commits touched."""
    touched = touched_files(repo, ref)
    out = []
    prev = None
    for g in groups:
        paths = [p for sha in members(repo, g.last, prev) for p in touched.get(sha, [])]
        out.append(derive_tags(paths))
        prev = g.last
    return out


def cmd_groups(a):
    groups = compute_groups(a.repo, a.ref, a.tags)
    tags = group_tags(a.repo, groups, a.ref)
    days = len({g.date for g in groups})
    print(f"{len(groups)} groups over {days} active days (committer date, own tz); tag splits: "
          + ", ".join(f"{g.tag}@#{g.n}" for g in groups if g.tag))
    for g, t in zip(groups, tags):
        print(f"{g.n:3} {g.date} fp={len(g.fp):3} {tag_prefix(t)}{'  <- ' + g.tag if g.tag else ''}")
    primary = collections.Counter(t[0] for t in tags)
    anyt = collections.Counter(x for t in tags for x in t)
    print("primary:", dict(primary.most_common()))
    print("any position:", dict(anyt.most_common()))


def cmd_titles(a):
    rules = load_rules(a.rules)
    groups = compute_groups(a.repo, a.ref, a.tags)
    tags = group_tags(a.repo, groups, a.ref)
    old = read_titles(a.out)
    kept = dict(zip(_day_keys(old), old))
    rows = []
    keys = _day_keys([{"date": g.date} for g in groups])
    recs = commit_records(a.repo, [s for g in groups for s in g.fp])
    drafted = 0
    for g, t, key in zip(groups, tags, keys):
        prev = kept.get(key)
        if prev:
            if prev["tags"] != tag_prefix(t):
                print(f"#{g.n} {g.date}: keeping signed-off tags {prev['tags']} (derived {tag_prefix(t)})")
            rows.append({"n": g.n, "date": g.date, "tags": prev["tags"], "title": prev["title"]})
        else:
            title = rules.rename_text(draft_title([recs[s].subject for s in g.fp]))
            rows.append({"n": g.n, "date": g.date, "tags": tag_prefix(t), "title": title})
            drafted += 1
    write_titles(a.out, rows)
    print(f"{a.out}: {len(rows)} rows, {drafted} drafted, {len(rows) - drafted} kept")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("groups", "titles"):
        p = sub.add_parser(name)
        p.add_argument("--repo", required=True, help="the ORIGINAL (cairn) repo")
        p.add_argument("--ref", default="main")
        p.add_argument("--tags", nargs="*", default=list(KEEP_TAGS))
        if name == "titles":
            p.add_argument("--rules", required=True, help="rules module path (rename_text is applied to drafts)")
            p.add_argument("--out", default=str(TITLES))
    a = ap.parse_args(argv)
    {"groups": cmd_groups, "titles": cmd_titles}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
