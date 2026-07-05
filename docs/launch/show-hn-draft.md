# Draft: Show HN — Nostos v0.1 (DRAFT FOR OPERATOR REVIEW)

> **Status:** DRAFT. Not published. Operator's call on title, timing, and
> whether the 10k-client honesty is foregrounded or backstopped. Single-line
> conventional commit convention does not apply to this prose; operator edits
> freely.

---

**Title options (pick one):**

- *Show HN: Nostos — a Rust, Apache-2.0 PowerSync alternative with 2-way offline sync*
- *Show HN: We built a Postgres→SQLite sync engine in Rust that hits 833k ops/sec*
- *Show HN: Nostos — local-first sync, Rust-fast, Apache-open, no write-back endpoints*

---

Hi HN. We open-sourced **Nostos** ([repo][repo]) — a local-first sync engine that
reads Postgres logical replication and fans it out to on-device SQLite, with
two-way offline writes. Apache-2.0 end to end. It's the cell in the
local-first matrix that nobody occupied: Rust core, Rust server, real
Postgres logical replication, 2-way offline, free self-host.

[repo]: https://github.com/<operator-placeholder>/nostos

## What it does

- **Read path:** Postgres → `pgoutput` logical replication → Rust fan-out
  server → per-device SQLite. Predicate-compiled `where_sql` subscriptions;
  server-enforced tenant scoping. Snapshot + stream under one exported slot,
  so fresh clients get existing rows and concurrent-writes-during-snapshot is
  exactly-once (proven by a test, not asserted in docs).
- **Write path:** client mutations queue in a durable SQLite outbox, flush over
  the same authenticated WebSocket on reconnect, and apply to Postgres through
  a two-layer allowlist + parameterized values (the trust boundary is the
  security-critical surface; it's been injection-tested against real PG). The
  replication echo is a no-op because client apply is an idempotent upsert —
  no echo suppression needed.
- **Resume:** ack-driven LSN checkpoint. Drop the server mid-stream, restart,
  reconnect — no data loss, no duplication (chaos-tested).

## The honest benchmark

We measured server fan-out against PowerSync's **published** server ceiling of
~2,000–4,000 ops/sec for small rows ([their docs][ps-limits]).

[ps-limits]: https://docs.powersync.com/resources/performance-and-limits

| Tier | Nostos | Drops | vs PowerSync ceiling |
|------|-------|-------|----------------------|
| **1k clients** | **833,308 ops/sec** | **0%** | **208× their high ceiling** (417× their low) |
| 5k clients | 660k ops/sec | 0.91% | still dramatically faster |
| 10k clients (probe) | ~483k ops/sec | ~61.4% | throughput high, drops NOT under 1% |

The **headline is 1k @ 0% drops = 208× PowerSync's published high ceiling.**
That's a real number, end-to-end through the fan-out pipeline (synthetic source
on loopback, real router, real bounded WS fan-out, real WS client receive). The
original Week-1 proof was 142k @ 35.6×; the v0.1 WS write-path + router work
multiplied the 1k figure ~6×.

The **10k-client story is honest, not pretty.** Throughput at 10k is still
~483k ops/sec, but the current architecture drops ~61% of frames because
`FanOutService::run` does a per-event full-store scan (`O(N×E)`) for ack/
eviction. WS write batching (Task C3) helped at every tier but didn't fix the
binding 10k cost — that's the **table-sharded router**, tracked for Phase 2.
We're not shipping sub-1%-drops-at-10k yet; we're shipping "we know exactly
what to fix and the 1k number is real."

**Never compare denominators**: the 1k number is end-to-end fan-out; the
predicate engine's ~1.5M evals/sec is eval-only. Same-denominator only.

## Why we built it

PowerSync is the incumbent. Their client is great. We're not attacking them on
features — Sync Streams GA killed the old "static buckets" attack line and we
retired it. The defensible wedges are:

1. **Rust server throughput.** PowerSync's server is Node/TS with a published
   ~2–4k ops/sec replication ceiling. Nostos's is Rust. 35× at 1k clients.
2. **Apache-2.0 today.** PowerSync's server is FSL (2-year conversion, no-
   compete). Enterprise legal hates FSL. Nostos is Apache-2.0 now.
3. **Write-back without endpoints.** Nostos writes to your Postgres for you
   (ADR-0013). PowerSync's `uploadData()` is "you build and host it."
   ElectricSQL is read-only.
4. **Free, full-featured, unlimited self-host.** No FSL delay, no metered-per-
   op Cloud tax on the OSS edition.

## What's v0.1 and what's next

**v0.1 (this release):** real-PG default + snapshot, `where_sql` predicate
subscriptions, WS batching, write-back v1 with offline outbox, WASM transport
+ `/demo` page, two Flutter fixtures (pomodoro + Supabase-auth todo).

**Conspicuously later:** browser-durable storage (OPFS) is deferred past v0.1
(ADR-0017 — the Worker re-architecture is the cost, not the VFS choice; we
picked SQLite-WASM `opfs-sahpool` post-launch to avoid the COOP/COEP tax).
The web client's `localStorage` checkpoint is **best-effort durability** — the
browser may evict it under storage pressure; correctness is unaffected (the
client re-fetches from `resume_lsn` on reconnect), but a cold tab may replay
more than expected. Flutter/RN/Node SDKs are scoped, not shipped.
Conflict-resolution tiers above LWW, declarative write rules, and function
mode are Phase 4.

## Try it

```bash
git clone <repo>
cd nostos
cp .env.example .env
make setup         # rust 1.95.0 via rust-toolchain.toml
make test          # ~250 tests, all green
make dev-stack     # docker compose PG + nostos-server on :8800
```

Then in another terminal:

```bash
cargo run -p nostos-client --example reactive_scroll   # native 2-way demo
make web-demo                                          # /demo in browser
```

We're looking for design partners — especially anyone currently hosting
PowerSync's `uploadData()` who'd rather not. Founder contact in the repo.

We'd love feedback on: the benchmark methodology, the write-back trust
boundary, and whether the 10k-client honesty reads right.
