#!/usr/bin/env bash
# The post-rename fixups on a renamed worktree (rewrite.py's output or apply.py's):
#   1. cargo fmt --all                      (ci.yml lint-test: cargo fmt --all -- --check)
#   2. flutter pub get + dart format, the dirs ci.yml runs them in (flutter job + atlet);
#      pub get re-sorts the renamed pubspec.lock files and atlet's macOS plugin registrant
#   3. uniffi-bindgen-cs regen of sdk/nostos_dotnet/dotnet/generated/nostos.cs
#      (sdk/nostos_dotnet/README.md "## Build"; only the checksum lines move)
# Changes are left uncommitted for `hashfix.py --style`. Exits 1 if anything
# outside that scope changed. CARGO_TARGET_DIR is honoured.
#
#   scripts/nostos_rename/postfix.sh <renamed-worktree>
set -euo pipefail

W=$(cd "${1:?usage: postfix.sh <renamed-worktree>}" && pwd)
cd "$W"
[ -z "$(git status --porcelain)" ] || { echo "postfix: $W has uncommitted changes" >&2; exit 1; }
for t in cargo flutter dart uniffi-bindgen-cs; do
  command -v "$t" >/dev/null || { echo "postfix: $t not on PATH" >&2; exit 1; }
done
TARGET=${CARGO_TARGET_DIR:-$W/sdk/nostos_dotnet/target}

# pub get first, as ci.yml does: format reads the language version from the resolved package config
flutter_fmt() { (cd "$1" && shift && flutter pub get >/dev/null && dart format "$@"); }

echo "== cargo fmt --all"
cargo fmt --all
echo "== flutter pub get + dart format"
flutter_fmt sdk/nostos_flutter lib test example/lib
mkdir -p apps/atlet/flutter/build/ios/SourcePackages apps/atlet/flutter/build/macos/SourcePackages  # ci.yml's rsync workaround
flutter_fmt apps/atlet/flutter lib test integration_test
# ponytail: flutter 3.47.5 pub get also appends `android/**` to the example's analyzer excludes,
# on the unrenamed main too (checked 2026-09-24): not the rename's, so it goes back.
git checkout -- sdk/nostos_flutter/example/analysis_options.yaml
echo "== uniffi-bindgen-cs (sdk/nostos_dotnet)"
(cd sdk/nostos_dotnet && cargo build --release -q \
  && uniffi-bindgen-cs --library "$TARGET/release/libnostos_dotnet.dylib" --out-dir dotnet/generated --config uniffi.toml)
# ponytail: the standalone SDK locks (dotnet, swift, kotlin, node, tauri) were already stale before the
# rename (checked 2026-09-24: cargo --locked refuses them on main); the build refreshes it, the bindings
# don't depend on that (uniffi is pinned =0.28.3), so it goes back. A lock refresh is its own PR.
git checkout -- sdk/nostos_dotnet/Cargo.lock

CS=sdk/nostos_dotnet/dotnet/generated/nostos.cs
changed=$( { git diff --name-only; git ls-files --others --exclude-standard; } | sort -u)
out=$(grep -Ev -e '\.rs$' \
  -e '^sdk/nostos_flutter/(lib|test|example/lib)/.*\.dart$' \
  -e '^apps/atlet/flutter/(lib|test|integration_test)/.*\.dart$' \
  -e '^(apps/atlet/flutter|sdk/nostos_flutter/example)/pubspec\.lock$' \
  -e '^apps/atlet/flutter/macos/Flutter/GeneratedPluginRegistrant\.swift$' \
  -e "^$CS\$" <<<"$changed" || true)
bad_cs=$(git diff -U0 -- "$CS" | grep -E '^[-+]' | grep -Ev '^(\+\+\+|---) ' | grep -v checksum || true)
echo "== changed: $(grep -c . <<<"$changed" || true) files ($(grep -c '\.rs$' <<<"$changed" || true) .rs, $(grep -c '\.dart$' <<<"$changed" || true) .dart, $(grep -c 'pubspec\.lock$' <<<"$changed" || true) locks, $(grep -c 'Registrant\.swift$' <<<"$changed" || true) registrant, $(grep -c "^$CS\$" <<<"$changed" || true) bindings)"
[ -z "$out" ] || { echo "postfix: out of scope:" >&2; echo "$out" >&2; exit 1; }
[ -z "$bad_cs" ] || { echo "postfix: $CS changed beyond the checksum lines:" >&2; echo "$bad_cs" >&2; exit 1; }
echo "postfix: in scope; commit with hashfix.py --style"
