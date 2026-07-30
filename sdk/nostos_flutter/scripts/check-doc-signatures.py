#!/usr/bin/env python3
"""Assert every named argument in a `NostosDatabase.<factory>(...)` snippet in the
docs is a real parameter of that factory.

Why this exists: on 2026-07-30 both `README.md` and `USAGE.md` documented
`NostosDatabase.supabase(supabaseUrl:, supabaseAnonKey:, accessToken:)`. None of
those parameters exist — the real signature is `{nostosUrl, schema, sqlitePath}`.
The samples had been wrong for weeks and *nothing could catch them*: `make ci`
runs Rust, `dart analyze` ignores fenced code in markdown, and the e2e slices
exercise `example/`, never the docs. The invented names came from the redesign
plan's aspirational DX sample being copied as if it were the built API.

Deliberately a string check, not a compile: it needs no pub deps, no Flutter SDK,
and runs in ~30ms, so it can sit in front of any hook that will have it.

ponytail: only checks `NostosDatabase.*` named args — the surface that actually
drifted. Extend to `Nostos.*`/`Collection.*` if those start lying too. Upgrade
path if this ever isn't enough: extract snippets into a real
`example/doc_snippets.dart` and let the analyzer own it.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "lib" / "src" / "nostos_database.dart"
DOCS = [ROOT / "README.md", ROOT / "USAGE.md"]


def factory_params(src: str) -> dict[str, set[str]]:
    """Map factory name -> declared named parameters, from `static Future<..> f({..})`."""
    out: dict[str, set[str]] = {}
    for m in re.finditer(
        r"static\s+Future<NostosDatabase>\s+(\w+)\s*\(\s*\{(.*?)\}\s*\)", src, re.S
    ):
        name, body = m.group(1), m.group(2)
        # A parameter is the identifier immediately before `,` or end-of-body,
        # after stripping defaults. `required Foo bar,` -> `bar`.
        params = {
            p.group(1)
            for p in re.finditer(r"(\w+)\s*(?:=\s*[^,]+)?\s*(?:,|$)", body)
        }
        # Drop type names / keywords that the loose regex also catches.
        noise = {"required", "String", "int", "bool", "NostosSchema", "NostosConfig", "Future"}
        out[name] = {p for p in params if p not in noise}
    return out


def main() -> int:
    src = SRC.read_text()
    params = factory_params(src)
    if not params:
        print(f"FAIL: parsed no NostosDatabase factories from {SRC} — regex stale?")
        return 2

    bad = 0
    for doc in DOCS:
        if not doc.exists():
            continue
        text = doc.read_text()
        # Only look inside fenced code blocks. Scanning raw prose made this cry
        # wolf: an inline mention like `NostosDatabase.open(config: ...)` has no
        # terminating `;`, so the argument span ran forward and swallowed the
        # next real code block, blaming its arguments on the wrong factory.
        for fence in re.finditer(r"^```[a-zA-Z]*\n(.*?)^```", text, re.S | re.M):
            # Blank out `//` comments (keeping newlines so line numbers survive).
            # A comment is allowed to contain `;` or `(`, and both would otherwise
            # truncate the argument span and make the call silently unchecked —
            # a check that skips in silence is worse than no check at all. This
            # is not hypothetical: a `// e.g. from path_provider; any writable
            # path works` comment in README.md disabled its own call site.
            block = re.sub(r"//[^\n]*", "", fence.group(1))
            block_line = text[: fence.start()].count("\n") + 1
            for call in re.finditer(
                r"NostosDatabase\.(\w+)\s*\(([^();]*?)\)\s*;", block, re.S
            ):
                factory, args = call.group(1), call.group(2)
                if factory not in params:
                    continue
                line = block_line + block[: call.start()].count("\n") + 1
                for arg in re.finditer(r"^\s*(\w+)\s*:", args, re.M):
                    if arg.group(1) not in params[factory]:
                        print(
                            f"FAIL {doc.name}:{line}: NostosDatabase.{factory} has no "
                            f"parameter '{arg.group(1)}' "
                            f"(declared: {', '.join(sorted(params[factory]))})"
                        )
                        bad += 1

    checked = ", ".join(f"{k}({len(v)})" for k, v in sorted(params.items()))
    if bad:
        print(f"\n{bad} bad named argument(s) in docs. Factories: {checked}")
        return 1
    print(f"OK: doc snippets match NostosDatabase factories — {checked}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
