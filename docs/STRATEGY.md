# Nostos — Strategic Product Brief
### The open, Rust-fast, local-first sync engine.

> *"A cairn is a trail marker of stacked stones. When you're offline and lost, it's how you find your way home. Nostos is how your data does."*

**Status:** Founder strategy + v1 design — revised July 2026
**Author:** Founder (synthesized via deep research + GLM-5.2 L4 ultrathink)
**Tagline:** *Local-first sync that never gives up. Rust-fast. Apache-open. No write-back endpoints, no lock-in.*

---

## 0. TL;DR (read this if nothing else)

Nostos's four wedges (audited July 2026):

1. **A Rust server.** The server — the replicator, the sync router — is pure Rust (tokio + axum). Nostos's proven number: **2,618,601 ops/sec aggregate fan-out @ 1k clients, 0.00% drops** (eval-only — `FakeReplicator` on loopback, median of 3, 2026-09-02; the full real-PG → client-apply path is a separate figure, ~34.8k–36.2k rows/sec sustained — see [`benches/results/RESULTS.md`](../benches/results/RESULTS.md)). The Week-1 baseline of 142k is preserved as historical.
2. **Nostos is Apache-2.0 today** — OSI-open server, core, and SDKs; no change date, no no-compete clause.
3. **Write-back without endpoints.** ElectricSQL is read-path only. **Nostos's direct write-back** (ADR-0013) writes to your Postgres for you — no customer-built endpoints. **Honest trade-off:** because nostos writes directly as a least-privilege `BYPASSRLS` role, it **bypasses Supabase RLS** and substitutes its own authz (JWT ADR-0010 + table allowlist ADR-0013 + tenant-scope ADR-0018 — one column, coarser than arbitrary RLS). nostos wins for single-tenant / simple-tenant-scoped apps; for apps whose security model **is** complex per-user RLS, a split-write path through Supabase's Data API is the better fit. See [`docs/SECURITY.md`](SECURITY.md).
4. **Nostos self-host is free, full-featured, and unlimited.**

Meanwhile **ElectricSQL abandoned 2-way offline sync entirely (read-path only)**, **Zero is web-only and explicitly disabled offline writes**, and **Supabase Realtime has no offline/conflict/local-DB layer** (it's a feeder, not a competitor). **Threat:** Supabase acquired Triplit (Oct 2025) explicitly citing offline demand — a first-party offline layer from Supabase is the live risk; see §9.

**The white space:** *An Apache-2.0, Postgres-logical-replication-based, 2-way offline-first sync engine with first-class Flutter + React Native + Web SDKs, a Rust core, a Rust server, and a genuinely free self-host.* **No product occupies that cell today.** That cell is **Nostos**.

We win by commoditizing the engine (Apache-2.0, full-featured, unlimited self-host) and capturing value through a managed Cloud + Enterprise tier — the Supabase/Postgres play.

---

## 1. The opportunity — why now

### 1.1 Market
Local-first is going from fringe to default for any app that touches a flaky network (field workers, healthcare, logistics, travel, on-the-go consumer, AI agents operating offline). The collapse of MongoDB's **Atlas Device Sync / Realm** (deprecated) dumped an entire population of mobile devs into the market for an escape hatch. That's a captive, motivated, currently-underserved market.

### 1.2 The three vacated seats
| Seat | Who vacated it | Why |
|---|---|---|
| Open + Postgres-native + 2-way offline | **ElectricSQL** | Retreated to read-path only: *"Electric does not do write-path sync."* |
| Flutter/RN + offline writes | **Zero/Rocicorp** | Web-only, *explicitly "not local-first"*, no RN/Flutter SDK (RN is their #1 community ask). |
| Cheap + open + offline on Supabase | **Supabase Realtime** | Streams WAL but has zero offline/conflict/local-DB layer. Community consensus: *"impossible for Supabase to implement offline correctly at the framework level."* |

### 1.3 The demand signal
Supabase users needing offline are *forced* onto Electric/RxDB. A faster, cheaper, truly-open alternative lands on fertile ground.

---

## 2. Competitive intelligence

### 2.1 The landscape matrix (who owns which cell)
| Axis | Zero | Electric | Triplit | RxDB | Convex | Couchbase | PouchDB |
|---|---|---|---|---|---|---|---|
| **Offline-first (offline writes)** | ❌ | ❌ | ✅ | ✅ | weak | ✅ | ✅ |
| **2-way sync** | ✅ | ❌ (read-only) | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Postgres-native** | ✅ | ✅ | ❌ | plugin | ❌ | ❌ | ❌ |
| **Flutter** | ❌ | weak | ❌ | weak | weak | ❌ | ❌ |
| **React Native** | ❌ (asked-for) | ❌ | ❌ | weak | ✅ | ❌ | ✅ |
| **Web** | ✅ (best) | ✅ | ✅ | ✅ | ✅ | ❌ | ✅ |
| **Rust-fast** | ❌ (TS) | ❌ (Elixir) | ❌ (TS) | ❌ (JS) | ❌ | C/C++ | ❌ |
| **Truly open (OSI)** | ✅ Apache | ✅ Apache | ✅ MIT | partial | ❌ | ❌ | ✅ Apache |
| **Free self-host** | ✅ | ✅ | ✅ | partial | paid | paid | ✅ |

**The empty cell:** ✅ Offline + ✅ 2-way + ✅ Postgres + ✅ Flutter + ✅ RN + ✅ Web + ✅ Rust + ✅ OSI-open + ✅ free self-host. **That's Nostos.**

### 2.2 Don't go here (anti-segments)
- Rich-text collaborative editing → Yjs/Loro/Liveblocks own it (CRDTs required).
- Enterprise non-Postgres mobile sync → Couchbase owns it.
- Web-only realtime sync → Zero will beat us.
- Pure CDC/logical-replication *frameworks* → `pg_replicate`/Supabase-ETL exist; we're a *product*, not a library.

---

## 3. The competitive fronts to win on

Eight fronts. The first three are the headline moats; the rest are table-stakes we must match or beat.

### Front 1 — **Predicate-based Reactive Sync (cursor-resumable, no full reprocessing)**
> **Claim: *"Subscribe with a live predicate; scroll forever. Cursor-resumable, incremental — never full-reprocess."***

"Dynamic sync" alone is no longer a differentiator. What still is: Nostos's subscription model is **predicate-based and cursor-resumable from day one.** The client subscribes with a *live predicate* (a scoped, authorized query — e.g. `org_id == $org AND updated_at > $cursor`); the server continuously evaluates incoming logical-replication deltas against the set of *authenticated, live* client predicates (ADR-0012's shipped boolean-tree engine) and streams only matching deltas. State is **cursor-based (LSN + op offset)**, so it's resumable and **incremental** — a single change does *not* trigger full reprocessing of the matched set. A user with 100,000 items scrolls and syncs exactly what they look at, with no fixed cardinality ceiling.

### Front 2 — **Direct Write-Back (no endpoints)** 🏆 *DX moat*
> **Claim: *"Zero upload endpoints. Nostos writes to your Postgres for you — safely."***

**Nostos offers direct write-back:** you give Nostos a Postgres connection + declarative **write rules** (which columns, which auth scope, upsert vs. insert), and Nostos applies queued client mutations to Postgres with **transactional conflict detection** (version/column-etag checks) and applies your chosen merge strategy. A `function` mode remains for anyone who wants full control. Most teams never write a backend mutation endpoint again.

### Front 3 — **Rust Server Throughput** 🏆 *performance moat*
> **Claim: *"Faster server, lower tail latency, a fraction of the memory — proven on Nostos's own aggregate fan-out benchmark."***

**Nostos's server is pure Rust (tokio + axum)**, parsing `pgoutput` via `pgwire-replication`, fanning out to thousands of concurrent WebSocket clients with per-connection backpressure, measured at 2,618,601 ops/sec aggregate fan-out @ 1k clients, 0.00% drops (median of 3 passes, 2026-09-02, eval-only; the pre-fix figure was 833,307). We publish continuous benchmarks and **make the benchmark repo public** so the claim is auditable — see [`benches/results/RESULTS.md`](../benches/results/RESULTS.md).

### Front 4 — **Truly Open (Apache-2.0)**
> **Claim: *"Apache-2.0, end to end. Server included. No FSL trap, no 2-year wait, no no-compete clause."***

Enterprise legal treats source-available-with-restrictions as procurement friction. **Nostos is Apache-2.0 across server, core, and SDKs.** This is the single biggest non-technical wedge: it makes us the morally-and-legally clean default.

### Front 5 — **First-class Flutter + RN + Web from one core**
> **Claim: *"Every platform, one Rust core, first-class — not 'best on Flutter, alpha on the rest.'"***

**Nostos ships Flutter, React Native, Web (WASM/OPFS), Node/Electron, and native iOS/Android from one Rust core**, all first-class from day one, with CI on every platform. We meet developers where they are.

### Front 6 — **Tiered Conflict Resolution (LWW → CRDT-per-field → custom)**
> **Claim: *"Last-write-wins by default, conflict-free fields when you want them, custom merge when you need it."***

**Nostos's design is three tiers** (ADR-0014): (a) server-authoritative LWW per field — **shipped**, the default apply semantics (sane default, Postgres is source of truth); (b) **opt-in CRDT-per-field** for specific columns (counters, sets, rich-text — via Loro-style primitives — without bolting a whole CRDT doc onto your schema) — **deferred to Phase 4**; (c) **custom merge functions** for the hard cases — **deferred to Phase 4**. The right primitive per column, not a one-size hammer, once (b) and (c) land.

### Front 7 — **Transparent, predictable pricing**
> **Claim: *"No per-operation metering. Know your bill before you ship."***

**Nostos Cloud is base + flat-rate data + dirt-cheap per-op** (see §7). Self-host is **free and unlimited forever.** We compete on trust as much as tech.

### Front 8 — **Supabase-native, backend-pluggable**
> **Claim: *"Works with Supabase out of the box — and Postgres, Neon, CockroachDB, or any standard PG."***

First-class Supabase integration (Postgres + RLS + Auth wired), because that's where the demand is. But **backend-pluggable** (Neon, CockroachDB, any PG). MongoDB/MySQL source support on the roadmap, but **we lead with Postgres** — that's the white space.

---

## 4. The product — name, positioning, identity

### Name: **Nostos** *(primary recommendation)*
- **karn/** — a pile of stones marking a trail. When you're offline and off-grid, a cairn is how you find your way. **Sync checkpoints (LSNs) are our cairns** — durable markers that mean your data always finds its way home, across devices, through outages, back to the source of truth.
- Short (1 syllable), ownable domain space (`nostos.run` / `getnostos.io` / `nostossync.com`), pronounceable & spellable internationally, no crypto/AI taint, strong in tech trademark class 9.
- **Alternatives if Nostos is taken:** **Ply** (strands woven into one — sync merges streams), **Flint** (Rust-fast sparks), **Tideline** (the line sync draws across devices).

### Positioning (one sentence)
**"Nostos is the open, Rust-fast local-first sync engine — Postgres to every device, even offline, with no write-back endpoints and no license lock-in."**

### Identity pillars
- **Reliable** (the nostos metaphor: never lose your data, never give up on sync)
- **Fast** (Rust, end to end, auditable benchmarks)
- **Open** (Apache-2.0, the clean default)
- **Secure** (field-level encryption, RLS-aware, least-privilege predicates, audited auth)

---

## 5. Architecture

### 5.1 The shape — one Rust core, four thin SDKs, one Rust server

```
                          ┌──────────────────────────────────────────┐
   Postgres / Supabase ──▶│            nostos-server (Rust)            │
   (logical replication)  │  replicator · predicate engine · router   │
                          │   pgoutput via pgwire-replication         │
                          └───────────────┬───────────┬───────────────┘
                                WebSocket │           │ (WebTransport future)
                              (SSE read) │           │
                                          ▼           ▼
        ┌───────────────────────────────────────────────────────┐
        │                  nostos-core  (Rust crate)              │
        │  sync state machine · LWW + CRDT-field merge · schema  │
        │  cursor/checkpoint (LSN+offset) · dynamic predicates   │
        │              ┌──────────────────────────┐              │
        │              │   Storage trait (abstract) │              │
        │              └──────────────────────────┘              │
        └───┬─────────────┬───────────────┬───────────────┬───────┘
            │             │               │               │
         UniFFI         FRB           wasm-bindgen      napi-rs
        (iOS/Android/   (Flutter)      (Web/WASM/       (Node/
         React Native)                  OPFS)            Electron)
            │             │               │               │
         Kotlin/Swift   Dart pkg        npm pkg          npm pkg
         + op-sqlite    + sqlite3_       + sqlite-wasm    + better-sqlite3
           (RN)           flutter_libs    (OPFS)
```

### 5.2 The crate layer
| Crate / binary | Role | Stack |
|---|---|---|
| `nostos-core` | The platform-agnostic sync engine: state machine, conflict resolution, dynamic predicates, schema/cursor. **No I/O, no async runtime hard-coded** — pluggable. | pure Rust, `no_std`-friendly-ish |
| `nostos-storage-*` | Backends for the `Storage` trait: `rusqlite` (native), `sqlite-wasm` (web/OPFS), and adapters for `op-sqlite` (RN) + `sqlite3_flutter_libs` (Flutter). | Rust |
| `nostos-server` | The Rust sync server: PG logical-replication consumer, predicate engine, client router, metrics. | tokio + axum + `pgwire-replication` + `tokio-tungstenite` |
| `nostos-ffi-uniffi` | Kotlin/Swift/RN bindings (UniFFI) | UniFFI |
| `nostos-ffi-frb` | Flutter bindings (FRB v2, for first-class `Stream`) | flutter_rust_bridge |
| `nostos-ffi-wasm` | Web/Node bindings (wasm-bindgen / wasm-pack / napi-rs) | wasm-bindgen, napi-rs |
| `nostos-cli` | `nostos init / dev / deploy / benchmark` | Rust (clap) |

**Critical principle: the platform brings its own SQLite binary; Nostos brings the sync.** We don't ship one SQLite for all platforms (that's a lie at the binding layer). We ship one sync protocol + one `Storage` trait, and let each platform use the best native SQLite it already has (`op-sqlite` on RN, `sqlite3_flutter_libs` on Flutter, `sqlite-wasm`+OPFS on web, `rusqlite` native). This sidesteps the hardest cross-platform cliff.

### 5.3 FFI strategy — why four bridges, not one
There is **no single FFI bridge** that serves Flutter + RN + Web + Node well. The deciding factor is **streaming** (a sync engine continuously pushes change-feeds across the boundary):
- **Flutter → `flutter_rust_bridge` (FRB) v2:** first-class `Stream` support. Worth the per-platform cost — Flutter is our lead mobile SDK.
- **iOS/Android/RN → UniFFI:** Mozilla-backed, one IDL → Swift + Kotlin + RN Turbo Modules. Weak native `Stream` → we use a **callback-channel pattern** (register a listener; Rust pushes events into a bounded channel the platform drains).
- **Web → `wasm-bindgen` + `wasm-pack`:** runs in a Web Worker with **OPFS** for durable persistence; only real web option.
- **Node/Electron → `napi-rs`:** best-in-class, used by Rspack/SWC.

**The seam to manage:** getting the Rust core's `Send`/`Sync`/lifetime story to play nicely across tokio (server/Node), the JS event loop (web), Dart isolates (Flutter), and the RN bridge thread — without leaking platform complexity into `nostos-core`. De-risk: keep `nostos-core` **runtime-agnostic and `Send + Sync`**, push all platform threading into the thin FFI shims, and CI-test all four bridges on every commit.

### 5.4 Transport
- **Today: WebSocket** (bidirectional, universal) with **SSE option for the read-path** (CDN/proxy-friendly, auto-reconnect).
- **Protocol = transport-agnostic, length-prefixed framed messages** (so we can swap transports without touching the state machine).
- **Future: WebTransport/QUIC** upgrade path (`quinn`/`wtransport`) — multiplexed streams, 0-RTT reconnect, connection migration (huge for flaky mobile). Polyfill → WebSocket for the long tail.
- **Background push:** none of these wake a backgrounded app — FCM/APNs/Web Push as a wake-up trigger (the server nudges the client to reconnect).

---

## 6. The two technical moats, in depth

### 6.1 Predicate-based Reactive Sync — cursor-resumable, incremental

*Dynamic sync itself* is table-stakes now, not a Nostos-only feature. What still differentiates Nostos: **cursor-based, incremental** resume with no full-reprocessing cliff, and a predicate-evaluation engine purpose-built for it (ADR-0012).

**Nostos's model:**
1. The client opens a **sync session** authenticated with **parameters** (its `user_id`, `org_id`, roles).
2. The client subscribes with one or more **live predicates** — a small, safe subset of SQL scoped by the auth parameters: `SELECT * FROM tasks WHERE org_id = $org AND assignee_id = $user ORDER BY updated_at DESC` plus optional windowing/cursors.
3. The server maintains the set of *authenticated, live* predicates across all connected clients. As **logical-replication deltas** arrive, the server evaluates each changed row against *only the predicates whose parameter sets could match* (indexed by parameter → predicate), and streams matching deltas to the right clients.
4. State is **cursor-based** (LSN + per-stream op offset), so reconnects resume exactly where they left off — **no full reprocessing**.
5. As the user scrolls, the client **expands its predicate window** dynamically; the server streams more. **No fixed ceiling.** Complexity is **O(changed rows × matching predicates)**, not O(all buckets).

**Why it's a moat (narrowed, honestly):** the cursor-resumable, no-full-reprocessing property is the durable technical differentiator. The predicate-evaluation engine (ADR-0012) is the hard IP — shipped, benchmarked (~1.5M predicate-evals/sec eval-only through 10k predicates). We no longer claim "static buckets" as the wedge; we claim *incremental cursor resume + a purpose-built eval engine*.

**De-risk now:** prototype in month 1 — prove that evaluating thousands of concurrent authenticated predicates against a live PG stream doesn't degrade source-DB read performance (index the predicate lookup, never touch the source DB for evaluation).

### 6.2 Direct Write-Back — no endpoints to build

**Nostos's model — two modes:**
- **Direct mode (default):** you give Nostos a Postgres connection + declarative **write rules** (`table`, allowed `columns`, `auth_scope`, `merge: upsert|insert_only`, an `etag`/`version` column for optimistic concurrency). The client queues mutations; Nostos's server applies them to Postgres **inside a transaction** that re-checks the version/etag and applies your merge strategy. Conflict on the same row/column → your chosen strategy (LWW, CRDT-field, custom). Postgres remains the single source of truth.
- **Function mode:** for full control, you provide a function. Power users keep total control.

**Why it's a moat:** it removes the need to build and host an upload endpoint. Combined with Front 6 (tiered conflict resolution), Nostos can honestly say: *"point us at your Postgres; we'll handle offline reads AND writes."* That's the magic that wins demos.

---

## 7. Monetization — open core, managed cloud, enterprise

### 7.1 The model (the Supabase/Postgres play)
**Commoditize the engine; capture value through operations and trust.** Postgres is 100% free, yet Supabase/Neon/RDS/PlanetScale built enormous businesses operating it. We do the same for local-first sync.

- **Self-hosted (Apache-2.0): 100% free, forever, full-featured, unlimited.** Not crippled open-core. This is the land — and the moral high ground. We win adoption here.
- **Nostos Cloud (managed):** for teams that don't want to operate infra at scale. The convenience premium.
- **Enterprise:** for orgs that want self-host *plus* support, indemnification, SLAs, compliance, and advanced security.

### 7.2 The open-vs-managed boundary (intentionally generous)
**Everything functional is free in OSS:** the server, the predicate engine, direct write-back, all SDKs, LWW + CRDT-field conflict resolution. **The Cloud/Enterprise premium is purely operational & compliance**, never feature gates:
- *Cloud only:* managed hosting, autoscaling, dashboard, observability, multi-region routing, automated backups.
- *Enterprise only:* SSO/SAML, SSO audit log, SOC2/HIPAA artifacts, SLA + indemnification, on-prem/VPC-peering connect, field-level encryption key management, RBAC, dedicated tenancy.

This is the cleanest possible land-and-expand: **dev tries OSS locally (5-min setup) → ships to prod on free Cloud → grows → Pro → Enterprise.** No "open-core bait-and-switch" resentment.

### 7.3 Nostos Cloud pricing (transparent, predictable, dramatically cheaper)
| Tier | Price | Includes | Overages |
|---|---|---|---|
| **Hobby** | **Free** | 1 GB data synced/mo · 10,000 peak concurrent devices · 1 GB storage · community support | — |
| **Pro** | **$49/mo base** | 10 GB synced/mo · 50,000 peak devices · 10 GB storage · email support | **$0.50 per million sync ops** · **$0.10 / GB-month stored** · **$0.02 / GB egress** |
| **Scale** | **$499/mo base** | 100 GB synced/mo · 500k devices · priority support · multi-region | same overage rates, volume discounts kick in |
| **Enterprise** | **Custom** | unlimited · SSO/SAML · SOC2/HIPAA · SLA + indemnification · VPC/on-prem · dedicated | custom |

**The pitch:** Nostos's Free allows **10,000 peak devices**, and Pro is **base + flat data + $0.50/million ops** — know your bill before you ship. For a write-heavy B2B SaaS doing 100M sync ops/mo, Nostos Pro ≈ $49 + $50 = **~$99/mo**. That's a land-grab price; we win on volume, not margin-per-op.

### 7.4 Revenue streams (maturity ladder)
1. **Cloud subscriptions** (Pro/Scale/Enterprise) — primary, recurring.
2. **Enterprise self-host licenses** (support + indemnification + compliance) — large ACV, sales-led.
3. **Premium support tiers** (dedicated engineers, on-call) — high-margin.
4. *(Later)* **Nostos Studio** — a visual sync-rules/predicate designer + schema migration tooling (productized, seat-based).
5. *(Later)* **Nostos for AI agents** — durable offline state for on-device/edge agents (emerging TAM).

**Unit economics note:** the Rust server's low memory/CPU footprint is itself a margin advantage — our Cloud cost-to-serve is materially lower than a Node-based equivalent, so even at $0.50/million-ops we stay healthy.

---

## 8. Go-to-market — wedge, narrative, 12-month roadmap

### 8.1 The wedge
**Flutter + Expo/React Native developers building offline-first B2B SaaS and field-worker apps** — specifically the intersection of (a) the **Realm/Atlas-Device-Sync exodus** (MongoDB killed it) and (b) **Supabase users who hit the "no offline" wall**. These are pre-qualified, motivated, and currently have no clean-open option.

### 8.2 The narrative
> *"ElectricSQL gave up on offline writes. Zero is web-only and disabled offline writes. Supabase can't do offline (yet — they bought Triplit). **Nostos is the open, Rust-fast one that does it all — no write-back endpoints, no lock-in, free self-host.**"*

Launch beats: an **auditable public benchmark** (2.62M ops/sec aggregate fan-out, honest-units framing) and a **"migrate from Realm in 1 hour"** guide.

### 8.3 Channels
- **Show HN + r/Flutter + r/reactnative** at OSS launch (Apache-2.0 is the hook).
- **Supabase partnership** — become their officially-recommended offline-first layer. This is *the* distribution channel.
- **Content/SEO:** "offline-first Supabase," "Realm alternative," benchmark posts — capture high-intent search.
- **DevRel:** live-demo "point at Postgres, get offline on Flutter+Web in 5 minutes." The direct-write-back + no-buckets demo sells itself.
- **Design partners:** 5–10 B2B SaaS teams on free Enterprise in exchange for case studies.

### 8.4 12-month roadmap
| Phase | Months | Deliverables |
|---|---|---|
| **0. Spike & prove the moat** | 1 | PG logical-replication consumer in Rust (`pgwire-replication`); dynamic-predicate engine POC; **public, auditable fan-out benchmark.** |
| **1. Core + server MVP** | 2–3 | `nostos-core` (sync state machine, cursor checkpoints, LWW); `nostos-server` MVP (Rust); **Flutter SDK** (highest-value). Local dev loop works end-to-end. |
| **2. Multi-platform + Cloud alpha** | 4–5 | Web SDK (WASM/OPFS); React Native SDK; free Nostos Cloud alpha. |
| **3. OSS launch** | 6 | **Apache-2.0 release** on GitHub; Show HN + subreddits; "migrate from Realm" guide; Supabase partnership push. |
| **4. The DX moat ships** | 7–9 | **Direct write-back (no endpoints);** CRDT-per-field conflict resolution; **dynamic reactive sync GA** (bucket-less). Nostos Cloud GA + pricing live. |
| **5. Enterprise** | 10–12 | SSO/SAML, audit log, SOC2-in-progress, field-level encryption, RBAC, VPC/on-prem; first paid Enterprise pilots; case studies. |

> **Correction 2026-08-06:** Phase 0's original "≥5×" framing compared fan-out to another engine's replication-ingest rate (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.

---

## 9. Risks & de-risking

| # | Risk (what kills us) | Likelihood | De-risking |
|---|---|---|---|
| **1** | **The PG logical-replication state machine.** Stateful binary stream; LSN checkpoints, standby heartbeats/feedback, slot management, reconnect/reshard, WAL-bloat if consumer stalls, gap-filling on crash. | High | **Start here in week 1.** Build the durable-checkpoint/reconnect/failover story first; treat `pgwire-replication` as protocol-only and build the orchestration ourselves; chaos-test reconnects, crashes, slot loss. |
| **2** | **Threats — first-party offline from Supabase/hyperscalers.** **Supabase acquired Triplit (Oct 2025)** explicitly citing offline demand — a Supabase-native offline sync layer is the live, named risk. A hyperscaler shipping native Postgres→device sync would similarly make Nostos a feature. | Medium-High (elevated — Triplit deal is a concrete signal) | **Become Supabase's official partner before they ship their own** (the window is now narrower); Apache-2.0 means they're *welcome* to use us (we win adoption either way); move faster than a hyperscaler-acquired team can integrate; win on *server throughput + license + write-back DX* which a bolt-on acquisition won't match day-one. |
| **3** | **Memory/backpressure meltdowns** under tens of thousands of concurrent sync sessions — concurrent state machines + replication slots in Rust are leak/backpressure-prone. | Medium | Design backpressure into the core from day one; relentless load testing (10k+ concurrent clients); predicate engine must be **O(changed rows × matching predicates)**, never O(all predicates); per-connection memory budgets with hard eviction. |
| **4** | **WASM bundle-size rejection** — the web ecosystem is militant about bundle size; a 2 MB+ WASM core gets rejected for TTI. | Medium | **Hard size budget: <500 KB gzipped** for the web core; tree-shake aggressively; lazy-load the CRDT module; offer a **"lite" pure-TS read-path** for bundle-obsessed teams. |
| **5** | **The 4-bridge FFI maintenance tax** (UniFFI + FRB + napi-rs + wasm-bindgen), each with its own threading/runtime model. | Medium-High | First-class CI on all four from day one; keep `nostos-core` runtime-agnostic & `Send + Sync`; push all platform threading into thin FFI shims; the streaming seam (UniFFI's weak point) solved with a uniform callback-channel pattern. |

---

## 10. The 30-day validation sprint (what to do Monday)

1. **Week 1 — prove Front 3 (Rust throughput).** Stand up `nostos-server` reading PG logical replication via `pgwire-replication`; benchmark a pure fan-out to N WebSocket clients. **Goal: a public, auditable chart.** This chart funds everything.
   > **Correction 2026-08-06:** the N× framing compared fan-out to another engine's replication-ingest rate (unit mismatch) — retired; see benches/results/RESULTS.md §Correction.
2. **Week 2 — prove Front 1 (dynamic predicates).** Prototype the predicate-evaluation engine: feed a synthetic WAL stream, evaluate 10k concurrent authenticated predicates, measure source-DB impact and p99 latency. **Goal: prove no fixed cardinality ceiling and zero source-DB read cost.**
3. **Week 3 — prove Front 5 (multi-platform).** Get `nostos-core` (minimal) running through FRB on Flutter *and* wasm-bindgen on Web — the two hardest bridges. **Goal: one core, two platforms, one demo.**
4. **Week 4 — the demo + the post.** A 5-minute "point at Supabase Postgres → offline reads + writes on Flutter and Web, no buckets, no endpoints" demo. Ship the benchmark repo. **Goal: first 500 GitHub stars + 5 design-partner conversations.**

**Kill criterion:** if the predicate engine can't scale past 10k concurrent without degrading the source DB, **pivot the architecture before building the product on it.**

---

## 11. Sources (key URLs)

**Competitors:** [ElectricSQL writes](https://electric.ax/docs/sync/guides/writes) · [Zero](https://zero.rocicorp.dev/) · [Zero "when to use"](https://zero.rocicorp.dev/docs/when-to-use) · [Triplit](https://github.com/aspen-cloud/triplit) · [WatermelonDB](https://watermelondb.dev/) · [RxDB](https://rxdb.info/) · [Convex sync](https://www.convex.dev/sync) · [Couchbase Sync Gateway](https://www.couchbase.com/products/sync-gateway/) · [PouchDB conflicts](https://pouchdb.com/guides/conflicts.html) · [Dexie Cloud pricing](https://dexie.org/pricing) · [Supabase Realtime](https://supabase.com/blog/supabase-realtime-broadcast-and-presence-authorization) · [Sync engines compared 2025](https://merginit.com/blog/24082025-sync-engines-guide-electricsql-convex-zero)

**CRDTs:** [Loro 1.0](https://loro.dev/blog/v1.0) · [Loro perf](https://loro.dev/docs/performance) · [Automerge 2.0](https://automerge.org/blog/automerge-2/) · [Y-Sweet](https://github.com/jamsocket/y-sweet) · [Liveblocks](https://liveblocks.io/)

**Rust tech:** [pgwire-replication](https://crates.io/crates/pgwire-replication) · [pg_replicate](https://github.com/Mooncake-Labs/pg_replicate) · [sqlite-wasm](https://sqlite.org/wasm/doc/trunk/about.md) · [UniFFI for RN](https://hacks.mozilla.org/2024/12/introducing-uniffi-for-react-native-rust-powered-turbo-modules/) · [UniFFI futures](https://mozilla.github.io/uniffi-rs/0.28/futures.html) · [flutter_rust_bridge](https://pub.dev/packages/flutter_rust_bridge) · [napi-rs](https://crates.io/crates/napi) · [op-sqlite](https://github.com/OP-Engineering/op-sqlite) · [Turso/Limbo](https://turso.tech/blog/introducing-limbo-a-complete-rewrite-of-sqlite-in-rust) · [RxDB: transport comparison](https://rxdb.info/articles/websockets-sse-polling-webrtc-webtransport.html)

**Licensing:** [Sentry FSL](https://blog.sentry.io/introducing-the-functional-source-license-freedom-without-free-riding/) · [fsl.software](https://fsl.software/) · [FOSSA BSL](https://fossa.com/blog/business-source-license-requirements-provisions-history/)
