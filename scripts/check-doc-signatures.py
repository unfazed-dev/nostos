#!/usr/bin/env python3
"""check-doc-signatures.py — verify docs/api/flutter.md matches the Dart API.

Extracts the canonical method names from the documented API surface and checks
each exists (by name) on the actual Collection<T> / NostosDatabase Dart classes.
This is a structural check, not a type-system check — it catches doc drift
(methods renamed or removed without updating the docs), not signature subtleties.

Exit 0 = all documented methods found in source; exit 1 = drift detected.

ADR-0032 references this script as the doc-signature gate.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# --- canonical API surface (source of truth: docs/api/flutter.md) ------------
# Each entry: (dart_file, method_name, class_name)
# The script verifies <method_name> appears as a member declaration in <dart_file>.

REPO_ROOT = Path(__file__).resolve().parent.parent
SDK_LIB = REPO_ROOT / "sdk" / "nostos_flutter" / "lib" / "src"

EXPECTED_METHODS: list[tuple[str, str, str]] = [
    # NostosDatabase — lifecycle
    (str(SDK_LIB / "nostos_database.dart"), "connect", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "supabase", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "open", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "subscribe", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "subscribeTables", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "pauseSync", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "resumeSync", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "waitForFirstSync", "NostosDatabase"),
    (str(SDK_LIB / "nostos.dart"), "setToken", "Nostos"),
    (str(SDK_LIB / "nostos_database.dart"), "signOut", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "close", "NostosDatabase"),
    # NostosDatabase — writes
    (str(SDK_LIB / "nostos_database.dart"), "write", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "writeBatch", "NostosDatabase"),
    # NostosDatabase — reads / observability
    (str(SDK_LIB / "nostos_database.dart"), "deadLetters", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "watchSql", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "execute", "NostosDatabase"),
    (str(SDK_LIB / "nostos_database.dart"), "collection", "NostosDatabase"),
    # Collection<T> — typed reads
    (str(SDK_LIB / "nostos_database.dart"), "watch", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "getAll", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "get", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "fetchById", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "watchOne", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "count", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "exists", "Collection"),
    # Collection<T> — typed writes
    (str(SDK_LIB / "nostos_database.dart"), "upsert", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "upsertRow", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "patch", "Collection"),
    (str(SDK_LIB / "nostos_database.dart"), "delete", "Collection"),
    # T6 attachments (ADR-0034) — NostosDatabase hook + Attachments driver
    (str(SDK_LIB / "nostos_database.dart"), "registerSignOutHook", "NostosDatabase"),
    (str(SDK_LIB / "attachments.dart"), "attachments", "AttachmentDatabase"),
    (str(SDK_LIB / "attachments.dart"), "queueUpload", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "queueDownload", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "remove", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "pump", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "start", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "stop", "Attachments"),
    (str(SDK_LIB / "attachments.dart"), "lastErrorFor", "Attachments"),
    # Predicate library
    (str(SDK_LIB / "predicate.dart"), "eq", "Where"),
    (str(SDK_LIB / "predicate.dart"), "neq", "Where"),
    (str(SDK_LIB / "predicate.dart"), "lt", "Where"),
    (str(SDK_LIB / "predicate.dart"), "lte", "Where"),
    (str(SDK_LIB / "predicate.dart"), "gt", "Where"),
    (str(SDK_LIB / "predicate.dart"), "gte", "Where"),
    (str(SDK_LIB / "predicate.dart"), "inList", "Where"),
    (str(SDK_LIB / "predicate.dart"), "isNull", "Where"),
    (str(SDK_LIB / "predicate.dart"), "notNull", "Where"),
    (str(SDK_LIB / "predicate.dart"), "and", "Where"),
    (str(SDK_LIB / "predicate.dart"), "or", "Where"),
    (str(SDK_LIB / "predicate.dart"), "not", "Where"),
    (str(SDK_LIB / "predicate.dart"), "asc", "Order"),
    (str(SDK_LIB / "predicate.dart"), "desc", "Order"),
]

# --- helpers -----------------------------------------------------------------

# Matches Dart member declarations: `Future<...> methodName(`, `Stream<...> methodName(`,
# `void methodName(`, `T methodName(`, `static ... methodName(`, etc.
_MEMBER_RE = re.compile(
    r"""
    ^\s*                    # leading whitespace
    (?:static\s+)?          # optional static
    (?:factory\s+)?         # optional factory constructor
    (?:@[\w]+\s+)?          # optional annotation
    (?:async\s*|sync\s*)?   # optional async/sync
    (?:[\w<>,\s?]+?\s+)?    # optional return type (greedy-but-lazy)
    (?:\w+\.)?              # optional Class. prefix (named/factory constructors)
    (\w+)                   # method name (captured)
    (?:<[^>]+>)?            # optional generic type parameter: methodName<T>
    \s*                     # optional whitespace
    \(                      # opening paren = method (not a field)
    """,
    re.MULTILINE | re.VERBOSE,
)


def methods_in_file(path: str) -> set[str]:
    """Return the set of method names declared in a Dart source file."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except FileNotFoundError:
        return set()
    return {m.group(1) for m in _MEMBER_RE.finditer(text)}


# --- main --------------------------------------------------------------------

def main() -> int:
    # Cache: file path → set of declared method names.
    cache: dict[str, set[str]] = {}

    missing: list[tuple[str, str, str]] = []
    for file_path, method_name, class_name in EXPECTED_METHODS:
        if file_path not in cache:
            cache[file_path] = methods_in_file(file_path)
        if method_name not in cache[file_path]:
            missing.append((file_path, method_name, class_name))

    if missing:
        print(f"DOC DRIFT: {len(missing)} documented method(s) not found in source:")
        for fp, name, cls in missing:
            rel = Path(fp).relative_to(REPO_ROOT)
            print(f"  {cls}.{name}  expected in {rel}")
        return 1

    print(f"OK: all {len(EXPECTED_METHODS)} documented methods found in source.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
