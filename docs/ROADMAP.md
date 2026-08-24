# Nostos Roadmap

> The plan to go from Week-1 spike to v1.0 open-source launch. Each phase has a **single headline deliverable** and a **kill criterion.**

---

## Phase 0 — Spike & prove the moat  *(Week 1 — this repo, today)*

**Headline:** An auditable benchmark proving Nostos's Rust server fans replication events to **≥5× PowerSync's 2–4k ops/sec ceiling** at 1k/5k/10k concurrent WebSocket clients.

**Deliverables:**
- Hexagonal server skeleton: `nostos-domain` / `nostos-application` / `nostos-infra` / `nostos-server`.
- `FakeReplicator` → `FanOutService` → bounded per-session sinks → WebSocket transport.
- `nostos-bench` harness: in-process WS client swarm, measures sustained ops/sec + drop rate + p99. *(C3 — batched WS writes shipped: the per-session write task now drains up to 64 immediately-available frames into one JSON-array WS message under backlog, while sending a single object — byte-identical to the legacy wire — when only one frame is pending (zero latency tax at low rates). Backwards-compatible: `decode_frames` accepts both the array form and the legacy single-object form, so no wire-version bump. **Measured on Apple Silicon / 10 cores / rustc 1.95.0** — 1k headline: 833k → 833k ops/sec @ 0% drops (within noise, no regression); 5k: 592k → 660k ops/sec, drops 0.00% → 0.91%; 10k (probe — the full harness's `FanOutService::run` is O(N×E) via per-event full-store ack/eviction scans and hangs in teardown at 10k, so a lean `nostos-bench-10k` shim measures it): ~406k → ~483k ops/sec, drops ~67.5% → ~61.4%. **The 10k <1%-drop goal was NOT met** — batching is a strict improvement at every tier but the dominant 10k cost is the per-event store scan, not the per-connection WS write path; the named follow-up is the table-sharded router. ws_contract 8/8 green. Reconnect-storm probe (`nostos-reconnect-storm`): dropping+reconnecting 1k–2k of 2k–3k clients mid-stream drains cleanly — post-storm drop rate 0.00% across runs (pre-storm 0–14% reflects steady-state noise); **admission control / token-bucket NOT needed** and not built speculatively.)*
- `RESULTS.md` with the comparison chart vs PowerSync's published limits.

**Kill criterion:** if we can't demonstrate ≥3× over PowerSync's ceiling, the architecture is wrong — pivot before building more.

---

## Phase 1 — Core + real Postgres  *(Weeks 2–3)*

**Headline:** Nostos reads a real Postgres publication via logical replication and syncs to a local SQLite in one client.

**Deliverables:**
- `PgReplicator` — real `pgoutput` parsing via `tokio-postgres` + `pgoutput` crate. LSN checkpointing, slot management, reconnect/heartbeat.
- `nostos-core` client crate (Rust, no FFI yet) — applies `RowOp`s to a `rusqlite`-backed `Storage` trait.
- Durable checkpoint (LSN) on the client; reconnect resumes exactly where it left off.
- Chaos test: kill the server mid-stream → client reconnects → no data loss, no duplication.

**Kill criterion:** if the PG logical-replication state machine can't survive a mid-LSN crash without data loss or duplication, we don't have a product — fix before anything else.

**Ratified decisions (2026-07):**
- `NOSTOS_PG_URL` defaults to empty, not `localhost:5433`. Selecting
  `NOSTOS_REPLICATOR=pg` without a URL fails fast with the actionable error
  `Set NOSTOS_PG_URL, e.g. after: docker compose -f docker/docker-compose.yml up -d`.
  Rationale: a silent fallback to a localhost DB that may not exist masks
  misconfiguration; an actionable error is the correct operability bar for a
  real-PG-by-default binary.
- Write-back parameter binding is typed-inference (`SqlValue`), not the plan's
  text-cast-with-coercion — Postgres does not coerce `text`→`uuid` parameters.
  See ADR-0013 addendum "Typed parameter binding".

---

## Phase 2 — Dynamic predicates + multi-platform SDKs  *(Weeks 4–5)*

**Headline:** A client subscribes with a live predicate and scrolls forever — on Flutter AND Web.

**Deliverables:**
- Predicate expression engine (boolean tree of equalities/ranges over auth-scoped params). *(ADR-0012 — moat complete: boolean tree `And|Or|Not` + typed comparison `Lt|Gt|Le|Ge` over `Number/Float/Bool/Text`, proven against real PG rows via the JSON column extractor. **Baseline:** ~150-170 eval-only events/sec through 10k predicates (~1.5M predicate-evals/sec — already orders of magnitude above the PowerSync 2-4k ops/sec ceiling). An equality index was built, measured a 4-8× regression, and **reverted** — the eval loop is structurally the cost but not the binding constraint; index deferred until a real load shows it binding.)*
- **Native reactive-scroll example** — `cargo run -p nostos-client --example reactive_scroll` makes the moat visible: in-process server + durable SQLite client + typed predicate + mid-stream server restart with zero-loss resume. The native path's `chaos_resume` property, demonstrated end-to-end. *(First visible demo; Flutter/Web product surface still to come.)*
- `nostos-core` WebAssembly build (`wasm-bindgen` + OPFS storage). *(✅ in-memory apply bridge shipped ADR-0015; OPFS persistence deferred — Worker-only by spec.)*
- Flutter SDK via `flutter_rust_bridge` (first-class `Stream`). *(✅ **shipped** — `nostos_flutter`
  with `NostosDatabase.watch` returning a hot, replay-shared `Stream`, a typed `Collection<T>`
  facade (ADR-0024) and `SyncStatus` (ADR-0027); proven by the `flutter` `sdk-e2e` slice against a
  real server. This line read "ADR-0015 — deferred" until 2026-07-30.)*
- The first end-to-end demo: "point at Supabase Postgres → offline reads on Flutter + Web." *(gates on OPFS + transport + Flutter.)*

---

## Phase 3 — OSS launch  *(Week 6)*

**Headline:** Apache-2.0 v0.1 on GitHub. Show HN. "PowerSync vs Nostos" + "Migrate from Realm" posts.

**Deliverables:**
- React Native SDK (UniFFI for RN Turbo Modules).
- Free Nostos Cloud alpha.
- Migration guides + the auditable benchmark repo as the centerpiece.
- Supabase partnership outreach (be their officially-recommended offline layer).
- Sync-aware push (ADR-0037) + **nostos-pushd** standalone push daemon (ADR-0038, launch-blocker decision 2026-08-17) — the only push server with a sync-aware upgrade path; `nostos push init|check` credential ergonomics.

---

## Phase 4 — The DX moat  *(Weeks 7–9)*

**Headline:** "Point us at your Postgres; we handle offline reads *and* writes."

**Deliverables:**
- **Direct write-back** — declarative write rules + transactional version/etag checks. Nostos applies queued mutations to Postgres for you. No more `uploadData()` endpoints.
- **Tiered conflict resolution** — LWW (default) → CRDT-per-field (opt-in, via Loro-style primitives) → custom merge functions.
- **Dynamic reactive sync GA** — the bucket-less default.
- Nostos Cloud GA + transparent pricing live.

---

## Phase 5 — Enterprise  *(Weeks 10–12)*

**Headline:** First paid Enterprise pilots.

**Deliverables:**
- SSO/SAML, audit log, SOC2-in-progress, HIPAA artifacts.
- Field-level encryption key management, RBAC.
- VPC peering / on-prem connect.
- Case studies from design partners.

---

## Out of scope (deliberately)

- **Collaborative rich-text editing** — Yjs/Loro/Liveblocks own it; CRDTs are the wrong primitive for relational sync.
- **Non-Postgres source DBs** — MongoDB/MySQL source support is a *later* adapter, not a launch feature. We lead with Postgres because that's the white space.
- **A managed IDE/studio product** — possible later (seat-based), not v1.0.

---

## Status legend

- 🚧 **Spike** — proving feasibility, not a product.
- 🔬 **Alpha** — feature-complete for the phase, known rough edges.
- 🚀 **GA** — production-ready for the phase's scope.
- 📈 **Scaling** — optimization & hardening.

Today: **Phase 3 🚧 — v0.1 prepared, launch gated on operator.** v0.1 scope is code-complete: real-PG default + snapshot, `where_sql` predicate subscriptions, WS batching, write-back v1 with offline outbox, WASM transport + `/demo` page, two Flutter fixtures, stranger-tested README quickstart. RESULTS.md carries the honest 1k/5k/10k picture (**1k headline refreshed 2026-07: 833k ops/sec aggregate fan-out @ 0% drops** — PowerSync publishes no comparable aggregate fan-out figure, only 2–4k ops/sec replication ingest and 2–20k ops/sec per-client sync; the Week-1 baseline of 142k ops/sec is preserved as historical. 10k drop ceiling diagnosed — table-sharded router is the Phase 2 fix). Router status 2026-08-24: PARKED — full-path single-client evidence shows drain is not scan-bound; revisit on the first multi-client real-PG run that shows the O(N×E) scan binding. Push: ADR-0037 sync-aware push piloted in atlet on real APNs/FCM rails (plan 24/24); ADR-0038 nostos-pushd daemon + RemoteNotifier delegation implemented per `docs/plans/nostos-push-daemon-implementation.md`. Launch post drafts in `docs/launch/`, local `v0.1.0` tag; **publication, the RN SDK, Nostos Cloud alpha, and Show HN timing remain operator calls** (see `docs/plans/complete-nostos-fully-wired-operational.md` Phase F2).
