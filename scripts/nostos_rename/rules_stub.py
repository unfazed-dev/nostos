"""Stand-in for rules.py (feat/rename-tool) with the same interface.

A case-preserving cairn -> nostos swap with no hold list beyond this directory
and `rename:hold` lines. It exists so rewrite.py and the tests run before the
real rule table lands; production runs pass `--rules scripts/nostos_rename/rules.py`.
"""

HELD_PREFIXES = ("scripts/nostos_rename/",)
HOLD_MARKER = b"rename:hold"
PAIRS = (("cairn", "nostos"), ("Cairn", "Nostos"), ("CAIRN", "NOSTOS"))


def _held(path: str) -> bool:
    return path.startswith(HELD_PREFIXES)


def rename_text(text: str) -> str:
    for old, new in PAIRS:
        text = text.replace(old, new)
    return text


def rename_path(path: str) -> str:
    return path if _held(path) else rename_text(path)


def rename_content(path: str, data: bytes) -> bytes:
    if _held(path) or b"\0" in data[:8192]:
        return data
    lines = data.split(b"\n")
    out = []
    for line in lines:
        if HOLD_MARKER not in line:
            for old, new in PAIRS:
                line = line.replace(old.encode(), new.encode())
        out.append(line)
    return b"\n".join(out)
