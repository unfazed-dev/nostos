# PowerSync published performance figures — primary-source verification (2026-08-06)

## Purpose
Nostos's `benches/results/RESULTS.md` frames its moat claim (833,307 ops/sec @ 1,000
clients, fan-out) as "208× PowerSync's published high ceiling of 4k ops/sec (417× the
2k low)." This document verifies, from primary PowerSync sources only, whether that
2k/4k figure exists, what it actually measures, and whether it is a valid comparator
for nostos's number.

## Primary sources checked
1. https://docs.powersync.com/resources/performance-and-limits (current live docs page, fetched 2026-08-06)
2. https://powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter (Ralf Kistner, Jan 13, 2025)
3. powersync-ja GitHub org: `react-native-database-benchmarks`, `flutter-database-benchmarks` (client-library micro-benchmarks, not server throughput)

## Table: every concrete PowerSync-published performance figure found

| Figure | Unit | Direction / what it measures | Conditions | Exact quote | Source |
|---|---|---|---|---|---|
| 2,000–4,000 | operations/sec | **Database Replication: source DB → PowerSync Service** (ingest from Postgres into the service) | "Small rows"; PowerSync Cloud plan limits, customizable on Team/Enterprise | "**Small rows**: 2,000-4,000 operations per second" | docs.powersync.com/resources/performance-and-limits |
| up to 5MB/sec | bytes/sec | Same replication-ingest direction, large rows | "Large rows" | "**Large rows**: Up to 5MB per second" | same |
| ~60 | transactions/sec | Replication-ingest, transaction processing | smaller transactions | (per earlier docs snapshot cited in search results; not independently re-quoted from current live page, which does not list this line) | docs.powersync.com (older revision) |
| 2,000–20,000 | operations/sec **per client** | **Sync: PowerSync Service → Client** (fan-out direction, single-client rate) | "depending on the client [SDK]" | "**Sync speed**: Expect a rate of 2,000-20,000 operations per second per client, depending on the client" | docs.powersync.com/resources/performance-and-limits |
| up to 1M rows/client (10M "may still work") | rows | Sync, client-side dataset ceiling, not a rate | — | "Good performance expected up to 1 million rows per client" | same |
| 50,000+ | concurrent connections/instance | Service capacity ceiling, not a throughput rate | configurable, Team/Enterprise | "currently scale to over 50,000 per instance" | same |
| 27.6k | rows/sec | Client-side initial sync throughput, Linux desktop (Ryzen 9 7900X) | local server, 10k–10M row datasets, mean of 10 runs | "**Desktop**: Up to 27.6k rows/sec" | powersync.com/blog/how-fast-is-powersync-performance-benchmarks-for-flutter |
| 7.1k | rows/sec | Client-side initial sync throughput, Android (Pixel 8a) | 10k rows synced | "**Mobile**: Slower throughput, averaging 7.1k rows/sec on Android for 10k rows synced" | same |
| ~200ms / ~40ms | round-trip latency for 100 updates | Incremental sync latency, Android vs. desktop | 100-update batch | (paraphrased from blog body; not a throughput figure) | same |

Notes on what's *not* a primary figure: the search-engine "1,000 buckets/user," "15MB max row," and "1,999 columns" limits are service caps, not throughput numbers, and are omitted from the comparison table above for that reason.

## Verdict: does any primary source support "4k ops/sec" or "2k ops/sec" as a PowerSync ceiling?

**PARTIAL — the number exists, but it measures the wrong direction of the pipe.**

- The literal figure **"2,000-4,000 operations per second"** is real and current, published verbatim at docs.powersync.com/resources/performance-and-limits, under the heading **"Database Replication (Source DB → PowerSync Service)."** So RESULTS.md is not fabricating a number — the digits are correct and traceable to a primary source.
- But that figure is PowerSync's **replication-ingest rate**: how fast the PowerSync Service can consume a Postgres logical-replication stream from a single source database. It has nothing to do with how many clients are connected or how fast data fans out to them. It is structurally the same kind of measurement as nostos's Postgres→server ingest path, **not** nostos's server→N-client fan-out path.
- Nostos's 833,307 ops/sec figure is an aggregate **fan-out** number: total operations delivered per second across 1,000 concurrently connected clients (FakeReplicator source, eval-only). The PowerSync metric that actually corresponds to that direction is **"Sync (PowerSync Service → Client): 2,000-20,000 operations per second per client"** — and critically, that is a **per-client** rate, not an aggregate across N clients. PowerSync does not publish an aggregate multi-client fan-out ceiling anywhere in the docs, blog, or the two benchmark repos checked.
- Comparing nostos's aggregate 1,000-client fan-out number to PowerSync's single-stream replication-ingest number is an apples-to-oranges unit mismatch: same nominal unit ("ops/sec") but measuring two different stages of two different pipelines. The "208×" and "417×" multiples in RESULTS.md are therefore **not a valid same-metric comparison**, even though the underlying "4k" and "2k" digits are real, sourced numbers.

## Recommended honest comparison sentence for nostos to use instead

Do not divide nostos's aggregate fan-out number by PowerSync's replication-ingest number. Two defensible options:

1. **No comparable published aggregate figure — state it plainly:**
   > "PowerSync publishes a per-client sync rate of 2,000–20,000 ops/sec/client (docs.powersync.com/resources/performance-and-limits) but no published aggregate multi-client fan-out ceiling; nostos's 833,307 ops/sec is measured at 1,000 concurrent clients (0.00% drops, eval-only FakeReplicator) — no directly comparable PowerSync number exists to divide against."

2. **If a same-direction comparison is wanted, compare replication-ingest to replication-ingest** (Postgres → server), not nostos's fan-out number:
   > "PowerSync's published Postgres-ingest ceiling is 2,000–4,000 ops/sec for small rows (docs.powersync.com/resources/performance-and-limits); nostos's [replication-ingest benchmark, if/when measured] achieves ___ ops/sec." This requires nostos to actually benchmark its PgReplicator ingest path (real-PG, not FakeReplicator) to make a same-metric claim — that harness does not yet exist per RESULTS.md's own eval-only caveat.

Either way, RESULTS.md's current "208×/417×" framing should be retracted or re-labeled, since it silently compares two different pipeline stages under the same unit label.
