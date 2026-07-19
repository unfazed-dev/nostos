# Nostos Sync Strategy — Analysis & Recommendation

**Date:** 2026-07-19 · **Author:** tech-lead (Claude, GLM-5.2) · **Method:** 3-agent fan-out (code archaeology + industry research + PowerSync competitive) + architecture-domain consultant (conf HIGH)

---

## TL;DR (answer the operator's literal questions)

| Question | Answer |
|---|---|
| **What is nostos's current strategy?** | **Server-authoritative offline-first**: local-SQLite reads (work offline) + optimistic local writes to a durable outbox + store-and-forward flush on reconnect + ack-driven LSN resume + last-write-wins (LWW) conflict resolution, over a single snapshot-then-stream `/sync` WebSocket. |
| **What is the default?** | There is only **one** strategy. Current == default == only. |
| **Does nostos have a strategy enum / config flag that can change it?** | **No.** No `SyncStrategy` / `sync_mode` / `consistency` type exists anywhere in `crates/`. The only mode-like flags are `NOSTOS_REPLICATOR` (switches *data source*: pg/fake) and `NOSTOS_SYNC_AUTH` (switches *auth*: none/supabase-jwt) — neither switches *strategy*. |
| **"Queue while disconnected, replay on resume" — what is that called?** | It is **not a different strategy from offline-first — it IS offline-first.** The precise sub-pattern name: **"optimistic local writes + persistent mutation queue (store-and-forward)."** The operator's prompt inverted the terminology. |

---

## 1. nostos's actual strategy (verified from code)

Source: code-archaeology agent over `crates/` + `sdk/nostos_flutter/`.

| Dimension | nostos's behavior | Evidence |
|---|---|---|
| **Source of truth** | Server-authoritative (Postgres). Client holds a cache + outbox, not canonical state. | ADR-0013 addendum; collapsed write-back |
| **Reads while offline** | **Local-first** — reads hit on-device SQLite (`cairn_data` via `json_extract`). Fully offline-capable. | `sdk/nostos_flutter/lib/src/nostos_database.dart:223-226`; ADR-0013 addendum "On-device SQL read surface" |
| **Writes while disconnected** | **Optimistic local apply + durable outbox queue.** `SyncClient::write` enqueues to SQLite outbox, applies locally, returns immediately. Flush loop drains `Outbox::pending()` on reconnect. Exponential backoff; dead-letter at 50 attempts. | `crates/nostos-client/src/client.rs` (write + run_once); `crates/nostos-core/src/outbox.rs:53-59` (`mark_dead_letter`) |
| **Delivery model** | **Snapshot-then-stream** (initial COPY snapshot at consistent-point LSN, then WAL changes). Only model — no real-time-only path. | `crates/nostos-infra/src/replicator/pg.rs:452`; `snapshot.rs:29-52` |
| **Consistency** | **Eventual consistency** with ack-driven LSN resume + exactly-once. Server advances slot by *min* acked LSN (slowest client wins). | ADR-0009 |
| **Conflict resolution** | **Server-authoritative LWW by WAL order.** No client-side merge code exists. | ADR-0014 tier (a); ADR-0013 addendum |
| **Transport** | Single `/sync` WebSocket per session. | ADR-0013 |

**So:** nostos is **offline-first** (it meets every clause of the definition: local reads work offline, local writes queue offline, sync resumes on reconnect). The behavior cited in the prompt is the *defining* behavior, not a counter-example.

---

## 2. The config surface today (what IS flaggable vs hard-baked)

**Behavior-affecting env vars** (`crates/nostos-server/src/main.rs:38-159` `Config` struct):

- `NOSTOS_REPLICATOR` = `fake` | `pg` — **data source**, not strategy.
- `NOSTOS_SYNC_AUTH` = `none` | `supabase-jwt` — **auth**, not strategy.
- `NOSTOS_WRITE_TABLES` — write allowlist (ADR-0013).
- `NOSTOS_PG_URL` / `_SLOT` / `_PUBLICATION` / `_SLOT_WAL_KEEP_SIZE` / `NOSTOS_SLOT_MAX_LAG`.
- `NOSTOS_BIND`, `_WS_PATH`, `_SESSION_BUFFER`, `_LOG`, `_CORS_ORIGINS`, `_TIER`, `_LICENSE`.

**Client knobs** (`SyncClientConfig`, `client.rs:90-200`): `base_backoff`, `max_backoff`, `max_retries`, `idle_timeout`, `flush_quiesce` (50 ms), `dead_letter_max_attempts` (50).

**Hard-baked (NOT configurable):** LWW conflict resolution · snapshot-then-stream delivery · single-WS transport · server-authoritative source of truth · ack-driven LSN resume · CRDT/custom-merge tiers (Phase 4) · HTTP write-back endpoint (Phase 4).

**Code-only builders (NOT env-exposed):** `FanOutService::with_push_interval` (default `Duration::ZERO`) and the eviction policy — these are the cheap wins (see §5).

---

## 3. Industry strategy taxonomy (the menu nostos *could* draw from)

| Strategy | When to use | Representatives |
|---|---|---|
| **Local-first (full)** — local is canonical, sync is optimization | Ownership + offline + collaboration | Linear, Obsidian, Automerge, **nostos** |
| ↳ *optimistic writes + store-and-forward* (sub-pattern) | Offline *writes* | **nostos**, PowerSync (FIFO upload queue), Replicache (mutator+rebase) |
| ↳ *read-only offline* | Dashboards, reference data | ElectricSQL (read-path), Firebase+persistence |
| **Online-first / server-authoritative cache** | Traditional SaaS, low write concurrency | REST/GraphQL, Convex |
| **Real-time push (WS/SSE)** | Live dashboards, chat, presence | Supabase Realtime, Firebase, Liveblocks |
| **Polling** | Low-frequency, no push infra | Traditional REST, RxDB polling |
| **CRDT-based** (mathematically convergent, no central authority) | Concurrent multi-writer + offline + **decentralized** | Yjs, Automerge, Loro, Ditto |
| **Operational Transform** | Centralized collaborative text editing | Google Docs, ShareDB |
| **Log-based / logical replication** (WAL stream) | Server-authoritative + offline, DB already exists | **nostos**, PowerSync, ElectricSQL, Datomic |
| **Patch-based** (idempotent server patches) | Flexible mutation semantics | Replicache, Datomic |

**Best-practice hierarchy** (use the simplest that works): polling → real-time push → server-authoritative WAL/patch → CRDTs.

**Use CRDTs only when ALL of:** concurrent multi-user edits + offline + decentralized authority. **Do NOT use CRDTs when** you need hard invariants, uniqueness/exclusivity, strict global ordering, or server-side validation (Loro docs 🔥) — they merge, they do not reject. **nostos is server-authoritative with Postgres validation → correctly does NOT use CRDTs today.**

---

## 4. "Do we need a strategy enum?" — verdict: **NO**

**Industry consensus (strong):** exposing a *top-level consistency-model enum* in one product is an **anti-pattern** — it creates confusing mental models and edge cases at strategy boundaries. No mainstream sync engine does it (PowerSync, Replicache, ElectricSQL, Convex, Zero are each opinionated about *one* strategy). The dominant *good* pattern is **per-field opt-in**: default LWW server-authoritative, allow specific fields to opt into CRDT semantics (Ditto does this).

**nostos is already on the right side of this:** ADR-0004 / ADR-0014 ratify a 3-tier conflict model —
- **tier (a) LWW** — shipped (today).
- **tier (b) CRDT-per-field** — reserved, deferred to Phase 4.
- **tier (c) custom-merge** — reserved, deferred to Phase 4.

**That per-field tier IS the "multiple strategies" surface the operator is asking for — it already exists as a design, it just isn't implemented past tier (a).** A new top-level enum would be redundant with it *and* contradict both the ADRs and the industry consensus.

---

## 5. Recommendation (consultant-confirmed, conf HIGH)

**Ranked options:**

| Option | Verdict | Why |
|---|---|---|
| **(a) Build a top-level `SyncStrategy` enum** | ❌ **WORST.** Don't. | Directly contradicts ADR-0004/0014 + industry consensus. Mixes consistency models in one product = anti-pattern. |
| **(b) Implement conflict tiers (b) CRDT-per-field + (c) custom-merge now** | ⏸️ **Defer.** Runner-up but rejected for now. | Phase-4 scope; zero demonstrated user demand. Shipping speculatively burns timeline for generality nobody asked for. The ADRs already reserve the design space — it stays available. |
| **(c) Expose code-only operational knobs as env vars** | ✅ **DO NOW.** Days, not months. | `push_interval` and `eviction_policy` are already builders, just not env-exposed. Near-zero cost; addresses the real underlying desire (operational control). |
| **(d) Reframe "multiple strategies" as positioning, not code** | ✅ **DO NOW.** | The operator's ask is best answered by *naming* nostos's single coherent model correctly and contrasting it against competitors' multi-config confusion — not by building the confusion into nostos. |

**Chosen path: (d) + (c).** (a) rejected; (b) deferred to Phase 4 / first real CRDT-demand user.

### Risk register

- **[CRITICAL]** If a real user needs CRDT semantics before Phase 4, the positioning deflection fails. → *Mitigation:* the tier-(b) design is reserved in ADR-0014; escalation path exists, just not implemented.
- **[HIGH]** Operators may equate "one strategy" with inflexibility if positioning isn't crisp. → *Mitigation:* the positioning doc below (§6) must be written.
- **[MEDIUM]** Exposed env-var surface can grow into accidental config bloat. → *Mitigation:* gate new knobs behind a documented "operational knobs" section; one-knob-one-purpose.
- **[LOW]** Deferred tiers may need rework if Phase-4 requirements shift. Acceptable — ADRs are cheap to amend.

### Concrete next actions (in priority order)

1. **Write a one-page positioning doc** (§6 below) naming nostos's model + mapping competitor "strategies" to nostos equivalents. ← highest leverage, ~1 hour.
2. **Expose `push_interval` + `eviction_policy` as documented env vars** with sensible defaults. ← cheap, high perceived flexibility.
3. **Add an FAQ entry**: *"nostos ships one coherent sync model with per-field conflict tiers (b/c) reserved per ADR-0014. We do not expose a top-level strategy switch — that is an industry-recognized anti-pattern."*
4. **Do NOT build the enum.** If pressed, cite ADR-0004/0014 + this doc.

---

## 6. Positioning doc (draft) — nostos vs the "multi-strategy" framing

**nostos's one coherent model:** *Server-authoritative offline-first sync over Postgres logical replication.* Local SQLite reads, optimistic local writes, store-and-forward outbox, ack-driven LSN resume, LWW-by-WAL-order conflicts. Per-field conflict-tier upgrade path (CRDT / custom-merge) reserved per ADR-0014.

**Why one strategy is a feature, not a gap:** every mature sync engine is opinionated about one consistency model (PowerSync, Replicache, ElectricSQL, Convex, Zero). A product that exposes a runtime strategy switch is signaling it couldn't pick — and forcing *you* to debug the boundary cases. nostos picks: server-authoritative + LWW, because Postgres is the source of truth and Postgres already enforces your invariants. When you need field-level CRDT semantics (collaborative text, counters), tier-(b) is the reserved seam — opt in *per field*, not per app.

**Mapping competitor "strategies" to nostos:**

| Competitor feature | nostos equivalent |
|---|---|
| PowerSync "persistent FIFO upload queue" | nostos durable outbox + store-and-forward (identical pattern) |
| PowerSync Sync Rules / Sync Streams DSL | nostos dynamic predicates (ADR-0003/0011/0012) — native code, no DSL cap |
| Replicache mutator+rebase | nostos collapsed write-back + server-authoritative apply |
| ElectricSQL Shapes (read-only) | nostos predicates over `cairn_data` views |
| CRDT mode (Yjs/Ditto) | nostos tier-(b), reserved per ADR-0014 |
| "Online mode" / cache-first | N/A — nostos is offline-first by design (the cache is on-device SQLite) |

---

## 7. PowerSync gaps nostos can exploit (ranked by defensibility)

From competitive research (primary sources 🔥):

| Wedge | Defensibility | Detail |
|---|---|---|
| **W1: License** | **Highest** | PowerSync is **FSL-1.1-ALv2** (source-available, *not* OSI; Competing-Use clause blocks anyone building a sync product; 2-year-per-release clock to Apache-2.0). nostos is **Apache-2.0** zero-restriction. Unavailable to a whole class of buyers (procurement, product-vendors). **Lead with this.** |
| **W2: Throughput ceiling** | High | PowerSync's *own published* service ceiling: ~2,000–4,000 ops/sec (small rows), ~5,000 ops/sec total, ~60 tx/sec small txns. nostos claims 142k ops/sec @ 1k clients / 0% drops. **Caveat:** PowerSync publishes no number *at 1k concurrent clients*, so the "35×" is vs the service ceiling — apples-to-oranges until nostos publishes a matched-load bench. Risk: PowerSync publishes a higher number and closes the lane. |
| **W3: No bucket-storage tax** | High | PowerSync requires a **second database** (Mongo or Postgres) just for bucket state — extra ops, failure mode, cost. nostos: source Postgres is the only DB. PowerSync cannot remove this without rewriting storage. |
| **W4: Dynamic predicates over a DSL** | Lowest | PowerSync locks you into Sync Streams DSL with a 1,000-bucket-per-user fan-out cap (`PSYNC_S2305`). nostos predicates are native code, no such cap. Real, but PowerSync can extend the DSL / raise the cap — least durable. |

**Honest strengths to respect (not a hit piece):** PowerSync has the **broadest SDK coverage** (Flutter, RN, KMP, Swift, Web) — Flutter-first-class is exactly nostos's target, so nostos must match Flutter parity. PowerSync has mature ops (Cloud dashboard, monitoring, Sync Diagnostics), strong cross-data consistency, and beyond-Postgres sources (Mongo/MySQL/SQLServer/Convex). It is the production reference for Flutter local-first in 2026. **nostos's edge is narrow and specific — license + throughput + architectural simplicity.**

**Not recommended as wedges:** AI-privacy (PowerSync already ships E2EE + HIPAA — contested lane); collapsed-write-back (`NOSTOS_WRITE_TABLES` allowlist is itself friction — a tradeoff, not an advantage).

---

## 8. Claim list (Gate 4 — verified / assumed / unknown)

**VERIFIED (observed this session):**
- nostos has exactly one sync strategy; no `SyncStrategy`/`sync_mode`/`consistency` type exists (grep over `crates/` returned zero definitions). [code agent]
- Config-flag list + that flags switch source/auth, not strategy. [`main.rs:38-159`]
- Disconnected path = durable SQLite outbox + optimistic local apply + dead-letter. [`client.rs`, `outbox.rs:53-59`]
- LWW conflict resolution, tier-(a) shipped, tiers (b)/(c) reserved. [ADR-0014]
- Reads are local-SQLite, offline-capable. [`nostos_database.dart:223-226`]
- "Store-and-forward" is a sub-pattern of local-first, not a distinct strategy. [agent 2, Ink&Switch + PowerSync + Replicache primary docs]
- PowerSync published ceiling ~5k ops/sec; FSL-1.1-ALv2 license; requires 2nd bucket-storage DB. [agent 2, primary docs 🔥]

**ASSUMED (reasonable inference, couldn't fully verify):**
- "Top-level strategy enum is an anti-pattern" — *inferred from absence of counterexamples + architectural opinionation across PowerSync/Replicache/ElectricSQL; no single citable statement.* [agent 2 flagged ❄️] Carried into recommendation with appropriate hedging.
- nostos's 142k ops/sec @ 1k clients — from project memory / `benches/results/RESULTS.md`, **not re-verified this session**. The "35×" comparison is vs PowerSync's *service* ceiling, not a matched-load bench.
- Consultant recommendation (d)+(c) — conf HIGH, convergent with code + industry evidence, but it is a judgment call, not a proof.

**UNKNOWN (didn't / couldn't check):**
- Whether a real user will demand CRDT semantics before Phase 4 (the CRITICAL risk).
- PowerSync's response (if any) to nostos's benchmark claim.
- Additional env vars in `nostos-cloud` / `nostos-cli` beyond `nostos-server` (code agent scoped to `nostos-server/main.rs`).
- PowerSync's dedicated multi-region topology (no public docs found).

---

## Sources

**Industry / taxonomy (primary 🔥):** Ink & Switch local-first manifesto; Kleppmann local-first PDF + "CRDTs: The Hard Parts"; Loro "When Not to Use CRDTs"; PostgreSQL logical-replication docs; CouchDB/PouchDB conflict docs; RxDB replication docs; WatermelonDB sync docs; Supabase Realtime docs.

**PowerSync (primary 🔥):** Performance-and-Limits; Architecture (bucket system); Handling Update Conflicts; Sync Streams; Self-Hosting; Deployment Architecture; Monitoring; FSL legal page; GitHub issues #785 (multi-tab deadlock), #120 (watchOS), #314 (libsqliteJni.so).

**Secondary 🌡️:** QueryPlane comparison (2026-02); PowerSync blog; merginit guide (2025); wal.sh; evilmartians; HN threads.

Full URL list in the agent transcripts (sessions `ac038b1e2f7b79ca9` industry + `a19e8f61a6604a724` PowerSync).
