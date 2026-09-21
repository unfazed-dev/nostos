#!/usr/bin/env bash
# =============================================================================
# rename-to-qairn.sh — the nostos → qairn sweep.
#
# Inventory + rationale: docs/plans/qairn-rename-inventory-2026-09-21.md
#
# DRY-RUN BY DEFAULT. Nothing is written without --apply.
#
# The file universe is `git ls-files` and nothing else, so this script can
# never reach target/, build/, node_modules/, .dart_tool/, Pods/ or any other
# untracked artifact directory.
#
# ORDER (this is the part that is easy to get wrong):
#   1. rewrite FILE CONTENT for every selected tracked file
#   2. THEN `git mv` the paths
# Content first, paths second. The reverse order invalidates the path list
# mid-flight (a rename of crates/nostos-infra/ moves 59 not-yet-rewritten files
# out from under the iterator), and re-listing after every move is O(n²) on a
# 1254-file repo. Renaming after the rewrite is safe because `git mv` does not
# care what is inside the file.
#
# CASE SHAPES are enumerated, not blind-`gi`-substituted:
#   NOSTOS → QAIRN   (env vars, feature flags, Make variables)
#   Nostos → Qairn   (prose, Dart/Swift/Kotlin/C# type names)
#   nostos → qairn   (crate names, snake paths, urls, npm scope, reverse-DNS)
# The three are disjoint under case-sensitive matching, so the sweep is one
# pass and is naturally IDEMPOTENT: "qairn" contains no "nostos", so a second
# run finds zero occurrences and zero paths to move.
#
# RISKY CATEGORIES ARE OPT-IN. The default sweep deliberately leaves alone
# anything whose identity lives OUTSIDE this repo (a running Postgres, a
# device's SQLite file, a published GitHub release asset, a historical record).
# Each is a separate flag so the judgement calls are explicit and reviewable.
# =============================================================================

set -euo pipefail

FROM_LC="nostos"; TO_LC="qairn"
FROM_TC="Nostos"; TO_TC="Qairn"
FROM_UC="NOSTOS"; TO_UC="QAIRN"

REPO_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

MODE="dry"
SAMPLE=4
OPT_PG_IDENTITY=0
OPT_WIRE_IDENTITY=0
OPT_CLIENT_STORAGE=0
OPT_EXTERNAL_IDS=0
OPT_HISTORICAL_DOCS=0
OPT_BRAND_PROSE=0
OPT_LOCKFILES=0

usage() {
  cat <<'USAGE'
rename-to-qairn.sh [--apply] [--sample N] [opt-in category flags]

  --apply              write the changes (default: dry-run, nothing touched)
  --sample N           sample lines printed per category in dry-run (default 4)

Opt-in categories — ALL DEFAULT OFF. Each one is left untouched by the safe
sweep because its identity is owned by something outside this repository.

  --pg-identity        the Postgres role / password / database name / SQL
                       object names (nostos:nostos@…/nostos, cairn_writer,
                       cairn_pub, cairn_oplog, cairn_slot, cairn_push_tokens).
                       MUST move together with a live database or not at all.
  --wire-identity      negotiated protocol identifiers: the iroh ALPN
                       "cairn/sync/1", the Tauri invoke prefix "plugin:cairn|",
                       the "plugins.cairn" config key, "cairn:multitab".
                       Both ends of a connection must change in the same release.
  --client-storage     on-device names: cairn.db / cairn.sqlite, the client
                       SQLite tables cairn_data / cairn_meta / cairn_outbox,
                       the OPFS pool "cairn:opfs-sahpool", the storage key
                       prefix "cairn:checkpoint:", the .nostos/ project dir and
                       nostos_rules.toml. Renaming orphans existing local data
                       unless a migration ships with it.
  --external-ids       things already published under the old name: the GitHub
                       repo URL, the homebrew-tap tap, v0.1.0/v0.2.0 release
                       asset filenames, the run.nostos.* bundle ids.
  --historical-docs    docs/adr/, docs/plans/, plans/, benches/results/,
                       archive/, .zcode/ — dated records of decisions and
                       measurements made under the old name.
  --brand-prose        the five sentences that explain what a nostos IS (a pile
                       of stones marking a trail). A rename makes them nonsense;
                       they need rewriting by a human, not substituting.
  --lockfiles          Cargo.lock / pubspec.lock / package-lock.json. These are
                       generated. Regenerate them, do not sed them.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)            MODE="apply" ;;
    --sample)           SAMPLE="${2:?--sample needs a number}"; shift ;;
    --pg-identity)      OPT_PG_IDENTITY=1 ;;
    --wire-identity)    OPT_WIRE_IDENTITY=1 ;;
    --client-storage)   OPT_CLIENT_STORAGE=1 ;;
    --external-ids)     OPT_EXTERNAL_IDS=1 ;;
    --historical-docs)  OPT_HISTORICAL_DOCS=1 ;;
    --brand-prose)      OPT_BRAND_PROSE=1 ;;
    --lockfiles)        OPT_LOCKFILES=1 ;;
    -h|--help)          usage; exit 0 ;;
    *)                  echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [[ "$MODE" == "apply" ]] && [[ -n "$(git status --porcelain)" ]]; then
  echo "refusing to --apply on a dirty worktree: commit or stash first." >&2
  echo "a rename sweep you cannot 'git checkout -- .' out of is not a sweep." >&2
  exit 1
fi

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# -----------------------------------------------------------------------------
# 1. Protection table.
#
# Each enabled-OFF category contributes regexes that are lifted out of the text
# (replaced by a sentinel), held aside across the case sweep, and put back
# verbatim. This is why the script does not need per-file exclusion lists for
# these: a protected token survives inside a file that is otherwise rewritten.
#
# Format: <category>\t<perl regex>.  Order matters — most specific first.
# -----------------------------------------------------------------------------
PROTECT="$WORK/protect.tsv"; : > "$PROTECT"
p() { printf '%s\t%s\n' "$1" "$2" >> "$PROTECT"; }

if [[ $OPT_PG_IDENTITY -eq 0 ]]; then
  # The whole connection string goes in one piece: user, password, host, port
  # AND database name. Half-renaming a DSN is the single most destructive
  # outcome available here (see the inventory, "failure mode").
  p pg-identity 'postgres(?:ql)?://[^\s"'"'"'`)\\]+'
  p pg-identity 'POSTGRES_(?:USER|PASSWORD|DB):[ \t]*nostos\b'
  p pg-identity '(?:-U|-d)[ \t]+nostos\b'
  p pg-identity '\bnostos_writer(?:_dev_pw)?\b'
  p pg-identity '\bidx_nostos_\w+'
  p pg-identity '\bnostos_(?:pub|oplog|slot|push_tokens)\b'
  p pg-identity 'CREATE ROLE cairn\b'
fi

if [[ $OPT_WIRE_IDENTITY -eq 0 ]]; then
  p wire-identity 'cairn/sync/1'
  p wire-identity 'plugin:nostos\|'
  p wire-identity 'plugins\.nostos\b'
  p wire-identity 'cairn:multitab'
fi

if [[ $OPT_CLIENT_STORAGE -eq 0 ]]; then
  p client-storage 'cairn:opfs-sahpool'
  p client-storage 'cairn:checkpoint:'
  p client-storage '\bnostos_(?:data|meta|outbox)\b'
  p client-storage '\bnostos\.(?:db|sqlite|toml)\b'
  p client-storage '\bnostos_rules\.toml\b'
  p client-storage '\.nostos(?:/|")'
fi

if [[ $OPT_EXTERNAL_IDS -eq 0 ]]; then
  p external-ids 'https://github\.com/[^\s"'"'"'`)<]*nostos[^\s"'"'"'`)<]*'
  p external-ids '\bhomebrew-tap\b'
  p external-ids '\bdev\.nostos[\w.]*'
  p external-ids '\bnostos-(?:aarch64|x86_64)-[\w.-]+?\.(?:tar\.gz|zip)(?:\.sha256)?'
fi

if [[ $OPT_BRAND_PROSE -eq 0 ]]; then
  # The metaphor sentences. "A qairn is a pile of stones that marks a trail" is
  # not a true sentence about anything; these need a human rewrite.
  p brand-prose '[Aa] nostos is a pile of stones[^.]*\.'
  p brand-prose '[Aa] nostos is a trail marker of stacked stones[^.]*\.'
  p brand-prose '[Aa] nostos is a stack of stones[^.]*\.'
  p brand-prose 'a pile of stones marking a trail'
  p brand-prose 'a cairn is how you find your way'
  p brand-prose 'are our cairns'
  p brand-prose '\*\*karn/\*\*'
fi

# -----------------------------------------------------------------------------
# 2. File selection.
#
# `git ls-files -z` is the universe. Excluded-by-flag DIRECTORIES are dropped
# wholesale (their content is a record, not a reference); excluded-by-flag
# TOKENS are handled by the protection table above.
# -----------------------------------------------------------------------------
EXCLUDE_RE=''
add_exclude() { EXCLUDE_RE="${EXCLUDE_RE:+$EXCLUDE_RE|}$1"; }
[[ $OPT_HISTORICAL_DOCS -eq 0 ]] && add_exclude '^(docs/adr/|docs/plans/|plans/|benches/results/|archive/|\.zcode/)'
[[ $OPT_LOCKFILES      -eq 0 ]] && add_exclude '(^|/)(Cargo\.lock|pubspec\.lock|package-lock\.json|pnpm-lock\.yaml|yarn\.lock)$'

SELECTED="$WORK/selected"
EXCLUDED="$WORK/excluded"
if [[ -n "$EXCLUDE_RE" ]]; then
  git ls-files -z | tr '\0' '\n' | grep -Ev "$EXCLUDE_RE" > "$SELECTED" || true
  git ls-files -z | tr '\0' '\n' | grep -E  "$EXCLUDE_RE" > "$EXCLUDED" || true
else
  git ls-files -z | tr '\0' '\n' > "$SELECTED"; : > "$EXCLUDED"
fi

# -----------------------------------------------------------------------------
# 3. The rewriter. One perl process over the whole selected list.
#
#    protect → sentinel → case sweep → restore sentinel
#
# Sentinels are \x01<n>\x01, which cannot occur in source text we care about;
# the script aborts on any file that already contains \x01 rather than guess.
# -----------------------------------------------------------------------------
cat > "$WORK/rewrite.pl" <<'PERL'
use strict; use warnings;

my $mode   = $ENV{MODE}   // 'dry';
my $sample = $ENV{SAMPLE} // 4;
my ($FLC,$TLC,$FTC,$TTC,$FUC,$TUC) =
  @ENV{qw(FROM_LC TO_LC FROM_TC TO_TC FROM_UC TO_UC)};

my (@prot, %prot_cat);
if (open my $pf, '<', $ENV{PROTECT}) {
  while (<$pf>) { chomp; next unless length; my ($cat,$rx) = split /\t/, $_, 2;
                  push @prot, [$cat, qr/$rx/]; }
}

my %occ; my %fls; my %samples;        # what WILL change
my %pocc; my %pfls;                   # what was held back, by category
my @changed_files; my @sentinel_clash;

# Category tallies are reported on the POST-protection text, i.e. on exactly
# the bytes that get substituted. Shapes overlap on purpose (run.nostos and
# @nostos-sync/ are both also lowercase `nostos`) — the report says so.
my @cats = (
  ['SCREAMING  NOSTOS_*'      => qr/NOSTOS/      ],
  ['TitleCase  Nostos*'       => qr/Nostos/      ],
  ['snake      nostos_*'      => qr/nostos_/     ],
  ['kebab      nostos-*'      => qr/nostos-/     ],
  ['reverseDNS run.nostos*'   => qr/dev\.nostos/ ],
  ['npm scope  @nostos-sync/*'     => qr/\@nostos-sync\//  ],
  ['bare       nostos'        => qr/nostos(?![-_])/ ],
);

local $/ = undef;
my @files = grep { length } split /\n/, do { local $/; <STDIN> };

for my $path (@files) {
  next unless -f $path;
  next if -B $path;                                  # wasm, png, …
  open my $fh, '<:raw', $path or next;
  my $orig = <$fh>; close $fh;
  next unless defined $orig && $orig =~ /nostos/i;

  if ($orig =~ /\x01/) { push @sentinel_clash, $path; next; }

  # tally what each disabled category is holding back, before lifting it out
  my $t = $orig;
  for my $pr (@prot) {
    my ($cat,$rx) = @$pr;
    my $n = () = ($t =~ /$rx/g);
    if ($n) { $pocc{$cat} += $n; $pfls{$cat}{$path} = 1; }
  }

  # lift protected spans out
  my @keep; my $text = $orig;
  for my $pr (@prot) {
    my (undef,$rx) = @$pr;
    $text =~ s/($rx)/ push @keep, $1; "\x01" . $#keep . "\x01" /ge;
  }

  for my $c (@cats) {
    my ($label,$rx) = @$c;
    my $n = () = ($text =~ /$rx/g);
    if ($n) { $occ{$label} += $n; $fls{$label}{$path} = 1; }
  }

  my $new = $text;
  $new =~ s/\Q$FUC\E/$TUC/g;
  $new =~ s/\Q$FTC\E/$TTC/g;
  $new =~ s/\Q$FLC\E/$TLC/g;
  $new =~ s/\x01(\d+)\x01/$keep[$1]/g;

  next if $new eq $orig;
  push @changed_files, $path;

  if ($mode eq 'apply') {
    open my $out, '>:raw', $path or die "write $path: $!";
    print $out $new; close $out;
  } else {
    my @o = split /\n/, $orig, -1;
    my @n = split /\n/, $new,  -1;
    my @p = split /\n/, $text, -1;   # post-protection: what the shapes see
    my $clip = sub { my $s = shift; $s =~ s/^\s+//;
                     length($s) > 88 ? substr($s,0,85)."..." : $s };
    for my $i (0 .. $#o) {
      next if !defined $n[$i] || $o[$i] eq $n[$i];
      for my $c (@cats) {
        my ($label,$rx) = @$c;
        # match the shape on the PROTECTED line, not the original — otherwise a
        # line that only changed NOSTOS_PG_SLOT gets sampled as a `nostos_` change
        # when `cairn_slot` is in fact being held back.
        next unless defined $p[$i] && $p[$i] =~ /$rx/;
        next if @{ $samples{$label} // [] } >= $sample;
        push @{ $samples{$label} }, sprintf("%s:%d\n           - %s\n           + %s",
                                            $path, $i+1, $clip->($o[$i]), $clip->($n[$i]));
      }
    }
  }
}

printf "content: %d files would change\n\n", scalar @changed_files
  if $mode eq 'dry';
printf "content: %d files rewritten\n\n", scalar @changed_files
  if $mode eq 'apply';

print "  WILL RENAME\n";
printf "  %-24s %8s %7s\n", 'shape', 'occ', 'files';
for my $c (@cats) {
  my ($label) = @$c;
  next unless $occ{$label};
  printf "  %-24s %8d %7d\n", $label, $occ{$label}, scalar keys %{ $fls{$label} };
}
print "  (shapes overlap: run.nostos and \@nostos-sync/ are also counted as lowercase)\n\n";

if (%pocc) {
  print "  HELD BACK (pass the flag to include)\n";
  printf "  %-24s %8s %7s\n", 'category', 'occ', 'files';
  for my $cat (sort keys %pocc) {
    printf "  %-24s %8d %7d   --%s\n", $cat, $pocc{$cat},
           scalar keys %{ $pfls{$cat} }, $cat;
  }
  print "\n";
}

if ($mode eq 'dry' && %samples) {
  print "  SAMPLE (capped at $sample per shape)\n";
  for my $c (@cats) {
    my ($label) = @$c;
    next unless $samples{$label};
    print "  --- $label\n";
    print "      $_\n" for @{ $samples{$label} };
  }
  print "\n";
}

if (@sentinel_clash) {
  print "  !! files containing \\x01, skipped rather than guessed:\n";
  print "     $_\n" for @sentinel_clash;
  print "\n";
}
PERL

echo "=============================================================================="
echo " nostos → qairn   [mode: $MODE]"
echo "=============================================================================="
echo
echo "universe: $(wc -l < "$SELECTED" | tr -d ' ') tracked files selected, \
$(wc -l < "$EXCLUDED" | tr -d ' ') excluded by directory/lockfile flags"
echo

MODE="$MODE" SAMPLE="$SAMPLE" PROTECT="$PROTECT" \
FROM_LC="$FROM_LC" TO_LC="$TO_LC" FROM_TC="$FROM_TC" TO_TC="$TO_TC" \
FROM_UC="$FROM_UC" TO_UC="$TO_UC" \
  perl "$WORK/rewrite.pl" < "$SELECTED"

# -----------------------------------------------------------------------------
# 4. Path renames — AFTER the content sweep.
#
# Directories are renamed shallowest-first and the list is recomputed after
# each move, because renaming crates/nostos-infra/ shifts every path beneath it.
# Files are renamed afterwards by basename. Both steps skip excluded paths.
# -----------------------------------------------------------------------------
newname() { echo "$1" | sed -e "s/$FROM_UC/$TO_UC/g" -e "s/$FROM_TC/$TO_TC/g" -e "s/$FROM_LC/$TO_LC/g"; }

path_excluded() { [[ -n "$EXCLUDE_RE" ]] && echo "$1" | grep -Eq "$EXCLUDE_RE"; }

list_dirs_to_move() {
  git ls-files | while IFS= read -r f; do
    path_excluded "$f" && continue
    d="$f"
    while d="$(dirname "$d")"; [[ "$d" != "." ]]; do
      case "$(basename "$d")" in *[Cc]airn*|*NOSTOS*) echo "$d" ;; esac
    done
  done | sort -u | awk '{print gsub(/\//,"/"), $0}' | sort -n -k1,1 | cut -d' ' -f2-
}

echo "paths:"
DIR_MOVES=0
if [[ "$MODE" == "apply" ]]; then
  while :; do
    d="$(list_dirs_to_move | head -1)"
    [[ -z "$d" ]] && break
    nd="$(newname "$d")"
    [[ "$d" == "$nd" ]] && break
    if [[ -e "$nd" ]]; then echo "  !! target exists, skipping: $d -> $nd"; break; fi
    git mv "$d" "$nd"
    echo "  dir  $d -> $nd"
    DIR_MOVES=$((DIR_MOVES+1))
  done
  FILE_MOVES=0
  git ls-files | while IFS= read -r f; do
    path_excluded "$f" && continue
    b="$(basename "$f")"; nb="$(newname "$b")"
    [[ "$b" == "$nb" ]] && continue
    git mv "$f" "$(dirname "$f")/$nb"
    echo "  file $f -> $(dirname "$f")/$nb"
  done
  echo "  $DIR_MOVES directories moved (files inside moved with them)"
else
  # dry-run: show the top-most directory moves, then the standalone file moves.
  # (while-read rather than mapfile: /bin/bash on macOS is still 3.2.)
  DIRS=()
  while IFS= read -r d; do [[ -n "$d" ]] && DIRS+=("$d"); done < <(list_dirs_to_move)
  TOPS=()
  for d in "${DIRS[@]:-}"; do
    [[ -z "$d" ]] && continue
    nested=0
    for e in "${DIRS[@]}"; do [[ "$d" != "$e" && "$d" == "$e/"* ]] && nested=1 && break; done
    [[ $nested -eq 0 ]] && TOPS+=("$d")
  done
  echo "  ${#TOPS[@]} top-level directory moves (everything beneath rides along):"
  for d in "${TOPS[@]:-}"; do echo "    $d -> $(newname "$d")"; done

  FILE_N=0; FILE_SAMPLE=()
  while IFS= read -r f; do
    path_excluded "$f" && continue
    b="$(basename "$f")"; nb="$(newname "$b")"
    [[ "$b" == "$nb" ]] && continue
    FILE_N=$((FILE_N+1))
    [[ ${#FILE_SAMPLE[@]} -lt $SAMPLE ]] && FILE_SAMPLE+=("$f -> $(dirname "$f")/$nb")
  done < <(git ls-files)
  echo "  $FILE_N files whose BASENAME also changes (sample of $SAMPLE):"
  for s in "${FILE_SAMPLE[@]:-}"; do echo "    $s"; done

  SKIPPED_PATHS=$(grep -ci nostos "$EXCLUDED" 2>/dev/null || true)
  echo "  $(grep -ci nostos "$EXCLUDED" 2>/dev/null || echo 0) excluded paths contain 'cairn' and stay put"
fi

echo
if [[ "$MODE" == "dry" ]]; then
  cat <<'NEXT'
==============================================================================
 dry run — nothing was written. Re-run with --apply.

 AFTER --apply, these are NOT done by this script and must be done by hand:
   * regenerate lockfiles: cargo metadata / flutter pub get / npm install
   * rebuild the committed wasm blob (apps/atlet/flutter/web/nostos/*.wasm
     contains the old name in its bytes; sed cannot reach it)
   * rewrite the brand-metaphor sentences (see --brand-prose)
   * decide the Postgres identity as one atomic call (see --pg-identity) and
     drop the docker volume if you take it
   * `make ci` and `make pg-e2e` before anything is believed
==============================================================================
NEXT
else
  cat <<'NEXT'
==============================================================================
 applied. NOT done automatically — do these now:
   1. cargo metadata --offline >/dev/null   (regenerates Cargo.lock)
      flutter pub get / npm install          (pubspec.lock, package-lock.json)
   2. rebuild the wasm blob committed under apps/atlet/flutter/web/
   3. rewrite the brand-metaphor prose by hand
   4. make ci && make pg-e2e
==============================================================================
NEXT
fi
