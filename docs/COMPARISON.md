# Nostos vs PowerSync — How We Compare, and Why the Live Race Is Deferred

> *What the benchmark actually measures against PowerSync, why a live head-to-head throughput race is deferred, and how to stand up the PowerSync self-host stack anyway.*

---

## 1. The comparison, as it stands

Nostos's benchmark (`nostos-bench`) compares its Rust server's fan-out throughput against **PowerSync's published server ceiling** of **2,000–4,000 ops/sec** for small rows ([PowerSync Performance and Limits](https://docs.powersync.com/resources/performance-and-limits)).

The claim is scoped and stated verbatim in every results artifact:

> *PowerSync publishes a server-side ceiling of ~2,000–4,000 ops/sec for small rows. Nostos's measurement is of the same logical operation — fanning row-change events to connected clients. The comparison is scoped: Nostos's number is from a synthetic replicator on loopback; PowerSync's is from their docs. The ratio is the point, not the absolute.*

See [`BENCHMARK-METHODOLOGY.md`](./BENCHMARK-METHODOLOGY.md) §8 for the full framing.

---

## 2. Why the live head-to-head is deferred (Phase 0)

A fair live throughput race requires measuring the **same logical operation** on both engines. At Phase 0 Nostos has no client SDK, so the two receive paths are structurally different:

| | Nostos (Phase 0) | PowerSync |
|---|---|---|
| Receive path | raw WebSocket frame → test counter | WS frame → **client SDK → SQLite apply** |
| What a "delivered op" means | frame hit the socket | row applied to the local SQLite |

PowerSync's published 2–4k ops/sec is the **source-DB → PowerSync Service replication** rate. Its **per-client sync** is 2,000–20,000 ops/sec — but that includes the client SDK's SQLite write on every row. Racing Nostos's raw-WS fan-out against PowerSync's full client-apply pipeline would have Nostos "win" for reasons unrelated to the server fan-out moat (we don't do the apply work PowerSync does). That's the apples-vs-oranges trap.

The honest position until Nostos ships a client SDK (ROADMAP Phase 1–2): compare server fan-out against the published number, and validate the PowerSync self-host path exists via the smoke test below — but don't publish a misleading live throughput delta.

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

Once Nostos ships a client SDK that applies rows to a local SQLite (ROADMAP Phase 1: `nostos-core` client crate; Phase 2: Flutter/Web SDKs), both engines will be doing the same end-to-end work. At that point a live head-to-head — same Postgres source, same client count, same apply cost on both sides — becomes methodologically sound, and this doc + the harness upgrade from "smoke" to "throughput comparison."
