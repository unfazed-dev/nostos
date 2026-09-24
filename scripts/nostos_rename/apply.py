#!/usr/bin/env python3
"""Forward-rename a tree: apply.py <src-tree-or-git-ref> <dest-dir>

Tracked files only. A directory source is read from its working tree via
`git ls-files`; anything else is a git ref in the current repo, read with
`git ls-tree` + `git cat-file`. Modes and symlinks are preserved (link
targets go through rename_path). Path collisions fail loudly.
"""

import os
import subprocess
import sys

from rules import rename_content, rename_path


def _git(args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True).stdout


def tracked(src):
    """Yield (mode, original path, bytes) for every tracked file of `src`."""
    if os.path.isdir(src):
        for rec in _git(["ls-files", "-s", "-z"], cwd=src).split(b"\0"):
            if not rec:
                continue
            meta, path = rec.split(b"\t", 1)
            mode, path = meta.split()[0].decode(), path.decode("utf-8", "surrogateescape")
            full = os.path.join(src, path)
            if mode == "160000":
                sys.exit(f"submodule not supported: {path}")
            if mode == "120000":
                data = os.readlink(full).encode("utf-8", "surrogateescape")
            else:
                with open(full, "rb") as f:
                    data = f.read()
            yield mode, path, data
        return
    entries = []
    for rec in _git(["ls-tree", "-r", "-z", "--full-tree", src]).split(b"\0"):
        if rec:
            meta, path = rec.split(b"\t", 1)
            mode, kind, sha = meta.decode().split()
            if kind != "blob":
                sys.exit(f"unsupported tree entry {kind}: {path.decode()}")
            entries.append((mode, sha, path.decode("utf-8", "surrogateescape")))
    cat = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    for mode, sha, path in entries:
        cat.stdin.write(sha.encode() + b"\n")
        cat.stdin.flush()
        size = int(cat.stdout.readline().split()[2])
        data = cat.stdout.read(size)
        cat.stdout.read(1)
        yield mode, path, data
    cat.stdin.close()
    cat.wait()


def plan(src):
    """[(mode, old path, new path, new bytes)], collision-checked."""
    out, seen, folded = [], {}, {}
    for mode, path, data in tracked(src):
        new = rename_path(path)
        if new in seen:
            sys.exit(f"path collision: {seen[new]} and {path} -> {new}")
        if new.lower() in folded and folded[new.lower()].lower() != path.lower():
            sys.exit(f"case-insensitive collision: {folded[new.lower()]} and {path} -> {new}")
        seen[new] = folded[new.lower()] = path
        body = rename_path(data.decode("utf-8", "surrogateescape")).encode("utf-8", "surrogateescape") \
            if mode == "120000" else rename_content(path, data)
        out.append((mode, path, new, body))
    return out


def main(src, dest):
    if os.path.exists(dest) and os.listdir(dest):
        sys.exit(f"{dest} is not empty")
    files = plan(src)
    for mode, _, new, body in files:
        full = os.path.join(dest, new)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        if mode == "120000":
            os.symlink(body.decode("utf-8", "surrogateescape"), full)
            continue
        with open(full, "wb") as f:
            f.write(body)
        os.chmod(full, 0o755 if mode == "100755" else 0o644)
    print(f"{len(files)} files -> {dest}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(*sys.argv[1:])
