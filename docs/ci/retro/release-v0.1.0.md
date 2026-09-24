# v0.1.0 (historical — no binaries)

Historical — no binaries. A notes-only release recreated for the tag created on 2026-07-05; `release.yml` never ran for it, and it is not re-run for old code.

**Range:** first commit…`v0.1.0` · **retro PRs:** #1–#4 · **commits:** 56

## Tag message

> v0.1.0 — real-PG default + write-back v1 + WASM transport + browser demo
>
> Code-complete v0.1 scope:
> - Real Postgres logical replication by default + initial snapshot (B1, B2)
> - where_sql predicate subscriptions, server-enforced tenant clauses (C1, C2)
> - WS write batching, reconnect-storm probe (C3)
> - Write-back v1: allowlist + parameterized + typed-inference binding (D2)
> - Durable client outbox; chaos write-resume (D3, D4)
> - WASM WebSocket transport (E1); OPFS deferred per ADR-0017 (E2)
> - Browser demo page (E3); stranger-tested README quickstart (F1)
>
> 1k-client headline 142k ops/sec @ 0% drops = 35.6x PowerSync's published
> server ceiling (unchanged). 10k-client drop ceiling diagnosed honestly:
> table-sharded router is the Phase 2 fix.
>
> Local tag only — not pushed (no remote configured). Publication, RN SDK,
> Nostos Cloud alpha, and Show HN timing are operator calls.

## Commits

### Features

- Week-1 spike — Nostos sync server + throughput moat benchmark (`b672a92`)
- complete nostos bottom-to-top — cloud, web, reactive-default, tests, docs (`16c0f6f`)
- Tier 0+1 foundation — ack-driven LSN resume, /sync auth, server-enforced predicates (`48d6875`)
- Tier 2 — client SDK core + durable checkpoint + WAL-bloat protection (ADR-0016) (`804919c`)
- Tier 2.5 — WASM FFI bridge, the first FFI target (ADR-0015) (`3874298`)
- Tier 3 — ADR-0012 predicate engine, slice 1 (boolean tree) (`d94eadb`)
- Tier 3.5 — ADR-0012 slice 2 (typed comparison + JSON extractor) (`066177c`)
- Tier 6 — native reactive_scroll example (make the moat visible) (`2dc32a4`)
- Tier 7 — safe-SQL-subset predicate compiler (the final verifiable increment) (`d0c7fe2`)
- add domain-guardian, pg-integrator, bench-runner, docs-curator agent personas (`c09420e`)
- add verify-nostos project skill with tiered verification ladder (`dca9369`)
- scaffold flutter pomodoro fixture with TimerConfig and Ticker port (`c8773c7`)
- pomodoro viewmodel with ticker port, lifecycle auto-pause, and equivalence-proven transitions (`556eab3`)
- pomodoro view with keyed controls and demo-mode entrypoint (`0d5f3e0`)
- fixture make verbs, persona-e2e baseline convention doc, master plan registry entry (`dbe3935`)
- todo fixture scaffold with auth and repository ports, fakes, supabase adapters, env seam (`1eba195`)
- todo fixture sign-in and list views with viewmodels over mocked ports (`adbd73b`)
- compile pg replicator by default with actionable misconfiguration errors (`ce6547a`)
- initial snapshot via COPY under exported slot snapshot — fresh clients get existing rows (`7d631c5`)
- make dev-stack one-command real-Postgres quickstart (`4def99f`)
- wire safe-SQL predicate compiler into subscribe path with server-enforced tenant clauses (`7df8cd1`)
- where_sql subscriptions in native client, wasm bridge, and reactive_scroll demo (`a88a7e1`)
- batched WS writes — 10k-client drops 67.5% -> 61.4%, 1k headline flat (`00536b0`)
- write-back v1 — WriteBack port, PgWriteBack upsert/delete with table allowlist, wire and transport (`dffb87a`)
- durable client outbox — offline writes survive restarts and flush on reconnect (`4a39b1d`)
- wasm websocket transport — browser subscribes, applies, acks, resumes from localStorage checkpoint (`a5260a2`)
- browser demo page — live filtered sync via wasm bridge (`14cc099`)

### Bug Fixes

- drive persona journeys by injected ticker — reliable desktop E2E without wall-clock dependence (`bf3d670`)
- stranger-test friction fixes for the v0.1 quickstart (`07b3967`)

### Refactor

- apply ponytail-audit cuts (-137 lines, dead code & yagni) (`f33fed7`)

### Documentation

- Tier 5 — index experiment built, measured 4-8x regression, REVERTED (`4937378`)
- add complete-nostos implementation plan — product completion plus agent operating layer (`598fff0`)
- fold advisor review into plan — snapshot concurrency test, reconnect-storm probe, idempotency proof, slot-retention risk (`7ba06e1`)
- add flutter pomodoro fixture persona-e2e baseline plan and register it in the master plan (`97fd09e`)
- fold todo fixture with dual-mode supabase auth smoke into the flutter fixtures plan (`0b6be48`)
- add implementation handoff entrypoint for the next agent (`86b9c14`)
- add project memory to CLAUDE.md and AGENTS.md alias for agent onboarding (`13dd4e9`)
- truth sweep — status, crate map, benchmark honesty, July-2026 competitive repositioning (`358536f`)
- tick Phase A checkboxes — agent operating layer complete (`d948646`)
- pomodoro personas as testable specs with persona-journey mapping guard (`8debc15`)
- tick fixtures Part I checkboxes — pomodoro baseline complete (`47f1e3c`)
- tick fixtures Part II task checkboxes — todo scaffold, views, smoke done (operator handoff items remain) (`196e2c8`)
- ADR-0013 addendum — v1 write-back over sync socket, LWW, table allowlist (`634ff9f`)
- tick Phase B/C/D checkboxes — real PG, predicates, write-back complete (`7641297`)
- ratify B1 pg_url default + D2 typed-binding decision, annotate B3 remote-push precondition (`7469ec2`)
- ADR-0017 web persistence decision — defer OPFS past v0.1, commit SQLite-WASM opfs-sahpool post-launch (`68b5dc2`)
- v0.1 release prep — RESULTS v0.1 section, launch post drafts, ROADMAP Phase 3 footer (`1ee2cd9`)

### Testing

- Tier 4 — 10k-predicate fan_out baseline (measure-before-optimize) (`bdbba86`)
- Tier 4.5 — re-measure with parse-once extractor (corrected baseline) (`3162550`)
- pomodoro boot smoke test on macos target (`c2ab75e`)
- persona e2e journeys for maya, sam, and rio in compressed demo time (`e6a9e11`)
- dual-mode supabase auth smoke for todo fixture — mocked now, live on operator credentials (`8817fa4`)
- chaos write-resume — offline writes + mid-stream restart, zero loss zero duplication (`b045dc0`)

### Miscellaneous Tasks

- add agent permissions allowlist for autonomous build-test loops (`2317fe6`)
- add CONTRIBUTING, cargo-deny gate, editorconfig; fix stale PgReplicator stub comment (`70fdd77`)
- run real-Postgres logical-replication e2e on every push (`f86b90c`)

---

🤝 Collaborated with Claude via [Claude Code](https://claude.com/claude-code)
