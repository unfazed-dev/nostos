#!/usr/bin/env bash
# Shared constants for the Nostos "local live" harness (W5). Sourced by the
# other tool/nostos_*.sh scripts — not meant to be run directly.
#
# "Local live" stands in for a real Supabase project (W0b is
# operator-blocked): real nostos-server + real docker Postgres + real HS256
# JWTs signed with the dev secret below. Same code paths a Supabase-JWKS
# deploy exercises (auth -> tenant-scoped reads -> tenant-enforced
# write-back), just HS256 instead of RS256/ES256 (auth.rs routes on the JWT's
# `alg` header, so this is a legitimate substitution, not a shortcut around
# the auth layer).

set -euo pipefail

# Repo root, resolved from this script's location (fixtures/flutter/todo/tool/).
NOSTOS_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
NOSTOS_FIXTURE_DIR="$NOSTOS_REPO_ROOT/fixtures/flutter/todo"
NOSTOS_STATE_DIR="$NOSTOS_FIXTURE_DIR/.nostos"

# Dev-only shared secret — never used against a real Postgres/Supabase
# project. Regenerated state lives entirely under .nostos/ (gitignored).
NOSTOS_DEV_JWT_SECRET="nostos-todo-w5-local-live-dev-secret-do-not-use-in-production"

NOSTOS_PG_URL="postgresql://cairn:cairn@localhost:5433/cairn"
NOSTOS_PUBLICATION="cairn_pub_todo_w5"
NOSTOS_SLOT="cairn_slot_todo_w5"
NOSTOS_TENANT_COLUMN="user_id"
NOSTOS_BIND="127.0.0.1:8810"
NOSTOS_WS_PATH="/sync"
NOSTOS_WS_URL="ws://$NOSTOS_BIND$NOSTOS_WS_PATH"
NOSTOS_HEALTH_URL="http://$NOSTOS_BIND/healthz"

NOSTOS_DEV_LOG="$NOSTOS_STATE_DIR/nostos-dev.log"
NOSTOS_DEV_PID_FILE="$NOSTOS_STATE_DIR/nostos-dev.pid"
