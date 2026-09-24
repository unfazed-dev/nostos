#!/usr/bin/env python3
"""Rename the held identity in place: identity.py <worktree>

The same rules as apply.py, minus the holds ADR-0048 migrates (Postgres,
wire, on-device storage, push payload/channel, Edge Function, Tauri plugin,
cloud cookie, atlet's engine id). What stays held is history or not ours to
rename: applied migrations, the rename docs and tool, git pins into the
archive, the brand metaphor, the legal entity, and symbols baked into the
committed .wasm. Rewrites tracked files and `git mv`s renamed paths.
"""

import os
import subprocess
import sys

import rules

KEEP = {"archive", "git-pin", "brand-metaphor", "legal-entity", "wasm-bindgen-symbol"}

rules.HOLDS = [h for h in rules.HOLDS if h[0] in KEEP]
rules.SCOPED_HOLDS = [h for h in rules.SCOPED_HOLDS if h[0] in KEEP]
rules._PROFILES = rules._profiles()
rules._SCOPES = [rules.re.compile(p) for _, p, _ in rules.SCOPED_HOLDS]
_scan = rules.scan


def _scan_no_dsn(path, text):
    """pg-dsn is held inside rules.scan itself; migrated here too."""
    for a, b, cls, rep in _scan(path, text):
        yield (a, b, None, "nostos") if cls == "pg-dsn" else (a, b, cls, rep)


rules.scan = _scan_no_dsn


def main(tree):
    git = lambda *a: subprocess.run(["git", *a], cwd=tree, check=True, capture_output=True).stdout
    changed = moved = 0
    for rec in git("ls-files", "-s", "-z").split(b"\0"):
        if not rec:
            continue
        meta, path = rec.split(b"\t", 1)
        mode, path = meta.split()[0].decode(), path.decode("utf-8", "surrogateescape")
        if mode != "100644" and mode != "100755":
            continue
        full = os.path.join(tree, path)
        with open(full, "rb") as f:
            data = f.read()
        body = rules.rename_content(path, data)
        if body != data:
            with open(full, "wb") as f:
                f.write(body)
            changed += 1
        new = rules.rename_path(path)
        if new != path:
            os.makedirs(os.path.dirname(os.path.join(tree, new)), exist_ok=True)
            git("mv", path, new)
            moved += 1
    print(f"{changed} files rewritten, {moved} moved")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
