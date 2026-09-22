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
# nostos-server connects as the least-privilege `cairn_writer` role (NOT the
# `cairn` superuser) — see docker/pg-init/02-nostos-role.sql. A compromised
# server can then only touch synced tables, not the whole DB (ADR-0013/0018).
NOSTOS_PG_URL_DEFAULT ?= postgresql://cairn_writer:cairn_writer_dev_pw@localhost:5433/cairn

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
# and the app slots (cairn_slot, atlet_*) are left alone.
# Uses the `cairn` superuser (not NOSTOS_PG_URL_DEFAULT's least-privilege
# cairn_writer): the tests create slots/publications and TRUNCATE.
NOSTOS_E2E_PG_URL ?= postgres://cairn:cairn@localhost:5433/cairn
.PHONY: pg-e2e
pg-e2e: ## Real-Postgres e2e suite; drops leaked inactive e2e_* slots first.
	@docker compose -f docker/docker-compose.yml exec -T postgres \
	  psql -U cairn -d cairn -tAc \
	  "SELECT count(pg_drop_replication_slot(slot_name)) FROM pg_replication_slots WHERE NOT active AND (slot_name LIKE 'e2e_%' OR slot_name LIKE 'repro_%')" \
	  | sed 's/^/swept leaked e2e slots: /'
	NOSTOS_E2E_PG=1 NOSTOS_PG_URL=$(NOSTOS_E2E_PG_URL) $(CARGO) test -p nostos-infra --features pg --no-fail-fast -- --test-threads=1
# nostos-cli's pg suite too: `nostos link --mode direct` generates SQL, and the
# only place a generator bug shows up is Postgres refusing (or silently
# mis-scoping) it. e2e_pg_direct_sql owns the `cairn` schema, hence -threads=1.
	NOSTOS_E2E_PG=1 NOSTOS_PG_URL=$(NOSTOS_E2E_PG_URL) $(CARGO) test -p nostos-cli --no-fail-fast -- --test-threads=1

# dev-stack: real-Postgres quickstart — compose up, wait for the publication,
# then run nostos-server against it with PgReplicator. The readiness poll gates
# on `cairn_pub` existing (not just `pg_isready`): during first init the
# entrypoint runs a *temporary* server to apply pg-init scripts, then restarts
# into the real one, so a plain readiness probe flips accepting -> rejecting
# -> accepting and can fool `sleep 3`. The publication only exists once the
# real server is up AND pg-init/01-sources.sql has run. (Same gate the B3
# e2e-pg CI job uses.) Ctrl-C stops the server; `make pg-down` tears down PG.
.PHONY: dev-stack
dev-stack: ## Real-Postgres quickstart: compose up + run server with PgReplicator.
	docker compose -f docker/docker-compose.yml up -d
	@echo "waiting for postgres (polling for cairn_pub publication)…"
	@for i in $$(seq 1 60); do \
	  if docker compose -f docker/docker-compose.yml exec -T postgres \
	       psql -U cairn -d cairn -tAc \
	       "SELECT 1 FROM pg_publication WHERE pubname='cairn_pub'" \
	       | grep -q 1; then \
	    echo "Postgres ready (cairn_pub present) after $${i}s"; \
	    break; \
	  fi; \
	  sleep 1; \
	done
	@docker compose -f docker/docker-compose.yml exec -T postgres \
	  psql -U cairn -d cairn -tAc \
	  "SELECT 1 FROM pg_publication WHERE pubname='cairn_pub'" | grep -q 1 \
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
# PowerSync (the comparison harness — self-host, not a throughput race).
# Brings up Postgres + the PowerSync Service together so the powersync_smoke
# test can assert PowerSync ingests from the same PG Nostos reads. See
# docs/COMPARISON.md for why the live head-to-head is deferred.
# ----------------------------------------------------------------------------
PS_COMPOSE := -f docker/docker-compose.yml -f docker/docker-compose.powersync.yml

.PHONY: ps-up
ps-up: ## Start Postgres + PowerSync (the comparison stack).
	docker compose $(PS_COMPOSE) up -d postgres powersync
	@echo "PowerSync sync API: http://localhost:8080"
	@echo "Run the smoke test: NOSTOS_POWERSYNC=1 cargo test -p nostos-infra --test powersync_smoke -- --nocapture"

.PHONY: ps-down
ps-down: ## Stop Postgres + PowerSync.
	docker compose $(PS_COMPOSE) down

.PHONY: ps-logs
ps-logs: ## Tail PowerSync logs.
	docker compose $(PS_COMPOSE) logs -f powersync

# ----------------------------------------------------------------------------
# Benchmark — the Week-1 deliverable
# ----------------------------------------------------------------------------
.PHONY: bench
bench: ## Run the throughput benchmark (the headline ≥5× vs PowerSync chart).
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
# no auth, no Chrome PNA gate. Override the plan dir: PLAYBOOK_DIR=plans/<slug>.
# render-playbook.py is a GENERIC agent-native plan.mdx renderer (stdlib-only;
# kept byte-identical with applications/p2/scripts/render-playbook.py): Mermaid,
# Code, Table, Callout, Checklist, QuestionForm, FileTree, TabsBlock,
# AnnotatedCode, Diagram, Columns + markdown. See its header docstring for limits.
# ----------------------------------------------------------------------------
PLAYBOOK_DIR ?= plans/nostos-supabase-realtime

.PHONY: playbook
playbook: ## Render the playbook (plan.mdx -> playbook.html) and open it in the browser.
	python3 scripts/render-playbook.py $(PLAYBOOK_DIR)/plan.mdx
	open $(PLAYBOOK_DIR)/playbook.html
