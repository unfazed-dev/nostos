# Nostos vs PowerSync — How We Compare (updated July 2026)

> *What the benchmark actually measures against PowerSync, why each number is labeled by denominator, and how to stand up the PowerSync self-host stack for an apples-to-apples race.*

---

## 0. Repositioning (July 2026 market facts)

Two of Nostos's historical attack lines no longer hold and have been retired:

- **"Static buckets only"** — PowerSync shipped **Sync Streams (dynamic, on-demand sync) to GA in May 2026**; the legacy YAML bucket sync rules are now "legacy." Nostos no longer claims PowerSync can't do dynamic sync.
- **"1,000-bucket hard cap"** — the 1,000-bucket-per-user limit is a **soft default** (10k configurable by request). It is not a hard failure ceiling and is no longer quoted as one.

The wedges that *do* still hold (and are the defensible positioning):

1. **Rust server throughput** vs PowerSync's Node/TS server and its published ~2–4k ops/sec replication ceiling.
2. **Apache-2.0 today** vs PowerSync's server FSL license (2-year conversion to Apache, no-compete clause).
3. **Write-back without customer-built endpoints** (Nostos's direct write-back, ADR-0013) vs PowerSync's `uploadData()` (you build & host it) and ElectricSQL's read-only path.
4. **Free, full-featured, unlimited self-host** — no FSL delay, no metered-per-op Cloud tax on the OSS edition.

See [`STRATEGY.md`](./STRATEGY.md) for the full strategic brief and a "Threats" note (Supabase acquired Triplit, Oct 2025).

---

## 1. The comparison, as it stands — every number labeled

Nostos's benchmark (`nostos-bench`) compares its Rust server's fan-out throughput against **PowerSync's published server ceiling** of **2,000–4,000 ops/sec** for small rows ([PowerSync Performance and Limits](https://docs.powersync.com/resources/performance-and-limits)).

**Every Nostos number is labeled by what it measures, and only same-denominator pairs are compared:**

| Nostos number | Denominator | Compared against | Competitor denominator |
|---|---|---|---|
| **142,336 ops/sec @ 1k clients, 0% drops** | **end-to-end** (FakeReplicator → real router → real bounded WS fan-out → frame received by in-process WS client) | PowerSync's published **~2,000–4,000 ops/sec** server replication ceiling | server-process replication rate (their docs) |
| ~1.5M predicate-evals/sec through 10k predicates | **eval-only** (predicate engine micro-bench, no fan-out, no network) | *nothing directly* — PowerSync publishes no comparable predicate-eval number | n/a |

The 142k figure is **end-to-end through the fan-out pipeline** (only the *source* of events is synthetic — the `FakeReplicator`). The ~1.5M predicate-evals/sec figure is **eval-only** and is never compared against PowerSync's end-to-end replication number — that would be an apples-to-oranges mix. See [`BENCHMARK-METHODOLOGY.md`](./BENCHMARK-METHODOLOGY.md) §8 for the full framing.

The claim, scoped, as stated in every results artifact:

> *PowerSync publishes a server-side ceiling of ~2,000–4,000 ops/sec for small rows. Nostos's measurement is of the same logical operation — fanning row-change events to connected clients. The comparison is scoped: Nostos's number is end-to-end through the fan-out pipeline with a synthetic replicator on loopback; PowerSync's is from their docs. The ratio is the point, not the absolute.*

---

## 2. Why the full live head-to-head is still deferred

Nostos now has a native client (`nostos-client`, rusqlite + tokio SyncClient) and a WASM bridge (`nostos-ffi-wasm`), so the receive paths are no longer structurally mismatched at the apply layer. What still differs is **workload coverage**: Nostos's published 142k number is the **fan-out server path** (the moat); a same-Postgres-source, same-client-count, same-apply-cost live race against PowerSync's self-host stack is the next methodological step. Until that harness runs, the honest position is: compare Nostos's labeled fan-out number against PowerSync's published server ceiling, and validate the PowerSync self-host path exists via the smoke test below — but don't publish a live delta that mixes denominators.

| | Nostos (today) | PowerSync |
|---|---|---|
| Receive path | WS frame → **native client → SQLite apply** (`nostos-client`); or raw frame → test counter (the 142k bench) | WS frame → **client SDK → SQLite apply** |
| What a "delivered op" means | frame received by WS client (bench); row applied to local SQLite (native client) | row applied to the local SQLite |

PowerSync's published 2–4k ops/sec is the **source-DB → PowerSync Service replication** rate. Racing Nostos's raw-WS fan-out (no apply) against PowerSync's full client-apply pipeline would have Nostos "win" for reasons unrelated to the server fan-out moat. That's the apples-vs-oranges trap the labeling in §1 is designed to avoid.

---

## 3. Standing up the PowerSync self-host stack

The stack lives in [`docker/docker-compose.powersync.yml`](../docker/docker-compose.powersync.yml) and brings up the PowerSync Service ([`journeyapps/powersync-service`](https://hub.docker.com/r/journeyapps/powersync-service), Open Edition) against the **same Postgres** Nostos's `PgReplicator` reads. A row inserted once is therefore visible to both engines — the apples-to-apples setup.

```sh
make ps-up        # Postgres + PowerSync Service
make ps-logs      # tail PowerSync
make ps-down      # stop both
```

PowerSync is configured in **dev mode** (`docker/powersync/config.yaml`): anonymous-token auth (no external Supabase JWT needed for local testing) and a single sync rule over the `tasks` table (`docker/powersync/sync-rules.yaml`) mirroring Nostos's benchmark workload.

### What the smoke test proves

`NOSTOS_POWERSYNC=1 cargo test -p nostos-infra --test powersync_smoke -- --nocapture`

asserts three things (env-gated so CI stays green without the stack):

1. the PowerSync Service is **healthy** (`GET /healthcheck`);
2. it **ingests from the shared Postgres** (a logical-replication slot is active on the same PG);
3. it **serves a sync WebSocket** (the `/sync/anonymous` endpoint upgrades).

That validates the self-host path and that the comparison artifact exists — without claiming a throughput winner.

---

## 4. When the live race becomes worth running

Nostos's native client (`nostos-client`, rusqlite + tokio SyncClient) and WASM bridge (`nostos-ffi-wasm`) are now in tree — the apply path exists. The remaining gate for a methodologically clean live head-to-head is wiring both engines against the **same Postgres source** with the **same client count** and **same apply cost on both sides** (ADR-0015 Flutter/RN/Node FFI bridges close the remaining platform-coverage gap). At that point this doc + the harness upgrade from "smoke" to "throughput comparison."
