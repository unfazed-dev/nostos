# =============================================================================
# Nostos — Makefile. The founder's control panel.
# Usage: `make <target>`. Run `make help` for the index.
# =============================================================================
SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help
COLOR  := \033[1;36m
RESET  := \033[0m
BENCH_RESULTS_DIR ?= benches/results

# Number of concurrent clients the websocket swarm benchmark spins up.
BENCH_CLIENTS ?= 1000,5000,10000
# How many replication events to push per client-tier during a bench run.
BENCH_EVENTS  ?= 100000

# Default Postgres URL for `make dev-stack` — mirrors docker/docker-compose.yml
# (host port 5433 → container 5432, user/db/pass = nostos). Override by setting
# this env var if you point dev-stack at a different Postgres.
# nostos-server connects as the least-privilege `nostos_writer` role (NOT the
# `nostos` superuser) — see docker/pg-init/02-nostos-role.sql. A compromised
# server can then only touch synced tables, not the whole DB (ADR-0013/0018).
NOSTOS_PG_URL_DEFAULT ?= postgresql://nostos_writer:nostos_writer_dev_pw@localhost:5433/nostos

CARGO := cargo

.PHONY: help
help: ## Show this index.
	@printf "$(COLOR)Nostos — targets$(RESET)\n"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  $(COLOR)%-22s$(RESET) %s\n", $$1, $$2}'

# ----------------------------------------------------------------------------
# Bootstrap
# ----------------------------------------------------------------------------
.PHONY: setup
setup: ## Install rust toolchain (rustup picks up rust-toolchain.toml) + check.
	@rustup show active-toolchain || rustup toolchain install
	@$(CARGO) --version
	@echo "✓ toolchain ready"

.PHONY: check-targets
check-targets: ## Verify SDK cross-compile targets are installed.
	@rustup target list --installed | grep -qE 'wasm32-unknown-unknown' && echo "✓ wasm32" || echo "✗ wasm32 missing"
	@rustup target list --installed | grep -qE 'aarch64-linux-android' && echo "✓ android" || echo "✗ android missing"
	@rustup target list --installed | grep -qE 'aarch64-apple-ios' && echo "✓ ios" || echo "✗ ios missing"

# ----------------------------------------------------------------------------
# Worktrees — one task = one worktree = one branch at .worktrees/<name>; the
# main clone stays on main (docs/ci/setup.md). Claude Code's worktrees land
# here too, via the WorktreeCreate hook (scripts/worktree-create.sh).
# ----------------------------------------------------------------------------
.PHONY: worktree
worktree: ## New task worktree: .worktrees/<NAME> on branch <NAME>, from origin/main.
	@test -n "$(NAME)" || { echo "usage: make worktree NAME=<name>"; exit 2; }
# Branches from origin/main (`git fetch origin` first for the real tip); falls
# back to local main when origin/main is missing (no remote yet, never fetched).
	@base=origin/main; git rev-parse -q --verify "$$base" >/dev/null || base=main; \
	  git worktree add -b "$(NAME)" ".worktrees/$(NAME)" "$$base"

.PHONY: worktree-rm
worktree-rm: ## Remove .worktrees/<NAME> + its branch once its PR merged on GitHub.
	@test -n "$(NAME)" || { echo "usage: make worktree-rm NAME=<name>"; exit 2; }
# `branch -d`, not -D: an unmerged branch survives. `git pull` main first so a
# merge on GitHub counts as merged.
	@branch=$$(git -C ".worktrees/$(NAME)" branch --show-current); \
	  git worktree remove ".worktrees/$(NAME)" && git branch -d "$$branch"

.PHONY: hooks
hooks: ## Once per clone: git hooks from scripts/hooks (pre-push refuses main).
	git config core.hooksPath scripts/hooks

# ----------------------------------------------------------------------------
# Build / test / lint
# ----------------------------------------------------------------------------
.PHONY: build
build: ## Build all crates (debug).
	$(CARGO) build --workspace

.PHONY: build-release
build-release: ## Build all crates (release, optimized for benchmarking).
	$(CARGO) build --workspace --release

.PHONY: test
test: ## Run the whole test suite (includes #[ignore]'d scale-regression floors).
	$(CARGO) test --workspace -- --include-ignored

.PHONY: fmt
fmt: ## Format the codebase.
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Fail if anything is unformatted.
	$(CARGO) fmt --all -- --check

.PHONY: clippy
clippy: ## Lint with clippy (workspace lints apply; -D warnings makes it strict).
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: lint
lint: fmt-check clippy ## fmt-check + clippy (what CI runs).

.PHONY: ci
ci: lint test ## Local mirror of CI: lint + test.
	@echo "✓ CI clean locally"

# check: the one root check (docs/ci/setup.md). Every CI job has a same-named
# area in scripts/check.sh; AREA=lint-test is `make ci`.
AREA ?= all
.PHONY: check
check: ## Local green = CI green: every CI job's area, or one with AREA=<job>.
	@./scripts/check.sh $(AREA)

.PHONY: sdk-e2e
sdk-e2e: ## Run all 10 SDK live-replication E2E slices (9 PUSH+ECHO + flutter PUSH-only, macOS). (flutter restored 2026-08-05)
	@./scripts/sdk-e2e.sh

# ----------------------------------------------------------------------------
# Run the server
# ----------------------------------------------------------------------------
.PHONY: run
run: ## Run the sync server (port 8800 by default; see .env).
	$(CARGO) run --release --bin nostos-server

# ----------------------------------------------------------------------------
# Postgres (for the real pg replicator — not needed for the Week-1 synth bench)
# ----------------------------------------------------------------------------
.PHONY: pg-up
pg-up: ## Start a Postgres 16 with logical replication enabled (docker).
	docker compose -f docker/docker-compose.yml up -d postgres

# pg-e2e: the real-Postgres e2e suite (NOSTOS_E2E_PG=1, --test-threads=1 — see
# CLAUDE.md). Sweeps INACTIVE e2e_*/repro_* slots first: every test names its
# slot after its pid, so an aborted run (Ctrl-C, PG restart) leaks them and the
# next run dies with "all replication slots are in use" (max 20). Live slots
# and the app slots (nostos_slot, atlet_*) are left alone.
# Uses the `nostos` superuser (not NOSTOS_PG_URL_DEFAULT's least-privilege
# nostos_writer): the tests create slots/publications and TRUNCATE.
NOSTOS_E2E_PG_URL ?= postgres://nostos:nostos@localhost:5433/nostos
.PHONY: pg-e2e
pg-e2e: ## Real-Postgres e2e suite; drops leaked inactive e2e_* slots first.
	@docker compose -f docker/docker-compose.yml exec -T postgres \
	  psql -U nostos -d nostos -tAc \
	  "SELECT count(pg_drop_replication_slot(slot_name)) FROM pg_replication_slots WHERE NOT active AND (slot_name LIKE 'e2e_%' OR slot_name LIKE 'repro_%')" \
	  | sed 's/^/swept leaked e2e slots: /'
	NOSTOS_E2E_PG=1 NOSTOS_PG_URL=$(NOSTOS_E2E_PG_URL) $(CARGO) test -p nostos-infra --features pg --no-fail-fast -- --test-threads=1
# nostos-cli's pg suite too: `nostos link --mode direct` generates SQL, and the
# only place a generator bug shows up is Postgres refusing (or silently
# mis-scoping) it. e2e_pg_direct_sql owns the `nostos` schema, hence -threads=1.
	NOSTOS_E2E_PG=1 NOSTOS_PG_URL=$(NOSTOS_E2E_PG_URL) $(CARGO) test -p nostos-cli --no-fail-fast -- --test-threads=1

.PHONY: supabase-e2e
supabase-e2e: ## Direct mode against a REAL Supabase stack (needs `supabase start` in $$SB_DIR).
# The pg e2e above stubs `auth`, `realtime` and `net`, so everything
# Supabase-specific is unproven there: how PostgREST renders xid8, whether
# PT410 becomes a 410, whether the realtime.messages policy actually refuses
# the wrong tenant, whether the Edge Function can read the token registry.
# Every bug this has found lived in one of those. SB_DIR defaults to a sibling
# `supabase/` project dir; override it.
	@test -n "$$SB_DIR" || { echo "set SB_DIR to a supabase project dir (one with config.toml)"; exit 2; }
	cd $$SB_DIR && supabase status -o json > /dev/null || { echo "run \`supabase start\` in $$SB_DIR first"; exit 2; }
	NOSTOS_SB_ANON_KEY=$$(cd $$SB_DIR && supabase status -o json | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>console.log(JSON.parse(s).ANON_KEY))") \
	NOSTOS_SB_SERVICE_KEY=$$(cd $$SB_DIR && supabase status -o json | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>console.log(JSON.parse(s).SERVICE_ROLE_KEY))") \
	node scripts/e2e-supabase-direct.mjs

.PHONY: web-conformance
web-conformance: ## The browser-Worker leg of nostos_core::conformance (OPFS, headless Chromium).
# Built WITH the off-by-default `conformance` feature: the cases must never
# ship in an app's .wasm (ADR-0015's size budget), so the shipping bundle is
# rebuilt straight after.
	wasm-pack build crates/nostos-ffi-wasm --target web --out-dir pkg-web --features conformance
	cd sdk/nostos_web && npx playwright test e2e/conformance.spec.cjs --reporter=line
	wasm-pack build crates/nostos-ffi-wasm --target web --out-dir pkg-web

# dev-stack: real-Postgres quickstart — compose up, wait for the publication,
# then run nostos-server against it with PgReplicator. The readiness poll gates
# on `nostos_pub` existing (not just `pg_isready`): during first init the
# entrypoint runs a *temporary* server to apply pg-init scripts, then restarts
# into the real one, so a plain readiness probe flips accepting -> rejecting
# -> accepting and can fool `sleep 3`. The publication only exists once the
# real server is up AND pg-init/01-sources.sql has run. (Same gate the B3
# e2e-pg CI job uses.) Ctrl-C stops the server; `make pg-down` tears down PG.
.PHONY: dev-stack
dev-stack: ## Real-Postgres quickstart: compose up + run server with PgReplicator.
	docker compose -f docker/docker-compose.yml up -d
	@echo "waiting for postgres (polling for nostos_pub publication)…"
	@for i in $$(seq 1 60); do \
	  if docker compose -f docker/docker-compose.yml exec -T postgres \
	       psql -U nostos -d nostos -tAc \
	       "SELECT 1 FROM pg_publication WHERE pubname='nostos_pub'" \
	       | grep -q 1; then \
	    echo "Postgres ready (nostos_pub present) after $${i}s"; \
	    break; \
	  fi; \
	  sleep 1; \
	done
	@docker compose -f docker/docker-compose.yml exec -T postgres \
	  psql -U nostos -d nostos -tAc \
	  "SELECT 1 FROM pg_publication WHERE pubname='nostos_pub'" | grep -q 1 \
	  || { echo "Postgres did not become ready in 60s — try 'make pg-logs'"; exit 1; }
	NOSTOS_REPLICATOR=pg NOSTOS_PG_URL=$(NOSTOS_PG_URL_DEFAULT) NOSTOS_WRITE_TABLES=tasks,providers,clients,availabilities,appointments,invoices $(CARGO) run -p nostos-server

.PHONY: pg-down
pg-down: ## Stop Postgres.
	docker compose -f docker/docker-compose.yml down

# web-demo: rebuild the WASM pkg (if stale) then start the Vite dev server for
# the /demo page. Run in a SECOND terminal alongside `make dev-stack` — the demo
# page connects cross-origin to the server's WS (default ws://localhost:8800/sync)
# so no Vite WS proxy is wired. wasm-pack is a no-op when nothing changed.
.PHONY: web-demo
web-demo: ## Rebuild the WASM pkg + start the web dev server (run alongside dev-stack).
	wasm-pack build crates/nostos-ffi-wasm --target web
	cd web && npm install && npm run dev

.PHONY: pg-logs
pg-logs: ## Tail Postgres logs.
	docker compose -f docker/docker-compose.yml logs -f postgres

# ----------------------------------------------------------------------------
# Benchmark — the Week-1 deliverable
# ----------------------------------------------------------------------------
.PHONY: bench
bench: ## Run the throughput benchmark (the headline aggregate fan-out chart).
	@mkdir -p $(BENCH_RESULTS_DIR)
	$(CARGO) run --release --bin nostos-bench -- \
		--clients $(BENCH_CLIENTS) \
		--events $(BENCH_EVENTS) \
		--out-dir $(BENCH_RESULTS_DIR)
	@printf "$(COLOR)✓ results written to $(BENCH_RESULTS_DIR)/$(RESET)\n"

.PHONY: bench-router
bench-router: ## Pure-router micro-benchmark (no network; isolates fan-out).
	$(CARGO) bench --workspace

.PHONY: results
results: ## Print the latest benchmark results as a table.
	@cat $(BENCH_RESULTS_DIR)/RESULTS.md 2>/dev/null || echo "No results yet. Run 'make bench'."

# ----------------------------------------------------------------------------
# Cleanliness
# ----------------------------------------------------------------------------
.PHONY: clean
clean: ## Remove all build artifacts.
	$(CARGO) clean
	rm -rf $(BENCH_RESULTS_DIR) target/criterion

.PHONY: git-init
git-init: ## Initialize git (idempotent) + initial commit.
	@if [ ! -d .git ]; then git init -q && echo "✓ git initialized"; else echo "✓ git already initialized"; fi

# ----------------------------------------------------------------------------
# Playbook (agent-native visual-plan MDX -> standalone HTML).
# Edit plan.mdx, then `make playbook` regenerates playbook.html and opens it.
# Self-contained: Mermaid via CDN, real tables/callouts — no Plan UI bridge,
# no auth, no Chrome PNA gate. Override the guide dir: PLAYBOOK_DIR=docs/guides/<slug>.
# render-playbook.py is a GENERIC agent-native plan.mdx renderer (stdlib-only;
# kept byte-identical with applications/p2/scripts/render-playbook.py): Mermaid,
# Code, Table, Callout, Checklist, QuestionForm, FileTree, TabsBlock,
# AnnotatedCode, Diagram, Columns + markdown. See its header docstring for limits.
# ----------------------------------------------------------------------------
PLAYBOOK_DIR ?= docs/guides/supabase-realtime

.PHONY: playbook
playbook: ## Render the playbook (plan.mdx -> playbook.html) and open it in the browser.
	python3 scripts/render-playbook.py $(PLAYBOOK_DIR)/plan.mdx
	open $(PLAYBOOK_DIR)/playbook.html
