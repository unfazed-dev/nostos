#!/usr/bin/env python3
"""Survivor census: census.py <src-tree-or-git-ref> <renamed-dir>

Counts every `cairn` token (case-insensitive) left in the renamed tree and
explains each one by the rule that kept it: a held path, a `rename:hold`
line, a binary blob, or a hold class from rules.HOLDS. Exits 1 if any token
is unexplained, or if a committed wasm-bindgen glue file no longer matches
its .wasm. Also lists pre-existing `nostos` tokens in the source: the spots
where the rename cannot be inverted.
"""

import collections
import os
import re
import sys

from apply import tracked
from rules import MARKER, held_path, is_binary, rename_path, scan

_TOKEN = re.compile(r"cairn", re.I)
_NOSTOS = re.compile(rb"nostos", re.I)


def explain(path, text):
    """Counter of hold class -> cairn tokens the rules keep in `text`."""
    counts = collections.Counter()
    runs, run = [], []
    for line in text.splitlines(keepends=True):
        if MARKER in line:
            counts["marker"] += len(_TOKEN.findall(line))
            runs.append("".join(run))
            run = []
        else:
            run.append(line)
    runs.append("".join(run))
    for chunk in runs:
        for a, b, cls, _ in scan(path, chunk):
            if cls:
                counts[cls] += len(_TOKEN.findall(chunk[a:b]))
    return counts


def wasm_names(data):
    """(exports, {(module, field)}) from a wasm binary's export/import sections."""
    def leb(i):
        n = shift = 0
        while True:
            b = data[i]
            n |= (b & 0x7F) << shift
            i, shift = i + 1, shift + 7
            if b < 0x80:
                return n, i

    def name(i):
        n, i = leb(i)
        return data[i:i + n].decode(), i + n

    exports, imports, i = set(), set(), 8
    while i < len(data):
        sid = data[i]
        size, j = leb(i + 1)
        end = j + size
        if sid in (2, 7):
            count, j = leb(j)
            for _ in range(count):
                if sid == 7:
                    field, j = name(j)
                    exports.add(field)
                    j = leb(j + 1)[1]
                else:
                    mod, j = name(j)
                    field, j = name(j)
                    imports.add((mod, field))
                    kind = data[j]
                    j += 1
                    if kind in (0, 3):  # func type idx / global (valtype, mut)
                        j = leb(j)[1] if kind == 0 else j + 2
                    elif kind == 1:  # table: reftype + limits
                        j = _limits(data, j + 1, leb)
                    elif kind == 2:  # memory limits
                        j = _limits(data, j, leb)
                    else:
                        j = leb(j + 1)[1]  # tag
        i = end
    return exports, imports


def _limits(data, j, leb):
    flag = data[j]
    j = leb(j + 1)[1]
    return leb(j)[1] if flag & 1 else j


def wasm_gate(dest, files):
    """Each committed .wasm must still match the glue next to it."""
    problems = []
    for new in files:
        if not new.endswith("_bg.wasm"):
            continue
        glue_path = os.path.join(dest, new[: -len("_bg.wasm")] + ".js")
        if not os.path.exists(glue_path):
            continue
        with open(os.path.join(dest, new), "rb") as f:
            exports, imports = wasm_names(f.read())
        with open(glue_path, encoding="utf-8") as f:
            glue = f.read()
        missing = sorted(set(re.findall(r"\bwasm\.(\w+)", glue)) - exports)
        missing += sorted(f"{m}::{f}" for m, f in imports if m not in glue or f not in glue)
        wasm_file = os.path.basename(new)
        if wasm_file not in glue:
            missing.append(f"glue does not load {wasm_file}")
        problems += [f"{new}: {x}" for x in missing]
    return problems


def main(src, dest):
    kept = collections.Counter()
    unexplained, nostos, files = [], [], []
    for mode, path, data in tracked(src):
        new = rename_path(path)
        files.append(new)
        survivors = len(_TOKEN.findall(new))
        if survivors:
            why = collections.Counter({held_path(path): survivors}) if held_path(path) else explain(None, path)
            kept.update({f"path:{c}": n for c, n in why.items()})
            if survivors != sum(why.values()):
                unexplained.append(f"{new}: path")
        if _NOSTOS.search(data) and not held_path(path):
            nostos.append(path)
        if mode == "120000":
            continue
        with open(os.path.join(dest, new), "rb") as f:
            out = f.read()
        found = len(_TOKEN.findall(out.decode("utf-8", "surrogateescape")))
        if not found:
            continue
        cls = held_path(path) or ("binary" if is_binary(data) else None)
        if cls:
            kept[cls] += found
            continue
        why = explain(path, data.decode("utf-8", "surrogateescape"))
        kept.update(why)
        if found != sum(why.values()):
            text = out.decode("utf-8", "surrogateescape")
            spans = [(a, b) for a, b, c, _ in scan(path, text) if c]
            for m in _TOKEN.finditer(text):
                if not any(a <= m.start() < b for a, b in spans):
                    line = text.count("\n", 0, m.start()) + 1
                    unexplained.append(f"{new}:{line}: {text.splitlines()[line - 1].strip()[:120]}")

    print("survivors by hold class:")
    for cls, n in sorted(kept.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  {n:6d}  {cls}")
    print(f"  {sum(kept.values()):6d}  total")
    print(f"pre-existing `nostos` in source (non-invertible), {len(nostos)} files:")
    for p in nostos:
        print(f"  {p}")
    bad = wasm_gate(dest, files)
    for x in bad:
        print(f"WASM GLUE MISMATCH {x}")
    for x in unexplained:
        print(f"UNEXPLAINED {x}")
    print(f"unexplained: {len(unexplained)}, wasm mismatches: {len(bad)}")
    return 1 if unexplained or bad else 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sys.exit(main(*sys.argv[1:]))
