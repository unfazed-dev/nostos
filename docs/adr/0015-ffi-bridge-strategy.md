# ADR-0015: FFI bridge strategy (Front 5)

- **Status:** WASM shipped (in-memory apply bridge); Flutter / RN / Node-native remain
- **Date:** 2026-06-27 (deferred) · 2026-06-28 (WASM shipped)

## Context

Front 5 ("First-class Flutter + RN + Web from one core") requires shipping the
Rust sync core to four platform ecosystems via four FFI bridges. As of Tier 2.5,
the prerequisite is met: `nostos-core` (ADR-0016) exists as a pure, WASM-clean
apply engine, and the first bridge — WASM — has shipped.

## Decision

**Ship the WASM bridge first; Flutter / RN / Node-native follow.** The
Architecture advisor (GLM-5.2) chose FFI over the predicate engine (ADR-0012)
because FFI delivers a usable product ("my Flutter app syncs offline and
reconnects") while the predicate engine is invisible server-side IP. WASM is the
canonical first bridge: it's the one that can be fully verified in-process
(`wasm-pack build` + a Node smoke test) and proves the bundle-size kill
criterion for all future bridges.

### What shipped (WASM)

**`crates/nostos-ffi-wasm`** — a `wasm-bindgen` bridge over `nostos-core`. A thin
projection of the apply engine's public surface to JS-friendly types:

- `NostosEngine` — wraps `ApplyEngine<InMemoryStorage>`. Methods: `feed(Frame)`,
  `flush()`, `checkpoint`, `rowCount`.
- `Frame` — mirrors `nostos_core::Frame`; `op` is a JS string (`"insert" |
  "update" | "delete"`), `lsn`/`txn_id` are `f64` (no BigInt at the boundary —
  real WAL positions stay under 2^53).
- `Outcome` — the commit result (`checkpoint`, `rowsApplied`).

**Kill criterion met:** the `.wasm` is **17 KB gzipped — 3% of the 500 KB
budget.** `nostos-core`'s deps are all pure-Rust (serde, serde_json, thiserror,
uuid, bytes) with no tokio/SQLite, so the bundle is small. Verified by a Node
smoke test (16 checks: frame construction, buffered feed, atomic flush,
checkpoint advance, idempotency, deletes, transaction-boundary batching).

**Toolchain note:** `uuid` needed the `js` feature added at the workspace level —
a no-op on native (only activates browser RNG under `cfg(target_arch="wasm32")`).

### A documented constraint (surfaced via `[$read-the-damn-docs]`)

**OPFS persistence requires a Web Worker.** `createSyncAccessHandle` (the sync
OPFS path) is Worker-only by spec, async to acquire, sync to use. A real
browser-durable backend is therefore a Worker module with a message protocol —
NOT a direct `wasm-bindgen` export. It also can't be verified in a Node-only
test harness (Node has no `FileSystemSyncAccessHandle`). Per ponytail's
no-unproven-code rule, OPFS is deferred to a verified follow-up that needs a
browser test harness; this slice ships the in-memory apply path (survives the
apply loop, not a page reload — honest about its scope).

## What remains deferred

- **OPFS persistence** — the browser-durable backend (Worker + sync-OPFS).
- **The transport on WASM** — `SyncClient` (tokio) doesn't run on wasm; a
  `web-sys` WebSocket transport is a separate slice (comes with OPFS).
- **Flutter (`flutter_rust_bridge` v2)**, **iOS/Android/RN (UniFFI)**,
  **Node-native (`napi-rs`)** — the other three bridges. They follow the same
  `nostos-core` surface; each needs its platform's codegen tool exercised
  against a real platform project to verify.

## Consequences

**Positive:** the WASM apply path is real and proven; the bundle is far under
budget; the JS↔Rust boundary works end-to-end. One bridge down, three to go.

**Negative:** the in-memory bridge doesn't survive a page reload (no OPFS yet),
and there's no transport on WASM yet — so a browser client can't *connect* to
`/sync`, only apply frames it's handed. The full browser demo waits on OPFS +
the `web-sys` transport.

## Alternatives considered

- **OPFS persistence now:** rejected — Worker-only by spec, unverifiable in
  Node; would ship unproven code (ponytail).
- **One bridge for all four ecosystems:** rejected — no single bridge does
  Flutter streaming + WASM + RN well (STRATEGY §5.3).
- **Predicate engine (ADR-0012) before FFI:** rejected by the advisor — it has
  a hidden dependency (opaque payloads need a column decoder for typed
  comparison) and is invisible to the buyer.

## References

- Depends on: ADR-0016 (`nostos-core` — now exists).
- Enables: the browser demo (with OPFS + transport, deferred).
- Code: `crates/nostos-ffi-wasm` (bridge), `smoke.mjs` (the verification).
- STRATEGY §5.1–§5.3 (the core + bridge architecture).
