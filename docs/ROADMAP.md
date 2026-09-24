# Nostos Roadmap

> The plan to go from Week-1 spike to v1.0 open-source launch. Each phase has a **single headline deliverable** and a **kill criterion.**

---

## Phase 0 — Spike & prove the moat  *(Week 1 — this repo, today)*

**Headline:** An auditable benchmark measuring how fast Nostos's Rust server fans replication events out to 1k/5k/10k concurrent WebSocket clients.

**Deliverables:**
- Hexagonal server skeleton: `nostos-domain` / `nostos-application` / `nostos-infra` / `nostos-server`.
- `FakeReplicator` → `FanOutService` → bounded per-session sinks → WebSocket transport.
- `nostos-bench` harness: in-process WS client swarm, measures sustained ops/sec + drop rate + p99. *(C3 — batched WS writes shipped: the per-session write task now drains up to 64 immediately-available frames into one JSON-array WS message under backlog, while sending a single object — byte-identical to the legacy wire — when only one frame is pending (zero latency tax at low rates). Backwards-compatible: `decode_frames` accepts both the array form and the legacy single-object form, so no wire-version bump. **Measured on Apple Silicon / 10 cores / rustc 1.95.0** — 1k headline: 833k → 833k ops/sec @ 0% drops (within noise, no regression); 5k: 592k → 660k ops/sec, drops 0.00% → 0.91%; 10k (probe — the full harness's `FanOutService::run` is O(N×E) via per-event full-store ack/eviction scans and hangs in teardown at 10k, so a lean `nostos-bench-10k` shim measures it): ~406k → ~483k ops/sec, drops ~67.5% → ~61.4%. **The 10k <1%-drop goal was NOT met** — batching is a strict improvement at every tier but the dominant 10k cost is the per-event store scan, not the per-connection WS write path; the named follow-up is the table-sharded router. ws_contract 8/8 green. Reconnect-storm probe (`nostos-reconnect-storm`): dropping+reconnecting 1k–2k of 2k–3k clients mid-stream drains cleanly — post-storm drop rate 0.00% across runs (pre-storm 0–14% reflects steady-state noise); **admission control / token-bucket NOT needed** and not built speculatively.)* *(2026-09-02 — 10k root cause found and fixed: the fan-out loop spawned one tokio task per session per event and nothing was being shed (`docs/plans/soak-10k-root-cause-2026-09-02.md`). Sequential fan-out + one shared `Arc<ReplicationEvent>` per event. **10k <1%-drop goal MET on Linux (Docker, 10 vCPU): 50M/50M delivered, 0.00% drops, 854,631 ops/sec, all 5000 events inside the 60 s window** vs 28% undelivered on the previous build — RESULTS.md "10k soak on Linux". On macOS the fixed build connects fast enough to hit the host's `ENOBUFS` limit at ~9.2k loopback sockets, so 10k is measured in a container only. **Native 1k headline re-run on the fixed build (2026-09-02, 3 passes, same config/host): 2,682,508 / 2,618,601 / 2,515,049 ops/sec, 0.00% drops, 100M/100M delivered in 37–40 s — median 2,618,601, 3.14× the old 833,305; headline updated.** macOS 10k soak stayed inconsistent (64.73% drops cold at load 10.7, then 0.00% / 2.13M ops/sec 90 s later) — not claimed; Linux container remains the 10k figure.)*
- `RESULTS.md` with the throughput chart.

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
- Predicate expression engine (boolean tree of equalities/ranges over auth-scoped params). *(ADR-0012 — moat complete: boolean tree `And|Or|Not` + typed comparison `Lt|Gt|Le|Ge` over `Number/Float/Bool/Text`, proven against real PG rows via the JSON column extractor. **Baseline:** ~150-170 eval-only events/sec through 10k predicates (~1.5M predicate-evals/sec). An equality index was built, measured a 4-8× regression, and **reverted** — the eval loop is structurally the cost but not the binding constraint; index deferred until a real load shows it binding.)*
- **Native reactive-scroll example** — `cargo run -p nostos-client --example reactive_scroll` makes the moat visible: in-process server + durable SQLite client + typed predicate + mid-stream server restart with zero-loss resume. The native path's `chaos_resume` property, demonstrated end-to-end. *(First visible demo; Flutter/Web product surface still to come.)*
- `nostos-core` WebAssembly build (`wasm-bindgen` + OPFS storage). *(✅ in-memory apply bridge shipped ADR-0015; OPFS persistence deferred — Worker-only by spec.)*
- Flutter SDK via `flutter_rust_bridge` (first-class `Stream`). *(✅ **shipped** — `nostos_flutter`
  with `NostosDatabase.watch` returning a hot, replay-shared `Stream`, a typed `Collection<T>`
  facade (ADR-0024) and `SyncStatus` (ADR-0027); proven by the `flutter` `sdk-e2e` slice against a
  real server. This line read "ADR-0015 — deferred" until 2026-07-30.)*
- The first end-to-end demo: "point at Supabase Postgres → offline reads on Flutter + Web." *(gates on OPFS + transport + Flutter.)*

---

## Phase 3 — OSS launch  *(Week 6)*

**Headline:** Apache-2.0 v0.1 on GitHub. Show HN. "Migrate from Realm" post.

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

Today: **Phase 3 🚧 — v0.1 prepared, launch gated on operator.** v0.1 scope is code-complete: real-PG default + snapshot, `where_sql` predicate subscriptions, WS batching, write-back v1 with offline outbox, WASM transport + `/demo` page, two Flutter fixtures, stranger-tested README quickstart. RESULTS.md carries the honest 1k/5k/10k picture (**1k headline refreshed 2026-07: 833k ops/sec aggregate fan-out @ 0% drops** — the Week-1 baseline of 142k ops/sec is preserved as historical. 10k drop ceiling diagnosed — table-sharded router is the Phase 2 fix). Router status 2026-08-24: PARKED — full-path single-client evidence shows drain is not scan-bound; revisit on the first multi-client real-PG run that shows the O(N×E) scan binding. **Update 2026-09-21:** the gated 100k/50k discriminating run confirms the cliff and falsifies kernel TCP pressure, swap and CPU saturation — the host sits 42% idle while throughput collapses 11.4×. A first attribution to the single-threaded fan-out loop was then **retracted the same day**: `nostos-fanout-walk` measures that loop linear from 10k to 100k (0.458 → 0.486 µs/delivery) and worth ~1.4% of the observed per-event time. The walk was parallelised anyway (2× with real sinks, `PARALLEL_FANOUT_MIN` = 8,192) but the cliff is still unexplained; the remaining suspects are the per-session wire encode + socket write, the kernel socket path at 100k connections, and the 100k in-process client tasks sharing the server's runtime. **Cliff located 2026-09-21 (Phase 5+6, `docs/plans/fanout-100k-cliff-diagnosis.md`): it is not the server.** `nostos-fanout-walk` gained a `sink=wire` mode that runs the real `encode_event` in each session's drain task — the encode every previous walk number omitted, because `TokioEventSink::deliver` only moves an `Arc`. With it, the entire server side is **linear** from 10k to 100k sessions: 0.200 → 0.213 → 0.222 µs per delivery (+11% over a 10× scale-up), i.e. 22 ms/event and 4.5M deliveries/sec at 100k. The gated container ladder then re-ran valid (`LINUX_DIAG_EXIT=0`, 50k reproduces its 0.158 s/event baseline at 0.139): 0.019 / 0.139 / 3.896 s per event. Against the spine's measured 15.0 / 22.2 / **287** µs of CPU per delivery, the fan-out server is **1.3% / 1.0% / 0.08%** of the cost — no change inside `FanOutService` or `InMemorySessionStore` can address more than ~1% of the cliff, and **the table-sharded router parked below would not have fixed it either**. Two retractions: the shared-store-mutex hypothesis is **falsified** (`min_acked_lsn` is 0.70 ms/event at 100k, not 1,814 ms), and the in-run `stage_*` timers measure scheduler starvation rather than work, since `Instant::elapsed()` spans `.await`. Memory, swap and kernel TCP pressure are falsified again with numbers (`SwapFree` flat, `psi_mem avg60 = 0.00`, and zero deltas on PruneCalled / RcvCollapsed / MemoryPressures / ZeroWindowDrop / BacklogDrop across 200,026 sockets), and `cores_used` *falls* to 7.37/10 at the cliff. What remains unexplained is 287 µs of real CPU per delivery at 100k — superlinear, outside the fan-out loop, with 2.6 cores idle. Remaining suspects: the loopback socket path at 200k sockets, and the probe's own 100k in-process client tasks sharing the server's runtime (i.e. partly a harness-topology artifact). **Do not cite the 100k tier as a Nostos server limit.** **A fix appeared to land and was retracted the same day:** `FanOutService::run` recomputes the slowest acked LSN every `ack_progress_every` events, shipped at `1`. A gated dose-response at 100k looked monotonic — 32,372 → 60,387 → 72,051 ops/sec at N = 1 → 16 → 100 — so the default was changed to **16**. Six interleaved replication tiers then measured the within-arm spread: `ack=1` 39,847 mean over 7,146–57,090 (**7.99×**), `ack=16` 43,983 over 21,936–74,711 (3.41×), sd ≈ 70% of the mean in both arms, mean ratio 1.10×, **arms fully overlapping**. The dose-response had sampled each configuration once and read run-to-run drift as signal. **`NOSTOS_ACK_PROGRESS_INTERVAL` is reverted to `1`** and Phase 5's falsification is reinstated: the isolated walk was right that the fold is ~3% of the cliff. The knob does shrink its own stage (`ack_scan` ~8×, cleanly separated in all six tiers) — that stage just does not bound throughput here. **The standing lesson: n=1 per arm is not a measurement, and a true mechanism is not an effect — "stage X got 8× cheaper" and "throughput rose" are separate claims needing separate evidence.** Harness defect exposed and fixed the same day: `fanout-100k-diag.sh` enforced the load gate only at tier start, so tiers passed at load1 < 8 and then ran at 34.21, and `LINUX_DIAG_EXIT=0` was hardcoded so a contended run still announced itself clean. It now samples the host every 10 s, ends each tier with a `MIDRUN samples=/violations=/load1_max=/valid=` verdict against the mid-run rule (newly canonical in `docs/BENCHMARK-METHODOLOGY.md` § 6.1, previously buried in a plan doc the script did not cite, and recalibrated the same day from "no single non-harness process > 20% CPU" — 2% of a 10-core host, unmeetable with a display attached — to aggregate non-harness CPU <= 150% on >= 95% of samples, derived from the VM's measured 740% peak and pinned by a regression test that still rejects the 2026-09-02 contaminated run), kills a contended tier after 3 consecutive violating samples with one re-arm, and exits with the worst tier rc. The six replication tiers predate the fix and remain order-of-magnitude only. **Drop-rate ladder 2026-09-21 (`benches/results/raw/2026-09-21-droprate-ladder/`):** neither six-tier ack run had a single tier under the methodology's <1% drop bar (7.9–95.7%), so neither measured throughput at all — the harness now stamps `drop_pct`/`throughput_valid` per tier. Laddering 10k→100k puts the last reproducibly-clean rung at **50,000 clients** (0.00%/0.12% over two passes); 55k and 60k each clear 1% only half the time over four passes each (0.00–1.70% and 0.00–4.20%) and cannot be separated, so there is no sharp ceiling, and the single-pass version of that ladder produced a crisp 55k/60k boundary that replication dissolved. Aggregate throughput is flat at 430k–675k deliveries/sec across 10k–80k — completeness degrades with client count, rate does not — and 100k is the only rung that collapses (123,982/sec, 25.6% drops, the sole window expiry). **Proposed answer 2026-09-22 (ADR-0045, `docs/adr/0045-per-key-conflating-session-sinks.md`):** the live plane carries state, not operations — `tuple_to_json_payload` (`replicator/pg.rs:1182`) emits a complete row image rather than a delta, the client's apply already discards a lower-LSN frame for the same `(table, pk)` (`nostos-client/src/sqlite.rs:649`), and ADR-0030's addendum records that the HLC merge is redundant for server-delivered frames because the server serializes. So a per-key **conflating** sink — move-to-tail, which preserves the ascending-LSN order ADR-0009's per-socket checkpoint depends on — leaves the client's SQLite byte-identical while bounding pending work by distinct rows instead of by event rate. `SinkMsg::Control` bypasses conflation untouched; overflow becomes ADR-0040's `resync_required`, which must move from opt-in (`NOSTOS_RESYNC_SIGNAL`, today default off) to default-on, since it is the one genuinely lossy case. Superseded frames get their own counter — folding them into `Metrics.dropped` would inflate the figure the project uses as its honesty surface. **Built and reverted the same day**: six tests pin the invariants (supersede, move-to-tail LSN ordering, shedding at capacity, no supersede across a control frame) and `make ci` is clean, but the A/B at 1k clients — same machine, same session, 3 runs per arm — measured **2,520,979 → 1,532,279 ops/sec median, −39.2%** (+256 ns per delivery) at 0.00% drops in both arms, against ADR-0030 D7's 3% revert threshold. The cost is the implementation, not the design: every delivery now takes a mutex, a `BTreeMap` insert and a hashed `HashMap` insert in place of a lock-free `mpsc::try_send`, and at zero backlog that work buys nothing. The fix is to conflate **only under backlog** (keep `try_send` as the fast path, divert to the conflating queue on `Full`), which is also the only regime where conflation has anything to do. Work preserved on the local branch `adr-0045-conflating-sink` (`6044dbc`); `NOSTOS_RESYNC_SIGNAL` stays default-off until it ships with that hybrid. **Attempt 2 the same day** keeps `mpsc::try_send` as the fast path and treats `Err(Full)` as the backlog signal, so the success path gains nothing at all; only the full branch folds into a conflating overflow. That measured **2,520,979 → 2,660,070 ops/sec median** over 3 runs per arm with overlapping ranges — **no measurable change**, 0.00% drops, full 100M deliveries. It is still not merged, for a reason worth more than the code: `nostos-bench` computes `drop_rate = 1 - delivered/(events x clients)` from the **client-side frame count**, and conflation exists to send fewer frames for the same state — so the harness scores every supersede as a loss. Merging would make `drop%`, the number README and RESULTS.md quote as the honesty surface, silently wrong. Two harness facts fell out: `nostos-bench` hardcoded `distinct_keys: 0` (every event a brand-new row, zero conflation opportunity by construction; a `--distinct-keys` flag was added, default 0 so history is unaffected), and the 10k rung on this host reports `elapsed_secs: 120.00` on every run — its drops are window expiry, with baseline at 200 keys swinging 27.11% then 1.19%. A first build of attempt 2 also lost 86.6% of traffic because `recv()` discarded the message it had just awaited; all six conflation unit tests stayed green through it and only the benchmark caught it. **Attempt 3 the same day takes that remaining step and the metric blocker is gone:** `DeliveryDecision::Superseded` is threaded `TokioEventSink::deliver` -> `deliver_chunk` -> `FanOutOutcome.superseded` -> `Metrics`/`MetricsSnapshot` -> `cairn_events_superseded_total` -> `nostos-bench`, which now computes `drop = attempted - delivered - superseded` and prints a `superseded` column. The sink returns `Superseded` for the event that REPLACES a waiting frame, not for the one replaced, which is what closes the arithmetic: n updates to a backlogged row give one `Delivered` plus n-1 `Superseded` and exactly one frame on the wire, so `delivered` equals frames sent and the contract becomes `delivered + dropped + superseded + faulted <= matched`. One run at 1k x 20,000 events with `--buffer 16 --distinct-keys 64` (a small buffer manufactures the backlog 1k clients never produce at the default) shows the fix with no run-to-run noise between the two figures, because both come from the same run: **14.18% under the old formula, 12.28% under the new one** on 17,163,573 delivered + 380,637 superseded out of 20,000,000 attempted. That 1.90 pp is exactly the conflation the old metric scored as loss; it is not a throughput result and must not be quoted as one. Non-regression at the headline shape (1k x 100,000, default buffer, 0 recycled keys), interleaved arms, 3 runs each: parent **1,645,329** vs **1,715,366** median with ranges overlapping heavily, i.e. **no measurable change**, 0.00% drops and 100M deliveries throughout. **A methodology fact fell out of that check and it outranks the result:** attempt 3's first three runs read 1.74M/1.90M/2.03M against the 2.52M/2.66M medians recorded for attempts 1-2 *earlier the same day*, which looks like a 30% regression — but re-measuring the PARENT commit in the same session put it at 1,645,329, below attempt 3. Both arms decline run-over-run and both sit far under their own earlier figures, so the drift is the host. **Cross-session absolute ops/sec are not comparable on this box; only arms interleaved within one session are.** Same shape as the `ack_progress` retraction, one level up: n=1 per *session* is not a baseline. **Still not merged, and now for a different reason than before:** the metric is honest, but ADR-0045's own binding gate reads "ship only if 50k stays sub-1% and at least one rung above 50k moves from over-1% to under-1%", and that ladder cannot be produced honestly on this host (the 10k rung reports `elapsed_secs: 120.00` every run). The conflation *benefit* therefore remains unmeasured at the tiers where backlog naturally occurs. ADR-0045 stays `proposed`; work on branch `adr-0045-conflating-sink` (`f83503a`). The 100k question stays unanswerable until the load generator leaves the server's host. Still eval-only (FakeReplicator loopback), so the real-PG trigger is unmet; see RESULTS.md § "The fan-out walk is linear". Push: ADR-0037 sync-aware push piloted in atlet on real APNs/FCM rails (plan 24/24); ADR-0038 nostos-pushd daemon + RemoteNotifier delegation implemented per `docs/plans/nostos-push-daemon-implementation.md`. Launch post drafts in `docs/launch/`, local `v0.1.0` tag; **publication, the RN SDK, Nostos Cloud alpha, and Show HN timing remain operator calls** (see `docs/plans/complete-nostos-fully-wired-operational.md` Phase F2).
