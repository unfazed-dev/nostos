#!/bin/sh
# ADR-0046: in DIR, symlink each pre-rename binary name to the current binary
# (the Valkey pattern), so scripts and unit files that still call the old name
# keep working until 1.0. Used by release.yml's tarballs and the Dockerfile.
# Guarded: no self-link while the names are equal, never clobbers a file, and
# a binary DIR does not ship is skipped.
set -eu
dir=${1:?usage: legacy-binary-names.sh DIR}

link() {
  if [ "$1" != "$2" ] && [ -e "$dir/$1" ] && [ ! -e "$dir/$2" ] && [ ! -L "$dir/$2" ]; then
    ln -s "$1" "$dir/$2"
  fi
}

# Each legacy name sits on its own held line so the rename leaves it alone.
link nostos \
  cairn # rename:hold — pre-rename binary name, symlinked until 1.0 (ADR-0046)
link nostos-server \
  cairn-server # rename:hold — pre-rename binary name, symlinked until 1.0 (ADR-0046)
link nostos-pushd \
  cairn-pushd # rename:hold — pre-rename binary name, symlinked until 1.0 (ADR-0046)
link nostos-cloud \
  cairn-cloud # rename:hold — pre-rename binary name, symlinked until 1.0 (ADR-0046)
