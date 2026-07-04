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
NOSTOS_PG_URL_DEFAULT ?= postgresql://cairn:cairn@localhost:5433/cairn

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
test: ## Run the whole test suite.
	$(CARGO) test --workspace

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
	NOSTOS_REPLICATOR=pg NOSTOS_PG_URL=$(NOSTOS_PG_URL_DEFAULT) $(CARGO) run -p nostos-server

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
# Flutter fixtures (the pomodoro reference app — see docs/testing/persona-e2e-baseline.md)
# Deliberately NOT in `make ci`: the Rust pipeline must not pay Flutter's
# build cost per-push. If CI coverage is wanted later, add a separate GitHub
# Actions workflow triggered on fixtures/** paths only.
# ----------------------------------------------------------------------------
.PHONY: fixture-test
fixture-test: ## fixture-test: flutter fixture unit/widget suites + persona-mapping guard.
	cd fixtures/flutter/pomodoro && flutter test test/

## fixture-e2e: smoke + persona journeys on the macOS desktop target.
## Runs each integration file in its own flutter invocation: Flutter desktop
## can't foreground the same .app for multiple files in one invocation
## (known tooling limit — failures are at launch, not assertions). This
## per-file loop is the standard desktop-integration CI pattern.
## No host-caffeination / inter-file teardown needed: the journeys drive time
## through an injected FakeTicker (no wall-clock), so a throttled .app cannot
## drift. See integration_test/journeys/helpers.dart.
.PHONY: fixture-e2e
fixture-e2e: ## fixture-e2e: smoke + persona journeys on the macOS desktop target (per-file loop).
	@cd fixtures/flutter/pomodoro && \
	  for f in integration_test/smoke_test.dart integration_test/journeys/*_journey_test.dart; do \
	    echo "=== $$f ==="; \
	    flutter test "$$f" -d macos || exit 1; \
	  done

# ----------------------------------------------------------------------------
# Flutter fixtures — todo (the Supabase-backed fixture: mock today, live on
# operator credentials). Same NOT-in-`make ci` rationale as the pomodoro verbs.
# The dual-mode smoke runs a SINGLE integration file per invocation, so it does
# NOT hit the per-file aggregate-launch limit noted on fixture-e2e above.
# ----------------------------------------------------------------------------
.PHONY: fixture-todo-test
fixture-todo-test: ## fixture-todo-test: todo fixture unit/widget suites (mocked ports).
	cd fixtures/flutter/todo && flutter test test/

## fixture-todo-smoke: dual-mode smoke, MOCK mode (no credentials needed)
.PHONY: fixture-todo-smoke
fixture-todo-smoke:
	cd fixtures/flutter/todo && flutter test integration_test/smoke_auth_test.dart -d macos

## fixture-todo-smoke-live: same smoke against real Supabase (needs env.json — see env.example.json)
.PHONY: fixture-todo-smoke-live
fixture-todo-smoke-live:
	cd fixtures/flutter/todo && flutter test integration_test/smoke_auth_test.dart -d macos --dart-define-from-file=env.json
