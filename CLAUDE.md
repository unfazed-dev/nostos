# Nostos — Project Memory

## What this is
Rust-native local-first sync engine: Postgres logical replication → Rust fan-out server →
on-device SQLite, offline-capable, Apache-2.0 end to end. Moat proof: 2,618,601 ops/sec
aggregate fan-out @ 1k clients, 0.00% drops (eval-only: FakeReplicator loopback; median of 3,
spread 2.52M–2.68M, measured 2026-09-02 on the Arc-shared sequential fan-out — the earlier
833,307 was the old fan-out, preserved as historical), plus the
FULL-PATH real-PG → client-apply figure measured 2026-08-24: ~34.8k–36.2k rows/sec sustained,
zero-drop at buffer 32768 — see benches/results/RESULTS.md. Never cite a cross-stage ratio
against another engine's figures, only same-stage, same-units comparisons per
docs/BENCHMARK-METHODOLOGY.md. (Week-1 baseline was 142k, preserved as historical.)

## Crate map (hexagonal — dependencies point inward, violations fail review)
| crate | role | may depend on |
|---|---|---|
| nostos-domain | pure types + invariants (Predicate, Lsn, events). Zero I/O, zero async | nothing |
| nostos-application | use-cases + port traits (FanOutService, SessionStore, ReplicatorStream, SyncAuth) | domain |
| nostos-infra | adapters: PgReplicator (feature `pg`), FakeReplicator, WS transport, wire codec, auth, write-back, oplog, push senders | application, domain |
| nostos-server | composition root — the axum binary | domain, application, infra, license |
| nostos-core | client apply engine + Storage trait. WASM-clean: no tokio, no SQLite | domain |
| nostos-client | native client: SqliteStorage (rusqlite) + tokio SyncClient | core, domain, infra |
| nostos-ffi-wasm | wasm-bindgen bridge over nostos-core | core, domain |
| nostos-bench | throughput harness — honest numbers (drops reported, env recorded) | domain, application, infra |
| nostos-license | HMAC-signed offline license claims — minted by nostos-cloud, verified by nostos-server; keeps crypto deps out of domain | domain |
| nostos-push | standalone push daemon nostos-pushd (composition root, ADR-0038) | domain, infra |
| nostos-cli | the `nostos` CLI — init, dev, doctor, deploy, link, pull, gen, rules init/edit/check, push | domain, infra |
| nostos-cloud | control plane (bin `nostos-cloud`): accounts, projects, API keys, Stripe billing, license minting | domain, infra, license |

`unsafe` is forbidden workspace-wide (all Cargo workspace members). The one
exception is machine-generated FFI glue in the non-member crate
`sdk/nostos_flutter/rust` (flutter_rust_bridge codegen — ADR-0015 addendum);
hand-written `unsafe` is forbidden everywhere. Clippy pedantic is on; CI fails
on warnings.

## Verbs (the only loops you need)
- `make ci` — fmt-check + clippy (-D warnings) + full test suite. Gate for every change.
- `scripts/check.sh [area]` (`make check AREA=`) — each CI job's steps locally, by job name; local green = CI green.
- `make worktree NAME=<task>` → work in `.worktrees/<task>` (branched from `origin/main`; the main clone stays
  on `main`) → PR, merge on GitHub → `make worktree-rm NAME=<task>`. `make hooks` once per clone: pre-push refuses `main`.
- `cargo test -p <crate>` — focused iteration.
- `docker compose -f docker/docker-compose.yml up -d` then
  `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://nostos:nostos@localhost:5433/nostos cargo test -p nostos-infra --features pg -- --test-threads=1`
  — the real-Postgres e2e. Without `NOSTOS_E2E_PG=1` the tests self-skip and
  report a false-positive pass. (Check docker/docker-compose.yml for the
  actual port/credentials.) The `--test-threads=1` is REQUIRED: several pg
  e2e binaries TRUNCATE the shared `tasks` table and assert exact snapshot
  contents — every test file documents this, and a parallel within-binary
  run races (caught 2026-08-17: snapshot test saw the concurrent test's
  seed row as a 4th row). `make pg-e2e` wraps all of this AND first drops
  leaked INACTIVE `e2e_*`/`repro_*` slots — an aborted run leaves them behind
  (slot names carry the pid) and the next run then fails with
  "all replication slots are in use" (caught 2026-09-21: 7 spurious failures).
- `make bench` — throughput benchmark. Record environment; report drop rates; never compare
  eval-only numbers against end-to-end numbers.
- `cargo run -p nostos-client --example reactive_scroll` — end-to-end native demo.
- `nostos rules init|edit|check` — generate/edit/validate `nostos_rules.toml` (ADR-0031: `all`/`toggles`/`hand` sync modes). No file on disk = `all` (zero-config).

## Conventions
- Commits: single line, conventional prefix, no author mentions.
- Toolchains track LATEST stable (standing rule 2026-09-21): `rust-toolchain.toml` is `channel = "stable"`,
  `rust-version`/README badge/CONTRIBUTING/ci.yml `flutter-version`/pubspec `sdk:` floors name the current
  release. At session start run `rustup update stable` + `fvm install <latest stable> && fvm global <it>`
  (check `rustup check`, `fvm releases`); bump the named versions when they move. Never pin older.
- Architectural decisions become `docs/adr/NNNN-<slug>.md` (next free number); code cites its ADR.
- Measure before optimize: perf changes ship with before/after numbers or get reverted
  (Tier-5 index revert is the precedent — docs/ROADMAP.md).
- Deliberate shortcuts carry a `ponytail:` comment naming the ceiling and upgrade path.
- The wire protocol stays human-debuggable JSON until a measurement says otherwise.
- Plans live in docs/plans/; personas in .claude/agents/; project skills in .claude/skills/.

## Reading order for a fresh agent
docs/ARCHITECTURE.md → ADRs 0001, 0003, 0009, 0011, 0012, 0013 → docs/ROADMAP.md.
Strategy/positioning: docs/STRATEGY.md. Benchmark rules: docs/BENCHMARK-METHODOLOGY.md.

---

# context-mode — MANDATORY routing rules

You have context-mode MCP tools available. These rules are NOT optional — they protect your context window from flooding. A single unrouted command can dump 56 KB into context and waste the entire session.

## BLOCKED commands — do NOT attempt these

### curl / wget — BLOCKED
Any Bash command containing `curl` or `wget` is intercepted and replaced with an error message. Do NOT retry.
Instead use:
- `ctx_fetch_and_index(url, source)` to fetch and index web pages
- `ctx_execute(language: "javascript", code: "const r = await fetch(...)")` to run HTTP calls in sandbox

### Inline HTTP — BLOCKED
Any Bash command containing `fetch('http`, `requests.get(`, `requests.post(`, `http.get(`, or `http.request(` is intercepted and replaced with an error message. Do NOT retry with Bash.
Instead use:
- `ctx_execute(language, code)` to run HTTP calls in sandbox — only stdout enters context

### WebFetch — BLOCKED
WebFetch calls are denied entirely. The URL is extracted and you are told to use `ctx_fetch_and_index` instead.
Instead use:
- `ctx_fetch_and_index(url, source)` then `ctx_search(queries)` to query the indexed content

## REDIRECTED tools — use sandbox equivalents

### Bash (>20 lines output)
Bash is ONLY for: `git`, `mkdir`, `rm`, `mv`, `cd`, `ls`, `npm install`, `pip install`, and other short-output commands.
For everything else, use:
- `ctx_batch_execute(commands, queries)` — run multiple commands + search in ONE call
- `ctx_execute(language: "shell", code: "...")` — run in sandbox, only stdout enters context

### Read (for analysis)
If you are reading a file to **Edit** it → Read is correct (Edit needs content in context).
If you are reading to **analyze, explore, or summarize** → use `ctx_execute_file(path, language, code)` instead. Only your printed summary enters context. The raw file content stays in the sandbox.

### Grep (large results)
Grep results can flood context. Use `ctx_execute(language: "shell", code: "grep ...")` to run searches in sandbox. Only your printed summary enters context.

## Tool selection hierarchy

1. **GATHER**: `ctx_batch_execute(commands, queries)` — Primary tool. Runs all commands, auto-indexes output, returns search results. ONE call replaces 30+ individual calls.
2. **FOLLOW-UP**: `ctx_search(queries: ["q1", "q2", ...])` — Query indexed content. Pass ALL questions as array in ONE call.
3. **PROCESSING**: `ctx_execute(language, code)` | `ctx_execute_file(path, language, code)` — Sandbox execution. Only stdout enters context.
4. **WEB**: `ctx_fetch_and_index(url, source)` then `ctx_search(queries)` — Fetch, chunk, index, query. Raw HTML never enters context.
5. **INDEX**: `ctx_index(content, source)` — Store content in FTS5 knowledge base for later search.

## Subagent routing

When spawning subagents (Agent/Task tool), the routing block is automatically injected into their prompt. Bash-type subagents are upgraded to general-purpose so they have access to MCP tools. You do NOT need to manually instruct subagents about context-mode.

## Output constraints

- Keep responses under 500 words.
- Write artifacts (code, configs, PRDs) to FILES — never return them as inline text. Return only: file path + 1-line description.
- When indexing content, use descriptive source labels so others can `ctx_search(source: "label")` later.

## ctx commands

| Command | Action |
|---------|--------|
| `ctx stats` | Call the `ctx_stats` MCP tool and display the full output verbatim |
| `ctx doctor` | Call the `ctx_doctor` MCP tool, run the returned shell command, display as checklist |
| `ctx upgrade` | Call the `ctx_upgrade` MCP tool, run the returned shell command, display as checklist |
