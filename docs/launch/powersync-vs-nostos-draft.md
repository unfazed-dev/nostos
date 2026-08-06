# Draft: PowerSync vs Nostos — the honest comparison (DRAFT FOR OPERATOR REVIEW)

> **Status:** DRAFT for the launch blog / comparison page. Not published.
> Operator edits freely. Mirrors the truth-swept positioning in
> `docs/COMPARISON.md` (commit `8951296`+); do not quote numbers from memory —
> verify against `benches/results/RESULTS.md` before publishing.

---

## The short version

PowerSync is the incumbent. They have more features, more SDKs, more customers,
and a great client. We're not pretending otherwise. Nostos exists because three
things are true at once in July 2026:

1. PowerSync's **server is Node/TS** with a published replication-ingest rate
   of ~2,000–4,000 ops/sec for small rows (Postgres → PowerSync Service) and
   2,000–20,000 ops/sec per-client sync (PowerSync Service → Client) — no
   published aggregate fan-out figure.
2. PowerSync's **server is FSL-licensed** (source-available, 2-year conversion
   to Apache, no-competing-use clause).
3. PowerSync's **write-back is `uploadData()`** — you build it, you host it.

Nostos's server is Rust, Apache-2.0 today, and writes to your Postgres for you.
That's the wedge. Everything else is execution.

## What we retired (we will not lie to win)

Pre-July-2026 Nostos marketing attacked PowerSync for "static buckets only" and
a "1,000-bucket hard cap." **Both are retired.** PowerSync shipped **Sync
Streams (dynamic, on-demand sync) to GA in May 2026**, and the 1,000-bucket
limit is a **soft default (10k configurable)**, not a hard ceiling. These are
no longer quoted as Nostos wedges. Anyone who tells you PowerSync can't do
dynamic sync in July 2026 is reading outdated marketing.

## The numbers, honest units

PowerSync publishes no aggregate multi-client fan-out figure. Its published
rates are ~2,000–4,000 ops/sec replication ingest (Postgres → PowerSync
Service, small rows) and 2,000–20,000 ops/sec per-client sync (PowerSync
Service → Client — a per-client rate, not an aggregate) ([source][ps-limits]).
We measure Nostos's fan-out path (Service → N clients, aggregate) on its own
terms; there is no PowerSync figure in the same units to divide against.

[ps-limits]: https://docs.powersync.com/resources/performance-and-limits

| Metric | Nostos | PowerSync | Comparable? |
|--------|-------|-----------|-------------|
| **1k-client aggregate fan-out** | **833,307 ops/sec @ 0% drops** | no aggregate fan-out figure published | ❌ different pipeline stage — do not divide |
| 5k-client fan-out | 660k ops/sec @ 0.91% drops | not published | Nostos-only |
| 10k-client fan-out (probe) | ~483k ops/sec @ ~61.4% drops | not published | Nostos-only |
| Predicate eval (microbench) | ~1.5M evals/sec through 10k predicates | not published | eval-only — **never** compared to PowerSync's end-to-end number |
| replication ingest (for reference) | not yet benchmarked (real-PG ingest harness pending) | 2,000–4,000 ops/sec (small rows) | same pipeline stage — the only valid future comparator |

**The headline is the 1k-client, 0%-drop, aggregate server-fan-out number:
833,307 ops/sec (current, 2026-07/08).** No PowerSync figure measures the same
thing, so we report it on its own terms rather than as a ratio. The original
Week-1 proof was 142k ops/sec aggregate fan-out; the v0.1 WS write-path +
router work multiplied the 1k figure ~6×. The 10k-client story is honest:
throughput stays high but the *current* architecture drops ~61% of frames at
10k because `FanOutService::run` does a per-event full-store scan. The fix
(table-sharded router) is scoped for Phase 2; the measurement, not the
marketing, says so.

A same-Postgres-source, same-client-count, same-apply-cost live race against
PowerSync's self-host stack is the next methodological step. Until that
harness runs — and until Nostos has its own real-PG replication-ingest number —
we report Nostos's aggregate fan-out figure on its own terms and compute no
ratio against any PowerSync figure. **No cross-stage ratio, ever.**

## Feature matrix

| | Nostos (v0.1) | PowerSync (July 2026) |
|---|---|---|
| Source DB | Postgres (logical replication, `pgoutput`) | Postgres, MySQL, MongoDB |
| Server language | Rust | Node.js / TypeScript |
| Server license | **Apache-2.0** | **FSL** (2-yr → Apache, no-compete) |
| Client SDKs shipped | Rust, WASM (web transport) | Flutter, React Native, JS/Web, Swift, Kotlin |
| Dynamic sync | `where_sql` predicate subscriptions | **Sync Streams GA** (May 2026) |
| Write-back | **direct, allowlisted, parameterized** (ADR-0013) | `uploadData()` — you build & host |
| Conflict resolution | LWW (server-authoritative, WAL order) | LWW + custom merge functions |
| Web durability | in-memory + localStorage checkpoint (OPFS deferred, ADR-0017) | IndexedDB / OPFS (wa-sqlite) |
| Self-host | free, full-featured, unlimited | free, full-featured, unlimited (Open Edition) |
| Managed cloud | scoped, not shipped | yes |

**What this table says:** PowerSync is more mature on every client surface and
has a real managed cloud. Nostos's v0.1 is narrower on purpose — fewer SDKs,
no managed cloud yet — but the things it does ship (Rust server, Apache-2.0,
direct write-back, predicate subscriptions) are exactly the things PowerSync's
architecture can't match without a rewrite.

## The write-back trust boundary (where Nostos's design pays off)

PowerSync: client queues mutations → you implement `uploadData()` → you host
the endpoint → you handle conflicts → you scope writes per-tenant.

Nostos: client queues mutations in a durable SQLite outbox → flushes over the
same authenticated WebSocket on reconnect → server applies to Postgres through
a two-layer allowlist (`NOSTOS_WRITE_TABLES`) + identifier regex + 100%
parameterized values, server-authoritative LWW by WAL order. The replication
echo is a no-op because client apply is an idempotent upsert.

The security-critical surface is the write-back adapter. It's been injection-
tested against real PG: `title"; --` in a payload returns `InvalidPayload`,
full stop. The plan-v1 spec said "bind everything as text and let PG coerce";
we deviated to typed-inference binding (`SqlValue` enum) because **PG does not
implicitly coerce `text`→`uuid` for parameters** — that deviation is ratified
in ADR-0013's addendum, not hidden.

## When to pick which

**Pick PowerSync if** you need a mature Flutter/RN SDK today, you want a
managed cloud, you're fine with FSL, and `uploadData()` doesn't bother you.

**Pick Nostos if** your legal team can't ship FSL, your server throughput needs
exceed Node's ceiling, you'd rather not build write-back endpoints, or you
want a Rust core you can extend to WASM/embedded/edge without a JS runtime.

**Pick neither if** you need CRDTs, rich-text co-editing, or non-Postgres
sources today.

## The threat we're watching

Supabase acquired Triplit (Oct 2025). Supabase has first-party offline
ambitions and the distribution to ship them. If/when Supabase ships a
first-party local-first sync layer, the "write-back without endpoints" wedge
weakens for the Supabase-installed base specifically. Our response is to be
the Apache-2.0, Rust-fast, self-hostable option for everyone *not* on
Supabase's cloud — and to be Supabase-JWT-compatible (ADR-0007) so Supabase
users can adopt Nostos's server without leaving Supabase auth.

## Try it

```bash
make dev-stack                              # PG + nostos-server on :8800
cargo run -p nostos-client --example reactive_scroll   # 2-way native demo
make web-demo                                # /demo in browser
```

The `/demo` page connects to your dev server, takes a `where_sql` predicate,
and shows live filtered rows + the checkpoint advancing. Reload the tab — it
resumes from the checkpoint. That's the moat, visible in a browser tab.
