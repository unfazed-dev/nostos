# Plan — Shared full-path benchmark harness: Nostos vs PowerSync

Status: proposed. Supersedes the "deferred" position in [`docs/COMPARISON.md`](../COMPARISON.md) §2 (line 44) and §4 (line 83).

## 0. Correction to the stated premise

The task brief says Nostos's comparable full-path number is "~35k rows/sec at 1k clients." It is not.
[`benches/results/RESULTS.md`](../../benches/results/RESULTS.md) line 195 records the environment for
the 36,230 / 34,772 rows/sec runs as **"single client, loopback."** The full-path apply number has
never been measured above one client. Every plan below is built on that corrected fact: the first
honest comparison point is a **1-client apply tier**, and multi-client apply is new measurement work,
not a re-run.

## 1. Goal and non-goals

**Goal.** One harness that runs Nostos and PowerSync back-to-back on the same machine, same Postgres,
same row writer, same table shape, same client count, same finish line, same accounting — reporting
rows/sec-to-client-apply, p50/p99 apply latency, and drops for each.

**Non-goals.**
- No comparison against Nostos's eval-only fan-out ladder (FakeReplicator loopback, 988k ops/sec @ 20k).
  That stage has no WAL decode and no client apply. It is not a PowerSync comparator, ever.
- No cross-stage ratios. PowerSync's published 2–4k ops/sec is *source-DB → Service replication*;
  its 2–20k ops/sec is *Service → client sync*
  ([performance-and-limits](https://docs.powersync.com/resources/performance-and-limits)). Same-stage,
  same-units only, per [`docs/BENCHMARK-METHODOLOGY.md`](../BENCHMARK-METHODOLOGY.md) §8 (line 112).
- No write-path (CRUD upload) measurement. Read-only race. PowerSync's upload queue needs a
  customer-built `uploadData()` endpoint
  ([client-architecture](https://docs.powersync.com/architecture/client-architecture)); including it
  would measure our backend, not theirs.
- No cross-machine, no WAN.

## 2. Same-stage definition

| Stage | Nostos | PowerSync |
|---|---|---|
| 1. Row committed | `INSERT` into shared PG, `commit_ts` column via `clock_timestamp()` | identical — same statement, same table |
| 2. Replication decode | `PgReplicator` / `pgoutput`, own slot | Service replication worker, own slot |
| 3. Server materialize | `FanOutService` predicate eval per session | bucket storage write + checkpoint assignment |
| 4. Wire | loopback WebSocket, JSON frame | HTTP stream or WebSocket sync line |
| 5. Local apply | `Storage::apply_batch` — one txn, upsert rows, advance checkpoint, `tx.commit()` (`crates/nostos-client/src/sqlite.rs:22-24`, commit at `:860`) | checkpoint downloads fully, then `ps_oplog` → `ps_data__<table>`, visible through the `<table>` view |

**Where the finish-line timestamp is taken.** On both sides: at the client's normal read path, after
the row is durably readable, **event-driven on both sides — never a poll.** Nostos — immediately after
`apply_batch` returns for the batch containing row *N*. PowerSync — inside a watch-query callback over
the `<table>` view (the SQLite view over `ps_data__<table>`), never `ps_oplog`. The Node SDK advertises
"query subscriptions that automatically push real-time updates"
([node SDK](https://docs.powersync.com/client-sdks/reference/node)) and the architecture page names
Live/Watch Queries ([client-architecture](https://docs.powersync.com/architecture/client-architecture)).
A poll would quantize every PowerSync latency sample by the poll interval and inflate its p50/p99 for a
reason unrelated to PowerSync, while Nostos's side stayed event-driven. If a watch proves unusable in
Step 3, fall back to a poll and **state the interval as a declared floor on every reported latency.**
Latency = `now() - commit_ts` carried in the row itself, corrected for PG↔host clock skew the way
`bench_pg_ingest.rs` already does (`measure_skew`, line 547).

**PowerSync's apply is checkpoint-quantized by design.** Rows land in `ps_data__` only once a full
checkpoint has downloaded ([protocol](https://docs.powersync.com/architecture/powersync-protocol):
checkpoint available → data → checkpoint complete). Its latency curve will be batchy. That is a real
property of the system and gets reported as measured, not smoothed or excused.

## 3. Harness design

### 3.1 Shared row writer

New bin `crates/nostos-bench/src/bin/shared_writer.rs`. Engine-agnostic: it only writes to Postgres.

- Rate-limited with the same pacing loop as `bench_pg_ingest.rs` (lines 479-505), batch size
  configurable, observed rate always reported.
- Table `race_rows`, created empty, published to both engines:
  `id TEXT PRIMARY KEY, seq BIGINT, payload TEXT, commit_ts TIMESTAMPTZ DEFAULT clock_timestamp()`,
  `REPLICA IDENTITY FULL`.
- **`id` must be `TEXT`, not `UUID`.** PowerSync requires a single text-type primary key named `id`
  ([client-id](https://docs.powersync.com/sync/advanced/client-id)). The existing `tasks` table uses
  `UUID PRIMARY KEY` (`docker/pg-init/01-sources.sql:11`), so it cannot be the race table without
  handing PowerSync a type-mapping cost Nostos does not pay.
- Empty at subscribe time on both sides — the trick from `e2e_pg_apply_throughput.rs` (line 125-149),
  so the first-connect snapshot is zero rows and the window is pure live path.

### 3.2 Nostos client swarm

Reuse `SyncClient` + `SqliteStorage` unchanged. No lighter apply — a lighter apply would be Nostos
grading its own homework. Session buffer stated per run (the 32768 vs 1024 finding at
`RESULTS.md:203-210` shows this knob dominates drops).

### 3.3 PowerSync client swarm — decision

**Use the official `@powersync/node` SDK for the apply finish line. Use the official
`test-client concurrent-connections` for the wire-delivery lane. Do not hand-write a Rust client.**

Rationale, from the docs:

1. The protocol page states the line-level format is not normatively specified: *"For details, see the
   implementation in the various PowerSync Client SDKs"*
   ([protocol](https://docs.powersync.com/architecture/powersync-protocol)). It documents the message
   *sequence* (checkpoint → data → checkpoint complete) but not the JSON shapes. A hand-written parser
   is therefore reverse-engineered from FSL-licensed source, and is the first thing a reader attacks.
2. `powersync-sqlite-core` (the Rust SQLite extension that does the real apply) is ruled out by its own
   README: *"The APIs here not currently stable, and may change in any release. The APIs are intended
   to be used by PowerSync SDKs only."*
   ([repo](https://github.com/powersync-ja/powersync-sqlite-core))
3. PowerSync ships an official load-testing tool. `test-client` has a documented
   `concurrent-connections` command: *"simulate concurrent connections to a PowerSync instance. This
   can be used for performance benchmarking and other load-testing use cases"*, `http` or `websocket`
   mode, `-n` connections, printing `op_id`, `ops`, `bytes`, `duration` per connection
   ([test-client README](https://github.com/powersync-ja/powersync-service/tree/main/test-client)).
   **It reports ops received, not rows applied** — so it can carry the wire-delivery lane and never the
   apply headline.
4. `@powersync/node` is Beta but "production-ready for tested use cases"
   ([node SDK](https://docs.powersync.com/client-sdks/reference/node)). It gives a genuine
   `ps_data__race_rows` apply, which is the only true counterpart to `SqliteStorage::apply_batch`.

### 3.4 Two ladders that never cross

One `PowerSyncDatabase` per client means a SQLite file plus a background worker each. A thousand of
those on one host, alongside Postgres, the PowerSync Service, and Nostos, is not a real measurement.

| Lane | Clients | Nostos side | PowerSync side | Reported metric |
|---|---|---|---|---|
| **Apply** (headline) | 1 / 10 / 100 | `SyncClient` + `SqliteStorage` | `@powersync/node`, N processes | rows/sec to local apply, p50/p99, drops |
| **Wire delivery** | 1k / 5k | existing delivered-frames counter (`crates/nostos-bench/src/main.rs:303`) | `test-client concurrent-connections -n` | ops/sec to client wire, drops |

No number from one lane is ever compared to a number from the other. Each results table states its
lane in its own heading.

**Deviation from the brief, flagged for the team lead's call.** The brief specified one 100 / 1k / 5k
ladder on the apply finish line. That is not runnable: 1k `PowerSyncDatabase` instances on one host is
not a measurement, and PowerSync's own docs cap an API container at 200 connections and recommend one
container per 100 (§5.1) — so a fair 1k apply tier needs ~10 API containers plus 1k Node processes plus
1k Nostos clients plus two Postgres instances on one machine. The **5k wire tier is likely infeasible**
on a single host for the same reason (~50 API containers) and should be dropped to 2k unless the
headroom check in §8.2 says otherwise. Scaling the requested ladder down is the team lead's decision,
not ours; this is the recommendation with its reason.

## 4. Accounting

- **attempted** = rows committed × clients **subscribed at commit time**. The ladder already learned
  this: `probe_10k.rs` waits for a subscribe quorum (lines 165-185) precisely because charging events
  to not-yet-subscribed clients silently inflated the drop rate. "Subscribed" means: Nostos — subscribe
  frame acked; PowerSync — first `checkpoint_complete` received.
- **delivered** = rows readable through the client's normal read path (§2).
- **dropped** = attempted − delivered, attributed. Nostos reports router `dropped` separately from
  never-connected and not-reached-in-window, as `probe_10k.rs` already does (lines 225-246). PowerSync
  has no drop-on-full equivalent; its shed shows up as latency and as a truncated window. Report
  "0 drops, N rows outstanding at window close" rather than implying a shed that did not happen.
- **apply latency** = `finish_ts − commit_ts`, skew-corrected. Percentiles via `stats.rs::Histogram`
  for Nostos; the Node harness emits raw samples to JSON and reuses the same percentile code so both
  sides use nearest-rank.
- **warm-up exclusion** = first 10% of rows discarded on both sides, count stated. Both engines get a
  settle wait before the window opens.

## 5. Fairness controls

- Same machine, same run session, **interleaved A/B order** (Nostos, PowerSync, PowerSync, Nostos) so
  thermal drift cannot favour whoever ran first.
- **Same run length**: identical fixed row count *and* identical wall-clock window cap per tier, stated
  in the results table. A tier that fails to drain inside the window is reported as "X of N delivered in
  W s", never converted to a rate — the rule `probe_10k.rs` already enforces (line 249-254).
- Host-noise guard, per `RESULTS.md:213-217`: record load average before and after, VS Code closed, and
  discard any run where a competing process exceeds the threshold that already invalidated two runs.
- Same Postgres instance as replication *source* for both.

### 5.1 PowerSync deployed per its own production guidance

Every knob below is taken from
[deployment-architecture](https://docs.powersync.com/maintenance-ops/self-hosting/deployment-architecture)
and [production-readiness-guide](https://docs.powersync.com/maintenance-ops/production-readiness-guide).
The current one-container stack in `docker/docker-compose.powersync.yml` is the docs' *minimal
development* setup and must not carry a published number.

- **Split the roles.** Production is "1x PowerSync replication container" (`start -r sync`) plus
  "2+ PowerSync API containers" (`start -r api`), not one all-in-one process. Only one replication
  process may run at a time (`PSYNC_S1003` otherwise).
- **Scale API containers to the client count.** This is the hard one: *"Each API container is limited to
  200 concurrent connections, but we recommend targeting 100 concurrent connections or less per
  container"*, and *"add 1x PowerSync API container per 100 connections."* Racing 1k clients against one
  container is a crippled default that the docs explicitly forbid. Budget: 1k clients → ~10 API
  containers; 5k → ~50. See §3.4 — this likely makes the 5k tier infeasible on one host.
- **Sizing.** Replication container 1GB / 1 vCPU; each API container 1GB / 1 vCPU; both raised to
  2GB / 2 vCPU for "larger rows and higher load." Record what was allocated.
- **`NODE_OPTIONS=--max-old-space-size-percentage=80`** on every container, as the docs prescribe, so
  the Node heap actually uses the memory we gave it.
- **Run a compact job** before each measured window (the docs recommend daily, "or after any large
  maintenance jobs"), so PowerSync is not measured against an un-compacted bucket store.
- **Source Postgres tuning.** Set `max_slot_wal_keep_size` deliberately and monitor slot lag against it,
  per the production-readiness guide — with two slots and a saturating writer, a slot invalidated
  mid-run silently ends the measurement.
- **Bucket storage backend.** See §8.3 — the docs' recommended setup names MongoDB, so Postgres storage
  is provisionally the wrong choice for a fair race.
- **Separate Postgres instance for PowerSync bucket storage.** The docs state the bucket storage
  database is separate from the source database
  ([self-hosted config](https://docs.powersync.com/configuration/powersync-service/self-hosted-instances)).
  `docker/powersync/config.yaml:27` currently points bucket storage at the *same* instance that will be
  absorbing the writer load and Nostos's slot. Leaving it there rigs the race against PowerSync.
- **Rewrite the config to the documented schema.** The current file does not match it. Ours has
  `authentication.dev_mode` (line 17), `sources[].database.connection_string` (line 21), `sync_rules.path`
  (line 30), `client_sync.max_params` (line 34). The documented shape is `replication.connections[]`,
  `storage:`, `client_auth:`, `sync_config.path`, `port:`.
- **Pin the image tag.** `docker/docker-compose.powersync.yml:25` uses `journeyapps/powersync-service:latest`.
  A benchmark must name the version it measured.
- **Real auth, not dev mode.** Swap `dev_mode` for `client_auth.jwks` static keys plus
  `test-client generate-token`
  ([development-tokens](https://docs.powersync.com/configuration/auth/development-tokens)), so JWT
  verification cost is on PowerSync's clock the way it would be in production.
- **Disable telemetry sharing** (`telemetry.disable_telemetry_sharing: true`) so no background reporting
  runs inside the window.
- Sync rules kept minimal and equivalent on both sides: one bucket over `race_rows`, no parameters, so
  neither engine pays partitioning cost the other avoids.

## 6. Deliverables, effort, stop line

| Step | Deliverable | Days |
|---|---|---|
| 0 | Rewrite `docker/powersync/config.yaml` to the documented schema; pin image; **MongoDB bucket storage** (single-node replica set, per the deployment-architecture doc — decision D6, §9); JWKS auth; extend `powersync_smoke.rs` to assert a real `checkpoint_complete` | 2 |
| 0.5 | **Per-container ceiling probe** (moved up from §8 Q1 — decision D5): one API container, ramp `@powersync/node` clients 25→50→100→150 until the first connect failure or checkpoint stall; the measured ceiling sets the containers-per-tier ratio for Steps 3 and 5 | 0.5 |
| 1 | `crates/nostos-bench/src/bin/shared_writer.rs` + `race_rows` DDL in `docker/pg-init/` | 1.5 |
| 2 | Nostos apply swarm: `crates/nostos-bench/src/bin/race_nostos.rs` (N × `SyncClient`, per-client histogram, JSON artifact) | 2 |
| 3 | PowerSync apply swarm: `benches/powersync-node/` (Node harness, **N clients packed per process via `worker_threads`**, client-side CPU sampled per engine — decision D7, same JSON artifact schema) | 3 |
| 4 | Wire-delivery lane: `test-client concurrent-connections` wrapper + Nostos counterpart | 1.5 |
| 5 | Orchestrator `benches/scripts/race.sh` (interleaved A/B, **headroom rule D4 enforced mechanically**, env capture) | 1 |
| 6 | `benches/results/race-<date>/RESULTS.md`; rewrite `docs/COMPARISON.md` §2/§4; update `docs/BENCHMARK-METHODOLOGY.md` §8 | 1.5 |

Total ≈ 13 days. **Status: HOLD — planning only** (decision D2, §9). Nothing in this table executes
until the unblock condition in D2 is met and the team lead says go.

**Stop line.** Whatever the harness measures gets published, including a loss. Concretely: if PowerSync
wins any apply tier, the amended artifacts are named in advance — the labeled-number table in
[`docs/COMPARISON.md`](../COMPARISON.md) §1 (line 25) and the headline paragraph in
[`CLAUDE.md`](../../CLAUDE.md). A run is discarded only for a stated environmental reason (host
contention, a crashed service), the discard is committed with its logs the way
`benches/results/remeasure-2026-09-02/CONTENDED.md` was, and a discard is never a silent re-roll.

## 7. Licensing note (our reading, not legal advice)

The PowerSync Service is FSL-1.1-ALv2
([LICENSE](https://github.com/powersync-ja/powersync-service/blob/main/LICENSE)). The governing clause:

> **Permitted Purpose**
> A Permitted Purpose is any purpose other than a Competing Use. A Competing Use means making the
> Software available to others in a commercial product or service that:
> 1. substitutes for the Software;
> 2. substitutes for any other product or service we offer using the Software that exists as of the
>    date we make the Software available; or
> 3. offers the same or substantially similar functionality as the Software.
>
> Permitted Purposes specifically include using the Software:
> 1. for your internal use and access;
> 2. for non-commercial education;
> 3. for non-commercial research; and
> 4. in connection with professional services that you provide to a licensee using the Software in
>    accordance with these Terms and Conditions.

Our reading: running the Service locally to benchmark it is "internal use and access" — squarely a
Permitted Purpose. Competing Use is about *making the Software available to others* in a commercial
product; publishing measurements does not do that. Nostos must not embed, redistribute, or resell the
Service, which we do not. The clause that actually constrains the writeup is **Trademarks**: "Except
for displaying the License Details and identifying us as the origin of the Software, you have no right
under these Terms and Conditions to use our trademarks, trade names, service marks or product names."
So: name PowerSync nominatively to identify what was measured; no logos, no branding, no implication of
endorsement. The FSL also grants Apache-2.0 on the second anniversary of each release, which is a fact
worth stating accurately rather than as "FSL forever."

## 8. Open questions

1. **Does `@powersync/node` scale to 100 instances on one host?** Unmeasured. ~~Step 3 must include a
   ceiling probe~~ **Resolved 2026-09-02 (D5): the probe is now Step 0.5 and runs before any tier is
   built**; if it caps below 100, the apply ladder tops out where it actually works and says so.
2. **Does the shared source Postgres become the bottleneck with two replication slots plus the writer?**
   If PG saturates first, the harness measures Postgres, not either engine. Needs a headroom check
   before any tier is published.
3. **Is Postgres bucket storage a handicap?** §5 moves bucket storage off the shared instance to avoid
   contention, which is necessary but may not be sufficient. The self-hosted config doc lists MongoDB as
   Option 1 and Postgres as Option 2, and **both the minimal and production setups in
   [deployment-architecture](https://docs.powersync.com/maintenance-ops/self-hosting/deployment-architecture)
   name MongoDB** (single node in replica-set mode for dev, 3-node replica set for production) — Postgres
   storage is never the recommended path there. If MongoDB storage is materially faster, racing on
   Postgres storage reintroduces exactly the crippled default §5 exists to prevent. **Resolved
   2026-09-02 (D6): MongoDB bucket storage; Step 0 rewritten accordingly.**
4. **Sync Streams or legacy Sync Rules?** Streams went GA May 2026 and Rules are now "legacy"
   ([sync overview](https://docs.powersync.com/sync/overview)). Racing the legacy path would be a
   crippled default. Default to Sync Streams; confirm the pinned image supports them.
5. **Checkpoint cadence knob.** PowerSync's apply latency is dominated by checkpoint frequency. If it
   is tunable, tune it in PowerSync's favour and record the setting; if not, note that the p99 is
   structural.
6. **What counts as a drop for PowerSync?** It has no drop-on-full contract. Confirm from the docs or
   observation whether a slow client is ever shed, or only delayed, before writing the accounting code.

## 9. Decisions (grilled 2026-09-02)

Team-lead answers from a `/grill-me` session on the three open calls (ladder shape, Step 0 stack
fixes, go/hold) plus the dependent details they forced. A future agent executes from this section
without re-asking. (Advisor consult for this section was skipped: sandboxed shell could not reach the
CLI login; decisions rest on §3–§8 and the team lead's answers.)

| # | Decision | Rationale / consequence |
|---|---|---|
| D1 | **The one public claim:** same-stage full-path rows/sec at the *apply finish line*, Nostos vs PowerSync, at a stated client count. | Only honest cross-engine claim (§2). Rejected: "Nostos holds 100k wire clients, PowerSync can't" — that is PowerSync's documented deployment shape (1 API container per ~100 connections, §5.1), not a benchmark result. Wire lane (Step 4) stays secondary and is never quoted as the headline. |
| D2 | **HOLD — planning only.** Execution unblocks when Nostos's own 100k fan-out collapse is *root-caused and documented* in `docs/plans/fanout-100k-collapse-2026-09-02.md` (a fix is not required) **and** the team lead explicitly says go. | The 100k investigation may change which Nostos tiers are interesting. A fix is not a precondition because the apply ladder (D3) tops at 1k, where Nostos shows no collapse. Nothing in §6 runs before then — not Step 0, not the ceiling probe. |
| D3 | **Host: this Mac** (10 cores, Docker Desktop VM 8 GiB). **Apply ladder: 100 / 500 / 1k**, stopping at the first tier that fails D4. | The requested 10k–100k ladder is not runnable on the apply lane on one host: 1k apply already needs ~10 API containers + 1k PowerSync clients + 1k Nostos clients + source PG + MongoDB (§3.4). 5k apply (~50 containers) is out. A rented Linux box was offered and declined for now; if it is used later, *both* engines run on it (same-conditions rule, `docs/BENCHMARK-METHODOLOGY.md`). |
| D4 | **Headroom rule (publish/invalid gate):** for the whole run, host load average < 0.8 × cores **and** no non-harness process > 20 % CPU; otherwise the tier is recorded as *invalid* with its logs (`CONTENDED.md` pattern), never re-rolled silently. | Same threshold that invalidated three Nostos re-measures on 2026-09-02. Mechanically enforced by `race.sh` (Step 5), not judged per tier. Also answers §8 Q2: a tier where the shared source PG saturates fails this rule via the PG process. |
| D5 | **Per-container ceiling probe moves to Step 0.5**, before any ladder tier is built. | Sets the containers-per-tier ratio. Running it last (old Step 5 / §8 Q1) risked rebuilding tiers. Skipping it and trusting "100 per container" risked making PowerSync look artificially slow — unfair to them. |
| D6 | **Step 0 = MongoDB bucket storage** (single-node replica set, the documented dev/production path) **+ `docker/powersync/config.yaml` rewritten to the documented schema.** | Current file points bucket storage at the *same* Postgres that carries the race writer and Nostos's replication slot — contaminates both numbers. Postgres-on-a-second-instance rejected: supported but not PowerSync's reference path, so any oddity reads as "you misconfigured it". |
| D7 | **Client-side fairness:** pack N PowerSync clients per Node process via `worker_threads`; sample and publish client-side CPU per engine next to every tier's number. | 1k Node processes cost far more host CPU than 1k Rust `SyncClient`s and would push PowerSync's tiers into D4 failure through no fault of the sync service. A Rust PowerSync client stays rejected (§3.3: protocol is "see the SDK"). |

**Execution order once D2 unblocks:** Step 0 → Step 0.5 → Step 1 → Step 2 → Step 3 → Step 5 → tiers
100 / 500 / 1k under D4 → Step 4 (wire lane, secondary) → Step 6. Stop line in §6 is unchanged: a
loss at any tier is published.

**Still open after grilling:** §8 Q4 (Sync Streams vs legacy Rules — default Streams, confirm on the
pinned image in Step 0), Q5 (checkpoint cadence knob), Q6 (PowerSync drop semantics). All three are
Step 0 / Step 3 findings, not decisions the team lead needs to make in advance.
