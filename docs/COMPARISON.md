# Nostos — How We Compare (updated July 2026)

> *What Nostos's benchmark numbers actually measure, why each number is labeled by denominator, and the positioning wedges that hold.*

---

## 0. Positioning (July 2026 market facts)

The wedges that hold (and are the defensible positioning):

1. **Rust server throughput** — a pure-Rust server (tokio + axum); every figure labeled in §1.
2. **Apache-2.0 today** — server, core, and every SDK.
3. **Write-back without customer-built endpoints** (Nostos's direct write-back, ADR-0013) vs ElectricSQL's read-only path.
4. **Free, full-featured, unlimited self-host** — no feature gates.

See [`STRATEGY.md`](./STRATEGY.md) for the full strategic brief and a "Threats" note (Supabase acquired Triplit, Oct 2025).

---

## 1. Nostos's numbers — every one labeled

Nostos's benchmark (`nostos-bench`) measures its Rust server's fan-out throughput.

**Every Nostos number is labeled by what it measures, and only same-denominator pairs are compared** — against each other or against any other engine's figure:

| Nostos number | Denominator |
|---|---|
| **142,336 ops/sec @ 1k clients, 0% drops** | **end-to-end** (FakeReplicator → real router → real bounded WS fan-out → frame received by in-process WS client) |
| ~1.5M predicate-evals/sec through 10k predicates | **eval-only** (predicate engine micro-bench, no fan-out, no network) |

The 142k figure is **end-to-end through the fan-out pipeline** (only the *source* of events is synthetic — the `FakeReplicator`). The ~1.5M predicate-evals/sec figure is **eval-only** and is never compared against an end-to-end number — that would be an apples-to-oranges mix. See [`BENCHMARK-METHODOLOGY.md`](./BENCHMARK-METHODOLOGY.md) §8 for the full framing.
