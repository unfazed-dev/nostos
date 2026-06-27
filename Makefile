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

.PHONY: pg-down
pg-down: ## Stop Postgres.
	docker compose -f docker/docker-compose.yml down

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
