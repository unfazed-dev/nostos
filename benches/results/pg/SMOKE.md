# Real-Postgres ingest leg — SMOKE fragments (2026-08-17)

**Stage:** real Postgres logical replication (pgoutput) → PgReplicator →
FanOutService → N loopback WebSocket frame-counting sinks, under a paced
batched-INSERT load against the seeded `tasks` table.
**NOT comparable to the 833,307 ops/sec eval-only fan-out headline** (different
replicator, different workload shape, sinks decode payloads for the lag gauge).
Same-stage, same-units only (docs/BENCHMARK-METHODOLOGY.md).

**These are SMOKE numbers** — a seconds-long run on a non-quiet machine to
prove the leg end-to-end. Production quiet-window numbers land in RESULTS.md
as a hand-curated MEASURED section.

## Environment

Apple M4 (10 cores), macOS arm64, rustc 1.95.0, release profile, 2026-08-17,
load avg ~5 (NOT quiet); Postgres 16-alpine in docker (host :5433,
wal_level=logical).

## Command

    cargo run --release -p nostos-bench --features pg --bin nostos-bench-pg-ingest -- \
      --clients 8 --events 2000 --rate 500 --out-dir benches/results/pg

## Orchestrator-run result (the integrator ran this, not the author)

| metric | value |
|---|---|
| rows written | 2,000 (target 500/s, observed 499.7/s — pacing honest, not driver-bound) |
| frames delivered | 16,000 / 16,000 expected — **100.000% delivery, 0.000% drops** |
| fan-out at pace | 3,997 frames/sec (stage number; bound by the 500 rows/s pace) |
| lag write→recv | p50 10.4 ms, p95 14.0 ms, p99 14.8 ms (16,000 samples, skew −0.4 ms; coarse gauge) |
| contamination guard | 0 frames before load (snapshot pollution excluded — slot pre-created) |
| slot lifecycle | `bench_pgi_59436_a4fd497b` pre-created, **dropped after**; integrator verified pg_replication_slots before/after identical |
| fixture cleanup | 2,000 rows deleted; 0 `pgi%` rows left in tasks |

Pre-existing note (not this leg): 9 leaked `e2e_snap_*`/`atlet_*` slots from
earlier test suites sit in pg_replication_slots — the compose ponytail
(max_replication_slots=20) covers the headroom; prune per docker-compose.yml's
note if they accumulate.
