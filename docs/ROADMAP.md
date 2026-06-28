# Nostos Roadmap

> The plan to go from Week-1 spike to v1.0 open-source launch. Each phase has a **single headline deliverable** and a **kill criterion.**

---

## Phase 0 — Spike & prove the moat  *(Week 1 — this repo, today)*

**Headline:** An auditable benchmark proving Nostos's Rust server fans replication events to **≥5× PowerSync's 2–4k ops/sec ceiling** at 1k/5k/10k concurrent WebSocket clients.

**Deliverables:**
- Hexagonal server skeleton: `nostos-domain` / `nostos-application` / `nostos-infra` / `nostos-server`.
- `FakeReplicator` → `FanOutService` → bounded per-session sinks → WebSocket transport.
- `nostos-bench` harness: in-process WS client swarm, measures sustained ops/sec + drop rate + p99.
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

---

## Phase 2 — Dynamic predicates + multi-platform SDKs  *(Weeks 4–5)*

**Headline:** A client subscribes with a live predicate and scrolls forever — on Flutter AND Web.

**Deliverables:**
- Predicate expression engine (boolean tree of equalities/ranges over auth-scoped params). *(ADR-0012 — moat complete: boolean tree `And|Or|Not` + typed comparison `Lt|Gt|Le|Ge` over `Number/Float/Bool/Text`, proven against real PG rows via the JSON column extractor. **Baseline:** ~150-170 eval-only events/sec through 10k predicates (~1.5M predicate-evals/sec — already orders of magnitude above the PowerSync 2-4k ops/sec ceiling). An equality index was built, measured a 4-8× regression, and **reverted** — the eval loop is structurally the cost but not the binding constraint; index deferred until a real load shows it binding.)*
- `nostos-core` WebAssembly build (`wasm-bindgen` + OPFS storage). *(✅ in-memory apply bridge shipped ADR-0015; OPFS persistence deferred — Worker-only by spec.)*
- Flutter SDK via `flutter_rust_bridge` (first-class `Stream`). *(ADR-0015 — deferred.)*
- The first end-to-end demo: "point at Supabase Postgres → offline reads on Flutter + Web." *(gates on OPFS + transport + Flutter.)*

---

## Phase 3 — OSS launch  *(Week 6)*

**Headline:** Apache-2.0 v0.1 on GitHub. Show HN. "PowerSync vs Nostos" + "Migrate from Realm" posts.

**Deliverables:**
- React Native SDK (UniFFI for RN Turbo Modules).
- Free Nostos Cloud alpha.
- Migration guides + the auditable benchmark repo as the centerpiece.
- Supabase partnership outreach (be their officially-recommended offline layer).

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

Today: **Phase 0 🚧.**
