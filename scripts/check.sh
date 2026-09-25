#!/usr/bin/env bash
# scripts/check.sh — the one root check: local green = CI green.
#
#   scripts/check.sh            # every area (same as `make check`)
#   scripts/check.sh <area>     # one area, named after its CI job
#
# Each area runs the steps of the same-named job in .github/workflows/ci.yml
# (pr-title: pr.yml). Change a job and its area together. An area whose
# toolchain or inputs are absent is skipped green with a one-line note
# (docs/ci/decisions.md row 4). CI installs every toolchain, so a local skip
# never hides a CI job.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# ci.yml's workflow-level env: rustc warnings fail every cargo step.
export RUSTFLAGS="${RUSTFLAGS:--D warnings}"
# An fvm project pin, when there is one, wins over the global flutter.
if [[ -d .fvm/flutter_sdk/bin ]]; then PATH="$PWD/.fvm/flutter_sdk/bin:$PATH"; fi

# `all` runs them in this order: cheap first.
AREAS=(commits pr-title deny lint-test sdk-typecheck benchmark e2e-pg sdk-e2e flutter)

# Conventional-commit types: the standard set plus `bench`, which this repo's
# history uses for measurement commits. git's own `Revert "…"` also passes.
CONVENTIONAL='(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert|bench)(\([^)]+\))?!?: |Revert "'

note() { printf '\033[0;33m-- %s: %s\033[0m\n' "$1" "$2"; }

need() { # <area> <tool>… — true when every tool is on PATH, else a skip note
  local area=$1 tool
  shift
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || { note "$area" "skipped ($tool not installed)"; return 1; }
  done
}

# The tags of a `[arxa-…]` tag-map table: docs/ci/decisions.md (the SSOT) and
# its copy in the PR template.
# shellcheck disable=SC2016 # the backticks are markdown code spans, not expansions
tag_map() { grep -oE '^\| `\[arxa-[a-z-]+\]`' "$1" | tr -d '|` '; }

area_lint_test() { # fmt-check + clippy -D warnings + test --include-ignored
  need lint-test cargo make || return 0
  make ci
}

pg_probe() {
  docker compose -f docker/docker-compose.yml exec -T postgres psql -U nostos -d nostos -tAc \
    "SELECT 1 FROM pg_publication WHERE pubname='nostos_pub'" 2>/dev/null | grep -q 1
}

area_e2e_pg() {
  need e2e-pg docker make || return 0
  docker info >/dev/null 2>&1 || { note e2e-pg "skipped (docker daemon not running)"; return 0; }
  # A Postgres that already answers is reused (no restart under a running
  # dev stack) and left up afterwards; `make pg-down` stops it. Otherwise:
  # the CI job's gate, nostos_pub present 5x in a row, because the init-time
  # temporary server has it too (see the comment in ci.yml).
  if ! pg_probe; then
    docker compose -f docker/docker-compose.yml up -d
    local ok=0
    for _ in $(seq 1 180); do
      if pg_probe; then ok=$((ok + 1)); else ok=0; fi
      [[ $ok -ge 5 ]] && break
      sleep 1
    done
    [[ $ok -ge 5 ]] || { echo "Postgres never became ready (make pg-logs)" >&2; return 1; }
  fi
  # The CI job's nostos-infra --features pg suite, plus nostos-cli's pg suite
  # and the leaked-slot sweep (see the Makefile).
  make pg-e2e
}

area_deny() {
  need deny cargo-deny || return 0
  cargo deny check licenses advisories bans
}

area_sdk_e2e() {
  need sdk-e2e cargo node dotnet || return 0
  SDK_E2E_STRICT=1 ./scripts/sdk-e2e.sh rust node dotnet tauri
}

area_flutter() {
  need flutter flutter dart cargo python3 || return 0
  # Idempotent global opt-in, as in the CI job.
  flutter config --enable-native-assets >/dev/null
  (
    cd sdk/nostos_flutter
    flutter pub get
    dart format --set-exit-if-changed -o none lib test example/lib
    flutter analyze
    flutter test
  )
  (
    cd apps/atlet/flutter
    # ci.yml's ponytail: SwiftPM's rsync never creates these on a fresh checkout.
    mkdir -p build/ios/SourcePackages build/macos/SourcePackages
    flutter pub get
    dart format --set-exit-if-changed -o none lib test integration_test
    flutter analyze
    flutter test
  )
  python3 sdk/nostos_flutter/scripts/check-doc-signatures.py
  (
    cd sdk/nostos_flutter/rust
    cargo clippy --all-targets -- -D warnings
    cargo test
  )
}

area_benchmark() { # the CI smoke size, not the headline run
  need benchmark cargo make || return 0
  make bench BENCH_CLIENTS=1000 BENCH_EVENTS=10000 BENCH_RESULTS_DIR=benches/results-smoke
}

area_sdk_typecheck() {
  need sdk-typecheck npm || return 0
  (cd sdk/nostos_react_native && npm ci && npm run typecheck)
  (cd sdk/nostos_capacitor && npm ci && npm run build)
}

# Required (row 6). Checks the commits this branch adds over main; merges are
# excluded because a PR run checks out GitHub's synthetic "Merge X into Y"
# commit, which a naive check false-positives (energize PR #1, 2026-08-16).
# COMMIT_RANGE overrides the range, e.g. to try the check on old history.
area_commits() {
  local base=origin/main range subjects bad
  git rev-parse -q --verify "$base" >/dev/null || base=main
  range=${COMMIT_RANGE:-$base..HEAD}
  subjects=$(git rev-list --no-merges --no-commit-header --format='%h %s' "$range")
  bad=$(grep -vE "^[0-9a-f]+ ($CONVENTIONAL)" <<<"$subjects" || true)
  if [[ -n $bad ]]; then
    echo "commits: subject(s) in $range without a conventional prefix (type(scope): …):" >&2
    printf '  %s\n' "$bad" >&2
    return 1
  fi
  echo "commits: every subject in $range is conventional"
}

# Warn-first (row 5): a missing or unknown [arxa-<skill>] tag is an annotation
# and the job stays green. Flipping it to required is a recorded decision.
area_pr_title() {
  local tags prefix tag re='^((\[[^]]+\])+) '
  tags=$(tag_map docs/ci/decisions.md)
  if [[ $tags != "$(tag_map .github/pull_request_template.md)" ]]; then
    echo "::warning title=tag map drift::.github/pull_request_template.md's tag map differs from docs/ci/decisions.md (the SSOT); copy the table over"
  fi
  if [[ -z ${PR_TITLE:-} ]]; then
    note pr-title "skipped (no PR title; CI passes it, locally: PR_TITLE='[arxa-…] …' scripts/check.sh pr-title)"
    return 0
  fi
  if [[ $PR_TITLE =~ $re ]]; then
    prefix=${BASH_REMATCH[1]}
    while read -r tag; do
      grep -qxF "$tag" <<<"$tags" || prefix=
    done < <(grep -oE '\[[^]]+\]' <<<"$prefix")
  else
    prefix=
  fi
  if [[ -z $prefix ]]; then
    echo "::error title=PR title::start the title with [arxa-<skill>] tag(s) from docs/ci/decisions.md, first = primary stage: $(tr '\n' ' ' <<<"$tags")"
    return 1
  else
    echo "pr-title: $prefix"
  fi
}

area=${1:-all}
case " all ${AREAS[*]} " in
  *" $area "*) ;;
  *) echo "usage: scripts/check.sh [$(tr ' ' '|' <<<"all ${AREAS[*]}")]" >&2; exit 2 ;;
esac

if [[ $area == all ]]; then
  for a in "${AREAS[@]}"; do
    printf '\033[1;36m== %s\033[0m\n' "$a"
    "area_${a//-/_}"
  done
else
  "area_${area//-/_}"
fi
echo "✓ check.sh $area green"
