# nostos-bench — throughput harness

Runs an in-process `nostos-server` app, N WebSocket clients and a
`FakeReplicator` driving the real `FanOutService`, then reports ops/sec, drop
rate and latency. Drops are always reported and the environment is recorded.

| bin | measures |
|---|---|
| `nostos-bench` | the headline aggregate fan-out (`make bench`) |
| `nostos-bench-10k` | the 10k-client shape |
| `nostos-fanout-walk` | the fan-out delivery walk in isolation |
| `nostos-reconnect-storm` | mass reconnect / resume |
| `nostos-bench-pg-ingest` | real-Postgres ingest (feature `pg`) |

```sh
make bench   # → benches/results/RESULTS.md
```

Rules: [`docs/BENCHMARK-METHODOLOGY.md`](../../docs/BENCHMARK-METHODOLOGY.md) —
never compare eval-only numbers against end-to-end ones.
