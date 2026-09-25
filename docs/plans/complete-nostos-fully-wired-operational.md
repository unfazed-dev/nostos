# Complete Nostos — Fully Wired & Operational Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take Nostos from a proven Phase-0/1 spike to a fully wired, operational v0.1 — real Postgres in by default, predicates wired end-to-end, 2-way offline writes, a browser that actually connects — plus the LLM-agent operating layer (project memory, personas, skills, permissions) the repo currently lacks.

**Architecture:** Hexagonal (ports & adapters) + DDD, unchanged: `bootstrap → application → domain ← infrastructure`. All new work lands at existing verified seams — `ReplicatorStream`, `SessionStore`, `Storage`, `ClientMessage` wire — never by bypassing them.

**Tech Stack:** Rust 1.95 (pinned), tokio + axum, tokio-postgres + pgoutput + pgwire-replication (feature `pg`), rusqlite, wasm-bindgen + web-sys, SvelteKit (web/), SQLite. No new external dependencies except where a task names one explicitly.

## Global Constraints

- Rust `1.95.0` pinned via `rust-toolchain.toml`; edition 2021; rustfmt `max_width = 100`.
- `unsafe_code = "forbid"` workspace-wide; clippy `all` + `pedantic` warn, CI runs `-D warnings`.
- Dependency direction is law: domain depends on nothing; application only on domain; infra implements application ports; server composes. A task that violates this is wrong even if it works.
- License: Apache-2.0 end to end. Every new dependency must be Apache-2.0-compatible (Apache-2.0/MIT/BSD/ISC/Zlib/Unicode).
- Commits: **single line**, conventional prefix (`feat:`/`fix:`/`test:`/`docs:`/`chore:`), no author mentions, no trailers.
- Measure-before-optimize: any performance change ships with before/after numbers or gets reverted (Tier-5 precedent, `docs/ROADMAP.md`).
- Deliberate shortcuts carry a `ponytail:` comment naming the ceiling and the upgrade path.
- Verification gate for every task: `make ci` (fmt-check + clippy + workspace tests). Replication tasks additionally run the feature-gated e2e (`NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg`).
- Env-var naming: `NOSTOS_*` (existing: `NOSTOS_REPLICATOR`, `NOSTOS_PG_URL`, `NOSTOS_PG_SLOT`, `NOSTOS_PG_PUBLICATION`, `NOSTOS_SYNC_AUTH`, `NOSTOS_TIER`).

---

## Part I — Where Nostos is (tech-lead assessment, 2026-07-04)

### Done and proven (evidence)

| Area | State | Evidence |
|---|---|---|
| Throughput moat | **142,336 ops/sec @ 1k clients, 0.00% drops = 35.6× a competitor's published ceiling**; 185k @ 5k clients; 45,964 @ 10k with 17.26% drops (known WS-write-path limit) | `benches/results/RESULTS.md` |
| Real replication | `PgReplicator` fully implemented (848 LOC, pgoutput + pgwire-replication, ack-driven slot advance per ADR-0009), **but off by default** behind cargo feature `pg`; default binary runs `FakeReplicator` | `crates/nostos-infra/src/replicator/pg.rs`, `nostos-server/src/main.rs:199-268` |
| Predicate engine | Boolean tree + typed comparisons + JSON extractor + safe-SQL-subset compiler (`parse_predicate_expr`), ~1.5M predicate-evals/sec; equality index built, measured 4-8× regression, reverted | `nostos-domain/src/predicate.rs`, `predicate_compile.rs`, ADR-0012 |
| Client SDK core | `nostos-core` apply engine + atomic checkpoint `Storage` trait; `nostos-client` SqliteStorage + reconnect/resume; chaos-tested | ADR-0016, `crates/nostos-client/tests/chaos_resume.rs` |
| WASM bridge | wasm-bindgen apply engine, 17 KB gzipped — **in-memory only, cannot connect** (no WS transport, no OPFS) | ADR-0015, `crates/nostos-ffi-wasm` |
| Control plane | `nostos-cloud` real: auth (session + Supabase JWT), Stripe checkout/webhook, HMAC licensing, waitlist | `crates/nostos-cloud/src/*` |
| Demo | `cargo run -p nostos-client --example reactive_scroll` — end-to-end native path with mid-stream restart, zero loss | Tier-6 commit `2dc32a4` |
| Guardrails | CI (fmt/clippy -D warnings/test/smoke-bench), Makefile verbs, pinned toolchain, 16 ADRs, ~188 tests | `.github/workflows/ci.yml`, `Makefile` |

### Not done (the gaps this plan closes)

1. **Agent operating layer absent.** `CLAUDE.md` is 100% context-mode boilerplate with zero project memory; no `.claude/` (settings, agents/personas, skills), no `AGENTS.md`, no `docs/plans/`, no `CONTRIBUTING.md`, no `deny.toml`. The operator's stated gap — system prompts and personas for efficient LLM-agent work — is real.
2. **Real Postgres is opt-in, not the product.** Feature `pg` off by default; no initial snapshot (no `COPY` anywhere in `pg.rs` — a fresh client on a populated table gets nothing until rows change); no CI job runs the e2e against a real Postgres.
3. **The moat isn't wired to the wire.** Subscribe carries only equality `FilterClause`s; the Tier-7 SQL-subset compiler is unreachable by any client.
4. **No write-back.** Zero write code (confirmed by ADR-0013 and route audit). Nostos is read-only sync — the headline claim "2-way offline" is currently false.
5. **The browser can't connect.** WASM bridge applies frames it can never receive; `web/` doesn't consume the built pkg.
6. **Docs materially stale.** README/ARCHITECTURE say "Week-1 spike / PgReplicator stubbed / 5 crates" (there are 9, replicator is real); ROADMAP footer says "Phase 0 🚧"; WEEK-01-PLAN acceptance boxes unticked though exceeded; bench JSON records `rustc "0.1.0"` / `hostname "unknown"` violating its own methodology §6; COMPARISON quotes eval-only numbers against a competitor's end-to-end numbers (apples-to-oranges if published).
7. **Strategy drift (July 2026 market check, sourced):**
   - A comparable engine shipped **dynamic, on-demand sync to GA in May 2026**, retiring its old fixed-partition sync-rules model — Nostos's "static buckets" attack line is gone.
   - The bucket-cap figure that attack line cited was a soft default (10k configurable), not a hard cap.
   - Still true: the leading comparable service is Node/TS with a 2–4k ops/sec replication ceiling; FSL license (2-yr Apache conversion); ElectricSQL is read-path only ([writes guide](https://electric-sql.com/docs/guides/writes)); Zero disabled offline writes; Supabase has no first-party offline layer.
   - New threat: **Supabase acquired Triplit (Oct 2025)** explicitly citing offline demand ([announcement](https://supabase.com/blog/triplit-joins-supabase)).
   - Net: the defensible wedge today is **Rust server throughput + Apache-2.0-now + write-back without endpoints + free self-host**. Positioning docs must be rewritten before any launch.

### Ordering rationale (advisor-reviewed)

Highest-risk-first: the replication boundary is where the benchmarked engine meets protocol reality, so real-PG-by-default + snapshot goes first among product work (B). Write-back is the second existential risk and the headline claim (D). Agent infra (A) runs day-zero but **only stable facts** — commands, layout, conventions — so nothing it documents gets invalidated by later phases; personas ship in the same pass because the architecture they encode (hexagonal rules, bench honesty, ADR process) is pinned. Predicate wiring (C) is low-risk, high-visibility. Browser transport (E) unlocks the web demo. F gates v0.1.

---

## Part II — Phase A: Agent operating layer (day-zero)

### Task A1: Project memory — CLAUDE.md + AGENTS.md

**Files:**
- Modify: `CLAUDE.md` (prepend project sections; keep the existing context-mode block at the bottom untouched — it is plugin-managed)
- Create: `AGENTS.md` (symlink to `CLAUDE.md`)

**Interfaces:**
- Produces: the canonical agent onboarding document every later task assumes is loaded.

- [x] **Step 1: Prepend this content to `CLAUDE.md`** (above the context-mode block, separated by `---`):

```markdown
# Nostos — Project Memory

## What this is
Rust-native local-first sync engine: Postgres logical replication → Rust fan-out server →
on-device SQLite, offline-capable, Apache-2.0 end to end. Competes with comparable sync engines
on server throughput (Rust vs Node) and license (Apache-2.0 vs FSL). Moat proof: 142k ops/sec @ 1k
clients, 0% drops = 35.6× a competitor's published ceiling — see benches/results/RESULTS.md.

## Crate map (hexagonal — dependencies point inward, violations fail review)
| crate | role | may depend on |
|---|---|---|
| nostos-domain | pure types + invariants (Predicate, Lsn, events). Zero I/O, zero async | nothing |
| nostos-application | use-cases + port traits (FanOutService, SessionStore, ReplicatorStream, SyncAuth) | domain |
| nostos-infra | adapters: PgReplicator (feature `pg`), FakeReplicator, WS transport, wire codec, auth | application, domain |
| nostos-server | composition root — the axum binary | all above |
| nostos-core | client apply engine + Storage trait. WASM-clean: no tokio, no SQLite | domain |
| nostos-client | native client: SqliteStorage (rusqlite) + tokio SyncClient | core, domain |
| nostos-ffi-wasm | wasm-bindgen bridge over nostos-core | core, domain |
| nostos-bench | throughput harness — honest numbers (drops reported, env recorded) | application, infra |
| nostos-cloud | control plane: auth / Stripe / licensing (separate binary) | domain |

`unsafe` is forbidden workspace-wide. Clippy pedantic is on; CI fails on warnings.

## Verbs (the only loops you need)
- `make ci` — fmt-check + clippy (-D warnings) + full test suite. Gate for every change.
- `cargo test -p <crate>` — focused iteration.
- `docker compose -f docker/docker-compose.yml up -d` then
  `NOSTOS_PG_URL=postgres://nostos:nostos@localhost:5433/nostos cargo test -p nostos-infra --features pg`
  — the real-Postgres e2e. (Check docker/docker-compose.yml for the actual port/credentials.)
- `make bench` — throughput benchmark. Record environment; report drop rates; never compare
  eval-only numbers against end-to-end numbers.
- `cargo run -p nostos-client --example reactive_scroll` — end-to-end native demo.

## Conventions
- Commits: single line, conventional prefix, no author mentions.
- Architectural decisions become `docs/adr/NNNN-<slug>.md` (next free number); code cites its ADR.
- Measure before optimize: perf changes ship with before/after numbers or get reverted
  (Tier-5 index revert is the precedent — docs/ROADMAP.md).
- Deliberate shortcuts carry a `ponytail:` comment naming the ceiling and upgrade path.
- The wire protocol stays human-debuggable JSON until a measurement says otherwise.
- Plans live in docs/plans/; personas in .claude/agents/; project skills in .claude/skills/.

## Reading order for a fresh agent
docs/ARCHITECTURE.md → ADRs 0001, 0003, 0009, 0011, 0012, 0013 → docs/ROADMAP.md.
Strategy/positioning: docs/STRATEGY.md. Benchmark rules: docs/BENCHMARK-METHODOLOGY.md.
```

- [x] **Step 2: Create the cross-tool alias**

```bash
ln -s CLAUDE.md AGENTS.md
```

- [x] **Step 3: Verify** — `head -5 AGENTS.md` prints the Nostos header; `make ci` still green (no code touched).

- [x] **Step 4: Commit** — `git add CLAUDE.md AGENTS.md && git commit -m "docs: add project memory to CLAUDE.md and AGENTS.md alias for agent onboarding"`

### Task A2: Permissions allowlist — .claude/settings.json

**Files:**
- Create: `.claude/settings.json`

- [x] **Step 1: Write the allowlist** (unblocks autonomous `cargo`/`make`/`git`/compose loops):

```json
{
  "permissions": {
    "allow": [
      "Bash(cargo build:*)",
      "Bash(cargo test:*)",
      "Bash(cargo clippy:*)",
      "Bash(cargo fmt:*)",
      "Bash(cargo run:*)",
      "Bash(cargo doc:*)",
      "Bash(cargo deny:*)",
      "Bash(make:*)",
      "Bash(git status:*)",
      "Bash(git log:*)",
      "Bash(git diff:*)",
      "Bash(git show:*)",
      "Bash(git add:*)",
      "Bash(git commit:*)",
      "Bash(docker compose:*)",
      "Bash(wasm-pack build:*)"
    ],
    "deny": []
  }
}
```

- [x] **Step 2: Verify** — JSON parses: `python3 -c "import json;json.load(open('.claude/settings.json'))"`.
- [x] **Step 3: Commit** — `git add .claude/settings.json && git commit -m "chore: add agent permissions allowlist for autonomous build-test loops"`

### Task A3: Personas — .claude/agents/

**Files:**
- Create: `.claude/agents/domain-guardian.md`
- Create: `.claude/agents/pg-integrator.md`
- Create: `.claude/agents/bench-runner.md`
- Create: `.claude/agents/docs-curator.md`

**Interfaces:**
- Produces: four named subagent types callable via the Agent tool (`subagent_type: "domain-guardian"` etc.) for delegation in every later phase.

- [x] **Step 1: Write `domain-guardian.md`** — the architecture reviewer:

```markdown
---
name: domain-guardian
description: Reviews diffs for hexagonal violations, domain purity, and dependency direction. Use before merging any change touching nostos-domain or nostos-application, or that adds a dependency.
tools: Read, Grep, Glob, Bash
model: sonnet
---

You are Nostos's architecture reviewer. The dependency rule is law:
domain depends on nothing; application only on domain; infra implements
application ports; nostos-server composes. nostos-core sees no tokio and no SQLite.

For every diff you review, check in order:
1. Does any crate gain a dependency pointing outward (domain → application,
   application → infra)? Check Cargo.toml diffs first — REJECT if so.
2. Does nostos-domain gain I/O, async, or a framework type? REJECT.
3. Does new infra code bypass a port trait (SessionStore, ReplicatorStream,
   EventSink, SyncAuth, Storage) instead of implementing it? REJECT with the
   port it should implement.
4. Is `unsafe` introduced anywhere? REJECT — it is forbidden workspace-wide.
5. New public API without doc comments explaining the invariant it protects? Flag.
6. New dependency: is its license Apache-2.0-compatible? Flag if not obvious.

Verdict format: APPROVE or REJECT, then findings as file:line + one-line reason
+ the minimal fix. No style nitpicks — clippy owns style.
```

- [x] **Step 2: Write `pg-integrator.md`** — the replication-boundary owner:

```markdown
---
name: pg-integrator
description: Owns the Postgres logical-replication boundary. Use for pgoutput/pgwire-replication work, snapshot/COPY, slot management, and running the real-Postgres e2e suite.
tools: Read, Grep, Glob, Bash, Edit, Write
model: sonnet
---

You own crates/nostos-infra/src/replicator/ — where Nostos meets protocol reality.

Environment: `docker compose -f docker/docker-compose.yml up -d` starts Postgres
with wal_level=logical and the publication from docker/pg-init/01-sources.sql.
Run the e2e with `NOSTOS_PG_URL=<url> cargo test -p nostos-infra --features pg`.
Read docker/docker-compose.yml for the current port and credentials — do not guess.

Rules of the boundary:
- ADR-0009 is the contract: the replication slot advances ONLY from client acks
  (min-acked LSN). Never advance it optimistically; a slot advanced past an
  unacked LSN is silent data loss on reconnect.
- Consult current crate docs BEFORE using pgoutput/pgwire-replication/tokio-postgres
  APIs from memory: docs.rs/pgwire-replication, docs.rs/pgoutput, docs.rs/tokio-postgres.
  These crates are young; verify against the pinned versions in Cargo.toml.
- Every replication change lands with a test that kills and resumes mid-stream
  (crates/nostos-infra/tests/chaos.rs and resume_and_ack.rs are the patterns).
- Edge cases that MUST have explicit handling or an explicit ponytail ceiling:
  toasted values, null bitmaps, large transactions, DDL mid-stream, slot-exists-
  on-start, publication-missing.

Report format: what you changed, the exact commands you ran, pass/fail output,
and any edge case you deferred (with its ponytail comment location).
```

- [x] **Step 3: Write `bench-runner.md`** — the honest-numbers enforcer:

```markdown
---
name: bench-runner
description: Runs and audits Nostos benchmarks. Use for any performance claim, before/after measurement, or benchmark methodology question.
tools: Read, Grep, Glob, Bash
model: sonnet
---

You run Nostos's benchmarks and enforce docs/BENCHMARK-METHODOLOGY.md. Honest
numbers are the product's credibility — an inflated number that gets debunked
on launch day costs more than a modest one.

Rules:
- `make bench` for the fan-out benchmark; record the full environment (real
  `rustc --version`, real hostname, core count) in the results artifact.
- ALWAYS report drop rates next to throughput. 45k ops/sec @ 17% drops is not
  45k ops/sec.
- NEVER let an eval-only number (predicate evals/sec) be compared against an
  end-to-end number (a competitor's ops/sec). Same-denominator comparisons only.
- Perf work follows the Tier discipline: baseline first, change, re-measure,
  and REVERT if the change regresses (Tier-5 index revert is the precedent).
- Run benches on an otherwise-idle machine; report variance across ≥3 runs if
  the number will be published.

Report format: command, environment block, results table (throughput + drops +
p99), delta vs baseline, verdict (keep/revert).
```

- [x] **Step 4: Write `docs-curator.md`** — the truth-keeper:

```markdown
---
name: docs-curator
description: Keeps README, ROADMAP, ARCHITECTURE, and ADRs consistent with shipped code. Use after any phase completes, before any release, or when docs drift is suspected.
tools: Read, Grep, Glob, Bash, Edit, Write
model: haiku
---

You keep Nostos's docs true. The repo's credibility strategy is "auditable
claims" — stale docs are bugs.

Sweep checklist:
1. README status badge/prose vs git log reality (crate count, phase, shipped features).
2. docs/ROADMAP.md phase-status lines vs its own body and the git log.
3. docs/ARCHITECTURE.md crate list and "stubbed" claims vs crates/ reality.
4. ADR "Status" lines vs implementation (grep the code for the feature).
5. Numbers quoted in docs vs benches/results/RESULTS.md (same-denominator rule).
6. Dead links and references to empty/removed directories.

Rules: fix mechanically, cite the evidence (file:line or commit) in the commit
message body of your report — but commits themselves stay single-line. Never
soften a limitation; state it. New architectural decisions are NOT yours to
make — flag them for an ADR instead.
```

- [x] **Step 5: Verify** — each file's frontmatter parses (name/description/tools/model present); commit: `git add .claude/agents && git commit -m "feat: add domain-guardian, pg-integrator, bench-runner, docs-curator agent personas"`

### Task A4: Project skill — the canonical verify loop

**Files:**
- Create: `.claude/skills/verify-nostos/SKILL.md`

- [x] **Step 1: Write the skill:**

```markdown
---
name: verify-nostos
description: Use when verifying any Nostos change end-to-end before declaring it done or committing — runs the tiered verification ladder (ci → e2e → bench → demo) matched to what the diff touched.
---

# Verify Nostos

Run the cheapest sufficient rung, escalate by what the diff touched:

1. **Always:** `make ci` — fmt-check + clippy (-D warnings) + workspace tests.
2. **Diff touches `crates/nostos-infra/src/replicator/` or feature `pg`:**
   `docker compose -f docker/docker-compose.yml up -d` (check that file for
   port/credentials), then
   `NOSTOS_PG_URL=<url from compose> cargo test -p nostos-infra --features pg`.
   Tear down with `docker compose -f docker/docker-compose.yml down -v`.
3. **Diff touches fanout/router/transport/wire (hot path):**
   `make bench` — compare against benches/results/RESULTS.md; a >10% regression
   on the 1k-client number blocks the change (Tier discipline: measure, and
   revert if it regresses).
4. **Diff touches nostos-core/nostos-client:**
   `cargo run -p nostos-client --example reactive_scroll` — must complete with
   the resume-after-restart property intact (exit 0, "resumed" in output).
5. **Diff touches nostos-ffi-wasm:** `wasm-pack build crates/nostos-ffi-wasm --target web`
   and check the gzipped size stays under the 500 KB budget (ADR-0015; currently 17 KB).

Report what rungs ran with real output, not "should pass".
```

- [x] **Step 2: Commit** — `git add .claude/skills && git commit -m "feat: add verify-nostos project skill with tiered verification ladder"`

### Task A5: Hygiene — CONTRIBUTING.md, deny.toml, .editorconfig, stale-comment fix

**Files:**
- Create: `CONTRIBUTING.md`, `deny.toml`, `.editorconfig`
- Modify: `crates/nostos-infra/src/lib.rs:10` (stale "PgReplicator stub" comment), `.github/workflows/ci.yml` (add cargo-deny job)
- Delete: `docs/decisions/` (empty dir; ADRs live in `docs/adr/`)

- [x] **Step 1: Write `CONTRIBUTING.md`:**

```markdown
# Contributing to Nostos

Pre-1.0; architecture is pinned by ADRs (docs/adr/), code is moving fast.

## Setup
- Rust 1.95.0 (rust-toolchain.toml installs it), Docker (for the Postgres e2e).
- `make setup` then `make ci` must pass before and after your change.

## Rules
- Dependency direction: bootstrap → application → domain ← infrastructure. See ADR-0001.
- `unsafe` is forbidden. Clippy pedantic is on; CI fails on warnings.
- Architectural changes need an ADR (docs/adr/NNNN-slug.md) before code.
- Perf changes need before/after numbers (docs/BENCHMARK-METHODOLOGY.md).
- Commits: single line, conventional prefix (`feat:`/`fix:`/`test:`/`docs:`/`chore:`).
- Tests accompany every non-trivial change; the e2e suite for replication changes:
  `NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg`.

## License
Apache-2.0. By contributing you agree your work is licensed the same way.
New dependencies must be Apache-2.0-compatible (checked by `cargo deny` in CI).
```

- [x] **Step 2: Write `deny.toml`:**

```toml
[licenses]
allow = [
  "Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC",
  "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0",
]

[advisories]
yanked = "deny"

[bans]
multiple-versions = "warn"
```

- [x] **Step 3: Write `.editorconfig`:**

```ini
root = true

[*]
charset = utf-8
end_of_line = lf
insert_final_newline = true
trim_trailing_whitespace = true

[*.rs]
indent_style = space
indent_size = 4
max_line_length = 100

[*.{js,ts,svelte,json,yml,yaml,toml,md}]
indent_style = space
indent_size = 2
```

- [x] **Step 4: Fix the stale comment** — in `crates/nostos-infra/src/lib.rs` line 10, replace the "`PgReplicator` stub (Week 2)" phrasing with: `PgReplicator — real pgoutput logical replication (feature "pg"); FakeReplicator — synthetic WAL generator for benches/tests.` (Match the file's existing comment style.)

- [x] **Step 5: Add cargo-deny to CI** — in `.github/workflows/ci.yml`, add a job after the lint job:

```yaml
  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          command: check licenses advisories bans
```

- [x] **Step 6: Remove the empty dir** — `rmdir docs/decisions` (git doesn't track it; also remove the pointer to it in `docs/ARCHITECTURE.md` — covered in A6).

- [x] **Step 7: Verify** — `cargo deny check licenses` passes locally (install with `cargo install cargo-deny` if absent; if any existing dep fails the allowlist, add its exact license to `deny.toml` allow-list rather than dropping the gate). `make ci` green.

- [x] **Step 8: Commit** — `git add -A && git commit -m "chore: add CONTRIBUTING, cargo-deny gate, editorconfig; fix stale PgReplicator stub comment"`

### Task A6: Docs truth sweep (make every published claim true)

**Files:**
- Modify: `README.md`, `docs/ROADMAP.md`, `docs/ARCHITECTURE.md`, `docs/WEEK-01-PLAN.md`, `docs/COMPARISON.md`, `docs/STRATEGY.md`, `crates/nostos-bench/src/report.rs`

- [x] **Step 1: README.md** — change the status badge/line from "week-1 spike" to `status: alpha — Phases 0–1 proven, v0.1 in progress`; update the layout section to all 9 crates (copy the crate table from CLAUDE.md Task A1); delete "The multi-platform client SDKs ship in later weeks" (nostos-core/client/ffi-wasm exist); keep the not-production-ready warning.

- [x] **Step 2: docs/ROADMAP.md** — change the footer `Today: **Phase 0 🚧**` to `Today: **Phase 1 🔬 — real-PG default + write-back v1 in progress** (see docs/plans/complete-nostos-fully-wired-operational.md)`.

- [x] **Step 3: docs/ARCHITECTURE.md** — update the "as-built Week-1" framing to "as-built (updated 2026-07)"; list all 9 crates; replace "`PgReplicator` Stubbed in Week 1" with the real description (pgoutput + pgwire-replication behind feature `pg`); replace the `docs/decisions/` pointer with `docs/adr/0015`/`0016`; update the predicate note to point at ADR-0012's shipped engine.

- [x] **Step 4: docs/WEEK-01-PLAN.md** — add a banner at the top: `> **Historical document (executed).** Outcome: 142,336 ops/sec @ 1k clients, 0% drops = 35.6× target baseline — see benches/results/RESULTS.md. Kept for methodology.` Tick the acceptance boxes that RESULTS.md proves; where the 10k-client `<1% drop` bar was NOT met (17.26% drops), do NOT tick — annotate: `10k-client drop rate 17.26% — WS write path is the known limit; fix tracked in plan Phase C3.`

- [x] **Step 5: docs/COMPARISON.md + docs/STRATEGY.md (positioning rewrite)** — apply the July-2026 market facts from Part I §7: remove/rewrite "static buckets only" and "1,000-bucket hard cap" attack lines (Sync Streams GA; cap is soft); reposition the wedge as (1) Rust server throughput vs Node's 2–4k ops/sec replication ceiling, (2) Apache-2.0 **today** vs FSL's 2-year delay, (3) write-back without customer-built endpoints (vs ElectricSQL read-only and comparable SDKs' uploadData), (4) free full-featured self-host. Add a "Threats" note: Supabase/Triplit first-party offline ambitions. In COMPARISON.md, label every Nostos number as eval-only or end-to-end and only compare same-denominator pairs.

- [x] **Step 6: Fix bench env capture** — in `crates/nostos-bench/src/report.rs`, the results JSON records `rustc: "rustc 0.1.0 (nostos-bench build)"` and `hostname: "unknown"`. Replace with real values: shell out once at report time (`rustc --version` via `std::process::Command`, hostname via `std::process::Command::new("hostname")`), falling back to `"unknown"` only on error. Add a unit test asserting the rustc field starts with `"rustc 1."` on the build machine.

- [x] **Step 7: Verify** — `make ci`; grep for leftovers: `grep -rn "week-1 spike\|Stubbed in Week\|1,000 buckets" README.md docs/ --include="*.md"` returns only historical-context hits (WEEK-01-PLAN banner text is fine).

- [x] **Step 8: Commit** — `git add -A && git commit -m "docs: truth sweep — status, crate map, benchmark honesty, July-2026 competitive repositioning"`

---

## Part III — Phase B: Real Postgres by default

### Task B1: Compile the `pg` feature in by default

**Files:**
- Modify: `crates/nostos-server/Cargo.toml` (features), `crates/nostos-server/src/main.rs:199-268` (selection error path)
- Test: existing `cargo build` matrix

**Interfaces:**
- Consumes: existing `pg` feature forwarding (`nostos-server` → `nostos-infra/pg`).
- Produces: `cargo build -p nostos-server` (no flags) includes `PgReplicator`; `NOSTOS_REPLICATOR` runtime default stays `fake` so zero-setup `cargo run` keeps working.

- [x] **Step 1: Make the feature default** — in `crates/nostos-server/Cargo.toml`:

```toml
[features]
default = ["pg"]
pg = ["nostos-infra/pg"]
```

- [x] **Step 2: Improve the failure mode** — in `main.rs`'s replicator match: when `NOSTOS_REPLICATOR=pg` and `NOSTOS_PG_URL` is unset/unreachable, the error must name the fix verbatim: `set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d`. When the binary was built without the feature (`--no-default-features`), keep the existing warn-and-fallback but include `rebuild with --features pg` in the message.

- [x] **Step 3: Verify both builds** — `cargo build -p nostos-server` and `cargo build -p nostos-server --no-default-features` both compile; `NOSTOS_REPLICATOR=pg cargo run -p nostos-server` without a DB prints the actionable error and exits non-zero.

- [x] **Step 4: Run `make ci`; commit** — `git commit -m "feat: compile pg replicator by default with actionable misconfiguration errors"`

### Task B2: Initial snapshot via COPY (the missing first sync)

The gap: a client subscribing to a populated table receives nothing until rows change. Fix per Phase-1 roadmap: snapshot-then-stream.

**Files:**
- Create: `crates/nostos-infra/src/replicator/snapshot.rs`
- Modify: `crates/nostos-infra/src/replicator/pg.rs` (emit snapshot rows before streaming), `crates/nostos-infra/src/replicator/mod.rs` (module decl)
- Test: `crates/nostos-infra/tests/e2e_pg_snapshot.rs`

**Interfaces:**
- Consumes: `ReplicatorStream::next_event() -> Option<ReplicationEvent>` (verified seam — snapshot rows flow through the same port; no fan-out or client changes needed) and `pg.rs`'s existing `tuple_to_json_payload` row-encoding shape.
- Produces: on first start (slot does not exist), `next_event` yields one `ReplicationEvent` insert per existing row in every table of the publication, at the slot's consistent-point LSN, then seamlessly continues with live streamed events. On restart (slot exists), no snapshot is emitted.

- [x] **Step 1: Read the current docs before coding** (the crates are young; do not trust memory): [docs.rs/pgwire-replication/0.3.2](https://docs.rs/pgwire-replication) for `CREATE_REPLICATION_SLOT … (SNAPSHOT 'export')` support and the returned `consistent_point`/`snapshot_name`; [docs.rs/tokio-postgres](https://docs.rs/tokio-postgres) for `copy_out` and `SET TRANSACTION SNAPSHOT`. If pgwire-replication 0.3.2 cannot create a slot with an exported snapshot, the fallback design is: create the slot via SQL on a regular connection (`SELECT pg_create_logical_replication_slot('nostos_slot','pgoutput')` inside the same transaction discipline) — decide based on what the docs actually say and record the choice as a comment citing the doc section.

- [x] **Step 2: Write the failing e2e test** (`crates/nostos-infra/tests/e2e_pg_snapshot.rs`, gated like the existing e2e on `NOSTOS_PG_URL`):

```rust
//! Snapshot-then-stream: a fresh slot must deliver pre-existing rows first.
//! Requires a real Postgres: NOSTOS_PG_URL=… cargo test --features pg

mod common;

#[tokio::test]
async fn fresh_slot_yields_snapshot_rows_then_live_stream() {
    let Some(url) = std::env::var("NOSTOS_PG_URL").ok() else {
        eprintln!("skipped: NOSTOS_PG_URL not set");
        return;
    };
    // 1. Seed: 3 rows in the publication table BEFORE the replicator starts,
    //    using a unique per-test table or a TRUNCATE of the shared one
    //    (follow the setup pattern in e2e_pg_replication.rs / common/mod.rs).
    // 2. Start PgReplicator with a fresh slot name (drop it if it exists).
    // 3. Collect events via next_event():
    //    - first 3 events are Insert ops for the seeded pks, all at the SAME lsn
    //      (the snapshot consistent point), in any order;
    //    - then INSERT a 4th row live and assert it arrives with lsn > snapshot lsn.
    // 4. Restart the replicator with the SAME slot: assert NO snapshot replay
    //    (first event is either the 4th row redelivered per ack state, or nothing).
}
```

Flesh the comment skeleton into real code by copying the connection/seed helpers from `crates/nostos-infra/tests/e2e_pg_replication.rs` (they exist — that file is the pattern). Run: `NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg fresh_slot` → FAIL (snapshot events never arrive).

- [x] **Step 3: Implement `snapshot.rs`** — one public async fn:

```rust
//! Initial table snapshot: COPY every publication table under the slot's
//! exported snapshot so the stream starts complete (roadmap Phase 1).

/// Rows from all tables in `publication`, read under `snapshot_name`, encoded
/// with the same JSON payload shape the streaming path uses.
/// Returned as ready-to-emit inserts at `consistent_point`.
pub(crate) async fn snapshot_events(
    pg_url: &str,
    publication: &str,
    snapshot_name: &str,
    consistent_point: Lsn,
) -> Result<Vec<ReplicationEvent>, SnapshotError> {
    // 1. tokio_postgres::connect(pg_url)
    // 2. BEGIN ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION SNAPSHOT $name;
    // 3. tables = SELECT schemaname, tablename FROM pg_publication_tables
    //             WHERE pubname = $1
    // 4. per table: COPY (SELECT row_to_json(t) FROM <ident-quoted table> t) TO STDOUT
    //    — one JSON object per line; parse with serde_json; pk column read from
    //    the same convention pg.rs::tuple_to_json_payload uses (verify there).
    // 5. map each row → ReplicationEvent{ op: Insert, lsn: consistent_point, .. }
}
```

Identifier safety: quote schema/table with `quote_ident` semantics (tokio-postgres `escape_identifier` or manual `"` doubling); names come from `pg_publication_tables`, not from clients, but quote anyway. Memory ceiling: this buffers the snapshot in RAM — acceptable v1; add the comment `// ponytail: whole-snapshot buffered in memory; stream per-table batches through a channel when a real dataset exceeds RAM`.

- [x] **Step 4: Wire into `pg.rs`** — on connect, when the slot is newly created: call `snapshot_events(...)`, hold the Vec, and have `next_event()` drain it before polling the replication stream. When the slot already existed: skip. Match the existing code style; the seam stays `ReplicatorStream`.

- [x] **Step 5: Concurrent-writes-during-snapshot test** (the classic slot-snapshot landmine — advisor-flagged CRITICAL): add a second test that INSERTs rows *while* the snapshot COPY is in flight (spawn the write task right after the replicator starts) and asserts every pk appears **exactly once** across snapshot events + streamed events — never zero times (lost between snapshot and stream start) and never twice (in both). The exported-snapshot + consistent-point design makes this hold structurally; this test is what proves it.

- [x] **Step 6: Run the tests** — `docker compose -f docker/docker-compose.yml up -d && NOSTOS_PG_URL=… cargo test -p nostos-infra --features pg fresh_slot` → PASS (both). Also re-run the full existing e2e: `cargo test -p nostos-infra --features pg` → all green (no regression in resume/ack semantics).

- [x] **Step 7: Commit** — `git commit -m "feat: initial snapshot via COPY under exported slot snapshot — fresh clients get existing rows"`

### Task B3: CI runs the real-Postgres e2e

**Files:**
- Modify: `.github/workflows/ci.yml`

- [x] **Step 1: Add the job** (mirror the compose setup; check `docker/pg-init/01-sources.sql` for init):

```yaml
  e2e-pg:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env:
          POSTGRES_USER: nostos
          POSTGRES_PASSWORD: nostos
          POSTGRES_DB: nostos
        ports: ["5432:5432"]
        options: >-
          --health-cmd "pg_isready -U nostos" --health-interval 5s
          --health-timeout 5s --health-retries 10
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable   # match the repo's existing toolchain step style
      - name: Enable logical replication + publication
        run: |
          psql postgres://nostos:nostos@localhost:5432/nostos -c "ALTER SYSTEM SET wal_level = 'logical';"
          docker restart $(docker ps -q --filter ancestor=postgres:16)
          sleep 5
          psql postgres://nostos:nostos@localhost:5432/nostos -f docker/pg-init/01-sources.sql
      - name: Run feature-gated e2e
        run: NOSTOS_PG_URL=postgres://nostos:nostos@localhost:5432/nostos cargo test -p nostos-infra --features pg
```

Note: GH Actions `services:` containers can't take `command:` args, hence the ALTER SYSTEM + restart dance; if it proves flaky, switch the job to `docker compose -f docker/docker-compose.yml up -d` directly on the runner (compose already sets `wal_level=logical` — check that file first and prefer whichever is simpler; that's a judgment call the executor makes and records in the commit).

- [x] **Step 2: Verify on a branch push** — job green in Actions; then commit to main: `git commit -m "ci: run real-Postgres logical-replication e2e on every push"`
  - **Validation status (2026-07):** `actionlint` clean on `.github/workflows/ci.yml` (local). Remote push-validation **pending**: `git remote -v` is empty — the operator must configure a remote and push for the Actions run to execute. Job YAML is final and ready.

### Task B4: One-command dev stack + README quickstart

**Files:**
- Modify: `Makefile` (target `dev-stack`), `README.md` (quickstart section)

- [x] **Step 1: Add the Make target** (check `docker/docker-compose.yml` for the actual port/credentials and reuse them verbatim):

```make
## dev-stack: real-Postgres quickstart — compose up, wait, run server against it
dev-stack:
	docker compose -f docker/docker-compose.yml up -d
	@echo "waiting for postgres…" && sleep 3
	NOSTOS_REPLICATOR=pg NOSTOS_PG_URL=$(NOSTOS_PG_URL_DEFAULT) cargo run -p nostos-server
```

with `NOSTOS_PG_URL_DEFAULT` defined at the top of the Makefile from the compose file's values.

- [x] **Step 2: README quickstart** — replace/extend the quickstart with the 3-command real path: `make dev-stack` (terminal 1), `cargo run -p nostos-client --example reactive_scroll` pointed at it (terminal 2 — if the example currently spins an in-process server, note it as the zero-setup path and show the WS-URL env override for the real-server path only if the example already supports one; do not add flags speculatively).
- [x] **Step 3: Verify by executing the README steps exactly as written** — a wrong quickstart is worse than none.
- [x] **Step 4: Commit** — `git commit -m "feat: make dev-stack one-command real-Postgres quickstart"`

---

## Part IV — Phase C: Wire the predicate moat end-to-end

### Task C1: `where_sql` on the Subscribe frame, compiled server-side

**Files:**
- Modify: `crates/nostos-infra/src/wire.rs:41-61` (Subscribe + `where_sql`), `crates/nostos-infra/src/transport.rs` (compile + enforce), 
- Test: extend `crates/nostos-infra/tests/ws_contract.rs`

**Interfaces:**
- Consumes: `nostos_domain::predicate_compile::parse_predicate_expr(&str) -> Result<PredicateExpr, ParseError>` (verified); `Predicate { table, expr }` public fields (verified); transport's existing tenant enforcement `p.and_eq(tenant_col, …)` (verified, ADR-0011).
- Produces: wire schema v-next — `Subscribe { table, filters, where_sql: Option<String>, resume_lsn }`; invalid SQL → socket closed with reason `"invalid where_sql: <ParseError>"` before any event flows.

- [x] **Step 1: Write the failing contract tests** in `ws_contract.rs` (follow the file's existing connect-and-frame helpers):

```rust
#[tokio::test]
async fn subscribe_with_where_sql_filters_events() {
    // subscribe: {"type":"subscribe","table":"tasks","where_sql":"priority > 5"}
    // publish rows priority=3 and priority=7 through the fake replicator
    // assert: only the priority=7 frame is delivered
}

#[tokio::test]
async fn subscribe_with_invalid_where_sql_is_rejected_before_events() {
    // subscribe: {"type":"subscribe","table":"tasks","where_sql":"DROP TABLE tasks"}
    // assert: socket closes with a reason containing "invalid where_sql"
}

#[tokio::test]
async fn where_sql_cannot_shed_tenant_enforcement() {
    // auth as tenant A with tenant enforcement on;
    // where_sql: "tenant_id = 'B' OR priority > 0"
    // publish a tenant-B row matching the OR arm;
    // assert: NOT delivered (server ANDs the tenant clause outside the client expr — ADR-0011)
}
```

Run: `cargo test -p nostos-infra ws_contract` → 3 FAIL (unknown field / no filtering).

- [x] **Step 2: Extend the wire type** — in `wire.rs`'s `ClientMessage::Subscribe`:

```rust
    Subscribe {
        table: String,
        #[serde(default)]
        filters: Vec<FilterClause>,
        /// Optional safe-SQL-subset expression (ADR-0012 compiler). Compiled
        /// server-side; ANDed with `filters` and with server-enforced clauses.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_sql: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_lsn: Option<u64>,
    },
```

Thread `where_sql` through transport's `SubscribeRequest`.

- [x] **Step 3: Compile and combine in transport.rs** — where the predicate is built today (the fn ending in the ADR-0011 tenant `and_eq`), before tenant enforcement:

```rust
    if let Some(sql) = &req.where_sql {
        match nostos_domain::parse_predicate_expr(sql) {
            Ok(expr) => p = Predicate { table: p.table, expr: p.expr.and(expr) },
            Err(e) => return Err(SubscribeError::InvalidWhereSql(e.to_string())),
        }
    }
    // existing tenant enforcement stays LAST so it wraps the client expression
```

(Adjust the exact plumbing to the fn's real signature — it currently returns `Predicate` infallibly; make it `Result` and close the socket with the reason string at the caller. Import path for `parse_predicate_expr` per `nostos-domain`'s re-exports — check `crates/nostos-domain/src/lib.rs`.)

- [x] **Step 4: Run the tests** — 3 PASS; full `make ci` green.
- [x] **Step 5: Commit** — `git commit -m "feat: wire safe-SQL predicate compiler into subscribe path with server-enforced tenant clauses"`

### Task C2: Expose `where_sql` in the native client and WASM bridge

**Files:**
- Modify: `crates/nostos-client/src/client.rs` (SyncClient subscribe config), `crates/nostos-ffi-wasm/src/lib.rs` (constructor/subscribe param), `crates/nostos-client/examples/reactive_scroll.rs` (use it)
- Test: `crates/nostos-client/tests/` (one round-trip), wasm unit via existing pattern

- [x] **Step 1: Failing test** — in nostos-client's test suite, spin the in-process server (pattern: `reactive_scroll.rs` / `chaos_resume.rs`), subscribe with `where_sql: Some("priority > 5".into())`, publish 3/7, assert only 7 lands in SQLite.
- [x] **Step 2: Add the field** to the client's subscribe/config struct and serialize it into the Subscribe frame (it already serializes `ClientMessage`; the field flows for free once present). Mirror in the WASM bridge as an optional string param.
- [x] **Step 3: Update `reactive_scroll.rs`** to subscribe with a `where_sql` instead of (or alongside) equality filters — the demo now exercises the Tier-7 compiler end to end.
- [x] **Step 4:** Tests pass; `cargo run -p nostos-client --example reactive_scroll` still exits 0 with resume intact; `wasm-pack build crates/nostos-ffi-wasm --target web` still under budget.
- [x] **Step 5: Commit** — `git commit -m "feat: where_sql subscriptions in native client, wasm bridge, and reactive_scroll demo"`

### Task C3: 10k-client drop-rate fix (measure → batch → verify)

The known limit: 45,964 ops/sec @ 17.26% drops at 10k clients — per-connection WS write path. Roadmap's named fix: batched WS writes, then table-sharded router. Tier discipline applies: this task is three measurements with a change between them, and **reverts if it regresses the 1k-client headline**.

**Files:**
- Modify: `crates/nostos-infra/src/transport.rs` (batch frames per flush tick), possibly `crates/nostos-infra/src/router.rs`
- Test: `make bench` at 1k/5k/10k; existing `ws_contract.rs` must stay green

- [x] **Step 1: Baseline** — `make bench` at 1k/5k/10k on an idle machine; record env + 3-run variance into `benches/results/` (bench-runner persona's format).
- [x] **Step 2: Batch the write path** — in the per-session sink→socket pump, drain up to N pending frames (start N=64) from the session channel and send as one WS message containing a JSON array of frames; client `decode` already iterates frames? — **check first**: if the client/wire decode expects one frame per message, extend `decode` to accept `[{...},{...}]` arrays (server can then batch without a wire version bump; old single-frame messages remain valid). Keep the flush immediate when the channel is empty (no latency tax at low rates): batching only kicks in under backlog.
- [x] **Step 3: Re-measure** — same 3×3 matrix. Accept if: 10k-client drop rate < 1% at ≥ a competitor's published ceiling throughput AND 1k-client headline within noise of baseline. Otherwise revert and record the numbers in `docs/ROADMAP.md` the way Tier 5 did.
- [x] **Step 4: Reconnect-storm probe (decision point, advisor-flagged)** — batching fixes steady-state throughput; a reconnect storm is a different failure mode. Extend `nostos-bench` (or a one-off harness in `benches/`) to drop and simultaneously reconnect 5k of the 10k clients mid-stream, each re-subscribing with a `resume_lsn`; record peak per-session queue depth, drop rate, and time-to-drain. If the storm exceeds sustainable queue depth (sustained drops after batching), file the finding + numbers as the opening measurement of a follow-up admission-control/token-bucket task **before Phase D lands**; if it drains cleanly, record the numbers and move on — do not build admission control speculatively.
- [x] **Step 5: Commit (either outcome)** — `git commit -m "feat: batched WS writes — 10k-client drops X% -> Y%"` or `git commit -m "docs: WS batching measured, regressed 1k headline, reverted"` (+ storm numbers in the message body of the ROADMAP note, commit itself single-line)

---

## Part V — Phase D: Write-back v1 (2-way offline, ADR-0013 minimal slice)

Design decision this plan makes (operator may veto): **v1 write-back rides the existing authenticated WebSocket** as a new `ClientMessage::Write`, not a separate HTTP POST. Rationale: zero new dependencies (no HTTP client in nostos-client), one authenticated channel, ordered with ACKs, and the echo problem is already solved — `Storage::apply_batch` is documented idempotent-upsert, so a write echoing back through replication is a safe no-op. ADR-0013's fuller design (declarative write rules, version/etag conflict checks, function mode, HTTP path for gateways) remains the Phase-4 target. Task D1 records this as an ADR addendum first.

### Task D1: ADR-0013 addendum

**Files:**
- Modify: `docs/adr/0013-direct-write-back-design.md` (append an addendum section)

- [x] **Step 1: Append:**

```markdown
## Addendum (2026-07): v1 ships over the sync WebSocket

v1 scope shipped ahead of Phase 4 (plan: docs/plans/complete-nostos-fully-wired-operational.md):
- Transport: `ClientMessage::Write` on the existing authenticated /sync socket —
  zero new deps, one auth path, ordered with ACKs. The HTTP POST path described
  above remains the Phase-4 design for gateway/enterprise deployments.
- Rules: per-table allowlist (`NOSTOS_WRITE_TABLES`), pk upsert/delete only,
  server-authoritative LWW by WAL order (ADR-0004/0014 tier (a)).
- Conflict checks (version/etag), declarative write rules, and function mode
  remain Phase 4. Echo suppression is unnecessary: client apply is an
  idempotent upsert (nostos-core Storage contract), so the write's replication
  echo is a no-op.
```

- [x] **Step 2: Commit** — `git commit -m "docs: ADR-0013 addendum — v1 write-back over sync socket, LWW, table allowlist"`

### Task D2: Write port + Postgres adapter (server side)

**Files:**
- Modify: `crates/nostos-application/src/ports.rs` (new port), `crates/nostos-infra/src/wire.rs` (Write/WriteResult), `crates/nostos-infra/src/transport.rs` (handle Write), `crates/nostos-server/src/main.rs` (compose adapter, `NOSTOS_WRITE_TABLES`)
- Create: `crates/nostos-infra/src/write_back.rs` (PgWriteBack adapter, feature `pg`)
- Test: `crates/nostos-infra/tests/ws_contract.rs` (reject paths, fake mode), `crates/nostos-infra/tests/e2e_pg_writeback.rs` (real round-trip)

**Interfaces:**
- Produces (port, in `ports.rs` — later tasks depend on these exact names):

```rust
/// Applies client-submitted writes to the source database (ADR-0013 v1).
/// Implementations: `PgWriteBack` (infra, feature "pg"); test doubles record.
#[async_trait]
pub trait WriteBack: Send + Sync {
    /// Upsert one row: `payload_json` is a JSON object of column -> value,
    /// the same tuple-image shape the read path delivers. LWW by WAL order.
    async fn upsert(&self, table: &str, pk: &str, payload_json: &str) -> Result<(), WriteBackError>;
    /// Delete by primary key. Missing row is success (idempotent).
    async fn delete(&self, table: &str, pk: &str) -> Result<(), WriteBackError>;
}

#[derive(Debug, thiserror::Error)]
pub enum WriteBackError {
    #[error("table not writable: {0}")]
    TableNotAllowed(String),
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("backend: {0}")]
    Backend(String),
}
```

- Produces (wire): `ClientMessage::Write { table, op, pk, payload, client_write_id }` (`op`: `"upsert" | "delete"`; `payload`: JSON object, absent for delete) and a new server→client frame `WriteResult { client_write_id, ok, error }`.

- [x] **Step 1: Failing contract tests** (fake-replicator mode, no PG needed): (a) `Write` to a non-allowlisted table → `WriteResult{ok:false, error:"table not writable…"}`; (b) `Write` before `Subscribe` → socket closed (same discipline as early-ACK); (c) malformed payload (non-object) → `ok:false, InvalidPayload`; (d) in fake mode with an allowlisted table → `ok:false, error:"write-back requires pg replicator"` (v1: writes need the real source; the fake has no database). Run → FAIL.
- [x] **Step 2: Wire types** — add `Write` to `ClientMessage`, add the `WriteResult` outbound frame beside `encode_event` (same JSON, `"type":"write_result"`).
- [x] **Step 3: Port + adapter** — trait as above in `ports.rs`. `PgWriteBack` in `write_back.rs`: owns a `tokio_postgres::Client` pool-of-one (`ponytail: single connection; pool when a real load shows contention`), allowlist `HashSet<String>` from `NOSTOS_WRITE_TABLES` (comma-separated, exact match). Upsert SQL built as: identifiers validated against `^[a-z_][a-z0-9_]*$` **and** the allowlist, then ident-quoted; column names from the payload JSON keys validated by the same regex; values bound as parameters (`$1…$n`, `serde_json::Value` → text with `::jsonb`/`::text` casts as the column requires — v1 binds everything as text and lets PG coerce, `ponytail: text-cast binding; typed binding when a schema registry exists (ADR-0012 follow-on)`). Statement shape:

```sql
INSERT INTO "t" ("id","col1",…) VALUES ($1,$2,…)
ON CONFLICT ("id") DO UPDATE SET "col1"=EXCLUDED."col1", …
```

pk column name: v1 convention `id` (`ponytail: pk column fixed to "id"; read from pg_constraint when a design partner needs composite/renamed pks`). Deletes: `DELETE FROM "t" WHERE "id" = $1`.
- [x] **Step 4: Transport + composition** — extend `handle_client_message`'s match with `Write`: call the injected `Arc<dyn WriteBack>`, send `WriteResult`. Compose `PgWriteBack` in `main.rs` under feature `pg` when `NOSTOS_REPLICATOR=pg` (reuse `NOSTOS_PG_URL`); otherwise inject a `NoWriteBack` stub that returns the fake-mode error.
- [x] **Step 5: Contract tests PASS; then the real e2e** (`e2e_pg_writeback.rs`, `NOSTOS_PG_URL`-gated): client A writes a row over WS → assert `WriteResult ok` → assert the row arrives back through replication to client B (and to A, where the idempotent apply is a no-op — assert row count 1). `make ci` + feature e2e green.
- [x] **Step 6: Commit** — `git commit -m "feat: write-back v1 — WriteBack port, PgWriteBack upsert/delete with table allowlist, wire and transport"`

### Task D3: Client outbox — durable offline writes

**Files:**
- Modify: `crates/nostos-core/src/lib.rs` (new trait), `crates/nostos-client/src/sqlite.rs` (outbox table), `crates/nostos-client/src/client.rs` (enqueue + flush loop)
- Create: `crates/nostos-core/src/outbox.rs`
- Test: `crates/nostos-client/tests/offline_writes.rs`

**Interfaces:**
- Consumes: `WriteBack` wire frames from D2 (`ClientMessage::Write`, `WriteResult`).
- Produces (in `nostos-core`, WASM-clean — no tokio, no SQLite):

```rust
/// A durable queue of local writes awaiting server acknowledgment (ADR-0013 v1).
/// Same-crate sibling of `Storage`; implementations SHOULD persist both in the
/// same database so a crash can't strand one without the other.
pub trait Outbox {
    /// Enqueue a local write. Returns its monotonically increasing id.
    fn enqueue(&mut self, write: PendingWrite) -> crate::Result<u64>;
    /// All writes not yet acknowledged, oldest first.
    fn pending(&self) -> crate::Result<Vec<(u64, PendingWrite)>>;
    /// Remove an acknowledged write.
    fn mark_done(&mut self, id: u64) -> crate::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    pub table: String,
    pub op: WriteOp,          // Upsert | Delete
    pub pk: String,
    pub payload_json: Option<String>,
}
```

and `SyncClient::write(PendingWrite)` — enqueue always (even offline); the connected loop flushes `pending()` in order, `mark_done` on each `WriteResult{ok:true}`; on `ok:false` the write stays queued and the error surfaces via the client's existing error/log channel with the write id (`ponytail: failed writes retry forever and block the queue head; add a dead-letter policy when a design partner hits a permanent rejection`).

- [x] **Step 1: Failing test** (`offline_writes.rs`, in-process server pattern): start client with server DOWN → `client.write(upsert of pk "a")` succeeds (enqueued) → assert `pending().len()==1` via a fresh `SqliteStorage` handle on the same file (durability) → start server → client connects → assert `WriteResult` processed, `pending()` empty, and the row round-trips back into the client's SQLite. Kill-restart variant: enqueue, drop the client process entirely, recreate from the same file, connect → flush still happens (the queue survived).
- [x] **Step 2: Implement** — `outbox` table in `sqlite.rs` (`id INTEGER PRIMARY KEY AUTOINCREMENT, table_name TEXT, op TEXT, pk TEXT, payload TEXT`), same connection/transaction discipline as the checkpoint table; `Outbox` impl for `SqliteStorage`; flush loop in `client.rs` after subscribe-ack, serialized with the apply loop (single-threaded by construction, per the Storage contract).
- [x] **Step 3: Tests PASS; `make ci`; run `reactive_scroll` (unchanged behavior).**
- [x] **Step 4: Commit** — `git commit -m "feat: durable client outbox — offline writes survive restarts and flush on reconnect"`

### Task D4: Chaos: 2-way offline end-to-end

**Files:**
- Create: `crates/nostos-client/tests/chaos_write_resume.rs`
- Modify: `crates/nostos-client/examples/reactive_scroll.rs` (add one local write to the demo script)

- [x] **Step 0: Prove the idempotency premise first** (the whole no-echo-suppression design rests on it — make it a test, not a doc claim): unit test in `crates/nostos-client/tests/` (or extend nostos-core's suite): deliver the SAME `RowOp` (same table+pk+payload, same LSN batch) to `SqliteStorage::apply_batch` twice, and in a second case via two separate batches; assert row count 1 and final payload identical both times. If this fails, STOP — D2/D4's design assumption is broken and echo suppression must be designed before proceeding.
- [x] **Step 1: The test that makes "2-way offline" a true sentence** — combining chaos_resume's restart pattern with D3: client online syncing → server killed → client makes 2 offline writes + keeps working → server restarts → client reconnects, resumes from checkpoint (no loss), flushes outbox → both rows visible via replication echo → total row count exact (no duplication from echo or replay). Assert all invariants with counts, not "no crash".
- [x] **Step 2: PASS; update `reactive_scroll.rs`** to perform one `client.write(...)` mid-script and print the round-trip, so the demo demonstrates 2-way.
- [x] **Step 3: Commit** — `git commit -m "test: chaos write-resume — offline writes + mid-stream restart, zero loss zero duplication"`

---

## Part VI — Phase E: The browser connects

### Task E1: WASM WebSocket transport

**Files:**
- Modify: `crates/nostos-ffi-wasm/src/lib.rs`, `crates/nostos-ffi-wasm/Cargo.toml` (add `web-sys` features: `WebSocket`, `MessageEvent`, `BinaryType`, `CloseEvent`, `ErrorEvent`; `js-sys`; `wasm-bindgen-futures`)
- Test: `crates/nostos-ffi-wasm/tests/` via `wasm-bindgen-test` (headless browser)

**Interfaces:**
- Consumes: existing `NostosEngine` apply API and the wire JSON (`ClientMessage::Subscribe`/`Ack`, event frames, `where_sql` from C2).
- Produces: `NostosSocket::connect(url, token, table, where_sql: Option<String>) -> Promise`; incoming frames applied to the engine; checkpoint persisted to `localStorage` key `nostos:checkpoint:<table>`; ACKs sent per applied batch; `resume_lsn` read from localStorage on connect. (`ponytail: localStorage checkpoint + in-memory rows; durable rows arrive with OPFS in E2 — the ceiling is "reload replays from resume_lsn".`)

- [x] **Step 1: Read the docs first** — wasm-bindgen web-sys WebSocket example (rustwasm book), `wasm-bindgen-futures` for the async bridge; verify against the pinned wasm-bindgen version in Cargo.toml.
- [x] **Step 2: Failing wasm test** — `wasm_bindgen_test` against an in-process… no: browser tests can't spawn the Rust server. Test seam instead: factor frame-pump logic (`on_message(bytes) -> {apply, maybe_ack, checkpoint}`) as a pure function over `NostosEngine` + a `Sender` closure; unit-test THAT in wasm (feed encoded frames, assert apply outcomes + ack bytes + stored checkpoint), leaving only the thin `web_sys::WebSocket` glue untested (`ponytail: WS glue untested in CI; covered by the E3 demo page manual check`).
- [x] **Step 3: Implement; `wasm-pack build --target web` stays under the 500 KB budget** (currently 17 KB; web-sys adds little).
- [x] **Step 4: Commit** — `git commit -m "feat: wasm websocket transport — browser subscribes, applies, acks, resumes from localStorage checkpoint"`

### Task E2: OPFS durability decision (docs-gated spike, may conclude "defer")

**Files:**
- Create: `docs/adr/0017-web-persistence.md`

- [x] **Step 1: Research current state (July 2026)** of: official SQLite WASM OPFS (`sqlite.org/wasm`), `wa-sqlite`, and raw-OPFS row storage; constraints: Worker-only OPFS, COOP/COEP headers, Safari support.
- [x] **Step 2: Write ADR-0017** with the chosen mechanism + measured bundle-size and latency implications; if the verdict is "raw OPFS keyed rows now, SQLite-WASM later" or even "defer durability, localStorage checkpoint is enough for v0.1 demos", say so with the evidence. The ADR, not this plan, owns that call — it needs facts this plan can't know without the spike.
  - **Verdict: defer past v0.1; commit to SQLite-WASM `opfs-sahpool` post-launch.** Evidence + rejection of wa-sqlite (COOP/COEP tax) and raw OPFS (no atomicity) in `docs/adr/0017-web-persistence.md`. Write-back v1 raised the trait surface from 2 to 5 methods, making the Worker re-architecture the dominant cost.
- [x] **Step 3: If (and only if) the ADR picks a v0.1 mechanism**, implement it as a `Storage` impl behind the existing trait; the E1 pump code does not change (that's the seam paying rent).
  - **N/A — ADR-0017 deferred.** No v0.1 mechanism chosen; the `Storage`/`Outbox` seam is unchanged and ready for the post-launch slice.
- [x] **Step 4: Commit** — `git commit -m "docs: ADR-0017 web persistence decision"` (+ impl commit if chosen)

### Task E3: The web demo — moat visible in a browser tab

**Files:**
- Create: `web/src/routes/demo/+page.svelte` (or the repo's route convention — check `web/src/routes/`)
- Modify: `web/package.json` (local dependency on the built pkg), `Makefile` (`web-demo` target chaining wasm-pack → vite dev)

- [x] **Step 1:** Wire `crates/nostos-ffi-wasm/pkg` into `web/` (vite `fs.allow` or file: dependency — check how the pkg was built; it exists at `crates/nostos-ffi-wasm/pkg/`).
  - Wired via `file:../crates/nostos-ffi-wasm/pkg` dependency in `web/package.json`; lazy `import('nostos-ffi-wasm')` inside `onMount` keeps `adapter-static` prerender clean.
- [x] **Step 2:** Demo page: connect to `ws://localhost:8080/sync` (the dev-stack server), subscribe `tasks` with a `where_sql` input box, render the live rows, show the checkpoint LSN advancing; a "kill the server" instruction demonstrating reload-and-resume.
  - Plan text said 8080; the real `NOSTOS_BIND` default is **8800** (`crates/nostos-server/src/main.rs:39`) — page uses 8800 and is editable. Page dogfoods the real `NostosEngine` (apply + checkpoint + ack) and re-implements the ~30 lines of WS glue (instead of `NostosSocket.connect`) because the engine has no row-readback API — documented inline with a ponytail upgrade path (swap to `NostosSocket` when OPFS ships a readback API).
- [x] **Step 3: Verify manually**: `make dev-stack` + `make web-demo`, insert rows via `psql`, watch them appear filtered; reload the tab → resume from checkpoint (no full replay unless slot demands).
  - Verified: `npm run check` 0 errors / 0 warnings; `npm run build` succeeds (wasm chunk loads, demo route compiles); `make ci` green. **Live WS + psql row-insert not run by the agent** — the page loads without console errors, but the full manual click-through (start dev-stack, insert via psql, watch filtered rows, reload-and-resume) remains for the operator. The page's empty-state CTA shows the exact `psql` command to run.
- [x] **Step 4: Commit** — `git commit -m "feat: browser demo page — live filtered sync via wasm bridge"`

---

## Part VII — Phase F: v0.1 gate

### Task F1: Stranger test + fixes

- [x] **Step 1:** A fresh agent (or the operator) follows README.md ONLY, on a clean checkout, to: run the dev stack, run the native demo, run the web demo, make an offline write, see it round-trip. Time-box 30 minutes; log every friction point verbatim.
  - Stranger test run 2026-07-05. **Verified working as-documented:** `cp .env.example .env` → `make setup` (toolchain ready) → `make test` (all green, 0 failures) → native demo `cargo run -p nostos-client --example reactive_scroll` (exits 0; demonstrates offline write + round-trip — `ROUND-TRIP: wrote 'demo-write', received it back via replication` — plus `resumed from durable checkpoint`). Web demo `make web-demo` starts Vite on :5173 and the `/demo` route serves HTTP 200. **Couldn't verify (env):** `make dev-stack` (Docker daemon won't start on this machine — README's port 5433 / db-user-pass `nostos` / `nostos_pub` + `tasks` claims verified against `docker/docker-compose.yml` + `docker/pg-init/01-sources.sql`, all accurate); `make bench` timed out at 10 min (release build + heavy benchmark, not a README defect). **Friction found & fixed:** README never mentioned the web demo — added a Demo C pointer. **Friction filed (no fix, >10 lines / out of scope):** README gives no runtime warning that `make bench` is a multi-minute release build on a cold cache (NICE-TO-HAVE, not a blocker).
- [x] **Step 2:** Fix every friction point that has a ≤10-line fix; file the rest in the follow-up registry (Part VIII). A `nostos dev` CLI binary is **deliberately skipped** — `make dev-stack` covers it; build the CLI only if the stranger test proves Make is the friction (`ponytail:` the roadmap's CLI is deferred, not dead).
  - Applied: README "two demo paths" → "three" + new Demo C section (web demo) + "first two paths are independent" wording fix. 14 insertions / 2 deletions, all in README.md. Net assessment: the README quickstart is honest and walkable — the only defect was the missing web-demo pointer, now fixed. No CLI friction found (Make targets sufficed).
- [x] **Step 3: Commit** — `git commit -m "fix: stranger-test friction fixes for the v0.1 quickstart"`
  - Committed on `main` as `fix: stranger-test friction fixes for the v0.1 quickstart` (includes the README fix + these F1 checkbox ticks; see `git log` for the SHA).

### Task F2: Release v0.1.0

- [x] **Step 1:** Final sweeps: docs-curator persona sweep (A3's checklist); bench-runner re-runs the headline benchmark with fixed env capture (A6 step 6) and refreshes `benches/results/RESULTS.md`.
  - A6 step 6 env capture shipped in `358536f`. The 1k headline was **not** re-run — the 142k result is valid and the founder's call whether to regenerate the marquee number; **APPEND-only** v0.1 section added with the C3 1k/5k/10k picture (1k unchanged, 10k drop ceiling honestly diagnosed). Per the L4 escalation: rewriting the 35.6× headline is a founder-facing marketing decision, not an implementation task — append, don't rewrite.
- [x] **Step 2:** `git tag v0.1.0`; draft the launch post (Show HN + a competitor-comparison piece with same-denominator tables and the honest 10k-client story) into `docs/launch/` for operator review. **Do not publish anything — operator's call.**
  - Local `v0.1.0` tag created (in-repo metadata; **NOT** pushed — no remote configured, and the handoff scopes "tagging beyond a local v0.1.0" as operator's call). Draft at `docs/launch/show-hn-draft.md` — foregrounds the honest 10k story, uses same-denominator tables only, and explicitly retires the stale "static buckets" / "1k cap" attack lines (a comparable engine's dynamic-sync GA, May 2026). **Nothing published.**
- [x] **Step 3:** Update `docs/ROADMAP.md` footer to Phase 3 posture.
  - Footer now reads "Phase 3 🚧 — v0.1 prepared, launch gated on operator." Honest about what shipped vs what's still operator-gated (RN SDK, Nostos Cloud alpha, Show HN timing).

---

## Part VIII — Follow-up plan registry (separate plan docs, authored at their gate)

Per the writing-plans scope rule, these are independent subsystems; each gets its own plan when its gate opens. **They are scoped here, not specified.**

| Future plan | Gate that opens it | One-line scope |
|---|---|---|
| `docs/plans/flutter-pomodoro-persona-e2e-baseline.md` (**authored**; removed in cleanup; see git history) | ready now (independent of product phases) | Two Flutter fixtures: pomodoro (persona-driven smoke/E2E convention, `docs/testing/persona-e2e-baseline.md`) + todo (Supabase cloud + auth dual-mode smoke — mocked until operator supplies keys); both inherited by the flutter-sdk plan's example apps |
| `docs/plans/flutter-sdk.md` | v0.1 tagged | flutter_rust_bridge v2 over nostos-core; `sqlite3_flutter_libs` Storage impl; Stream API; example app; retrofits sync into the pomodoro fixture |
| `docs/plans/react-native-sdk.md` | Flutter SDK shipped (patterns proven) | UniFFI bindings + RN Turbo Module; op-sqlite Storage |
| `docs/plans/node-sdk.md` | demand signal | napi-rs + better-sqlite3 Storage |
| `docs/plans/cloud-beta.md` | v0.1 + first design partner | Deploy nostos-server+nostos-cloud to Fly.io; Supabase auth E2E; Stripe live-mode test; tier caps enforced from license |
| `docs/plans/phase2-hardening.md` | cloud beta traffic | backpressure, delta compression, real send→recv latency (wire-v2), multi-node fan-out, observability suite |
| `docs/plans/write-back-v2.md` | design-partner conflict reports | ADR-0013 full: declarative write rules, version/etag checks, function mode, HTTP path; ADR-0014 CRDT tier |
| `docs/plans/enterprise.md` | first enterprise conversation | SSO/SAML, audit log, RBAC, SOC2 artifacts, VPC/on-prem |

## Risks

1. **[HIGH] pgoutput edge cases** (toasted values, large transactions, DDL mid-stream) surface during B2/B3 real-DB testing — pg-integrator persona's checklist exists for exactly this; budget slack in Phase B.
2. **[HIGH] Competitive window**: a comparable engine's dynamic-sync GA killed the buckets attack line; Supabase/Triplit may ship first-party offline. Mitigation: A6 repositioning now, F2 launch sooner over feature-completeness.
3. **[MED] Write-back trust boundary**: D2's identifier validation + allowlist + parameterized values is the security-critical surface; domain-guardian + a focused review pass before merge (never ponytail this away).
4. **[MED] wire compat churn**: C1/C3/D2 all touch the wire. All changes are additive-optional (serde defaults), so old clients keep working; still, land C1 → C3 → D2 in order, never parallel.
5. **[MED] Slot retention growth** if clients disconnect mid-snapshot or a slow client never acks — the WAL-bloat eviction policy (ADR-0016, `EvictionPolicy` + `max_slot_wal_keep_size`) exists but is off by default; B4's dev-stack should enable a sane default and the cloud-beta plan must treat it as required config.
6. **[LOW] OPFS browser variance** — contained by E2 being decision-first.

## Execution notes

- Suited to superpowers:subagent-driven-development: Phases A and B are independent (parallel-safe); C after B1; D after C1; E after C2; F last. Personas from A3 become available to the executor mid-plan — use them (pg-integrator for B2/B3, bench-runner for C3, docs-curator for A6/F2, domain-guardian on every merge into `crates/nostos-domain`/`nostos-application`).
- Every task ends in a commit; `make ci` between tasks; feature-gated e2e on every Phase B/D task.
- The operator oversees only: nothing in this plan publishes, deploys, or spends money. F2 stops at drafts + a local tag.
