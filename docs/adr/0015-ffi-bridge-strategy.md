# ADR-0015: FFI bridge strategy (Front 5 — deferred)

- **Status:** Deferred (Phase 2–3 — design sketch + kill criterion)
- **Date:** 2026-06-27

## Context

Front 5 ("First-class Flutter + RN + Web from one core") requires shipping the
Rust sync core to four platform ecosystems. The strategy doc specifies four
crates: `nostos-core` (the platform-agnostic engine), `nostos-storage-*` (SQLite
backends), `nostos-ffi-uniffi` (iOS/Android/RN), `nostos-ffi-frb` (Flutter),
`nostos-ffi-wasm` (Web/Node). **None of these crates exist.** The server-side
engine is real; the client core that would be bound across the bridges is not.

## Decision

**Defer the bridges to Phase 2–3.** A bridge with nothing meaningful to bind is
scaffolding; the client core (`nostos-core`: the apply state machine, the storage
trait, the cursor/checkpoint) must exist first.

**Design sketch:**
1. Extract `nostos-core` from the current `nostos-domain` + `nostos-application` —
   the pure sync state machine (apply a `RowOp` to a `Storage` trait, advance the
   LSN checkpoint, evaluate a local predicate for what to request). Runtime-
   agnostic, `Send + Sync`, no tokio.
2. `nostos-storage-rusqlite` (native), `-sqlite-wasm` (web/OPFS), adapters for
   `op-sqlite` (RN) + `sqlite3_flutter_libs` (Flutter).
3. Four FFI shims:
   - **Flutter → `flutter_rust_bridge` v2** (first-class `Stream`).
   - **iOS/Android/RN → UniFFI** (callback-channel pattern for streaming).
   - **Web → `wasm-bindgen` + `wasm-pack`** (Web Worker + OPFS).
   - **Node/Electron → `napi-rs`**.
4. **Critical principle:** the platform brings its own SQLite binary; Nostos
   brings the sync. One `Storage` trait, one wire protocol.

**The seam to manage:** `Send`/`Sync`/lifetime story across tokio (server/Node),
the JS event loop (web), Dart isolates (Flutter), and the RN bridge thread —
without leaking platform complexity into `nostos-core`.

## Rationale

- There is **no single FFI bridge** that serves all four ecosystems well
  (streaming is the deciding factor — STRATEGY §5.3). Four bridges is the cost.
- CI on all four from day one is the de-risk; keeping `nostos-core` runtime-
  agnostic is the architectural guard.

## Consequences

**Positive:** when shipped, one core serves every platform — the Front-5 claim.

**Negative:** the 4-bridge maintenance tax (STRATEGY risk #5); the client core
itself is the prerequisite, so this ADR gates on ADR-0016.

**Kill criterion:** if the WASM bundle exceeds 500 KB gzipped, or if any bridge
can't CI-build on every commit, the strategy's web/mobile claim is at risk
(STRATEGY §9, risk #4/#5).

## Alternatives considered

- **One bridge for all:** rejected — no single bridge does Flutter streaming +
  WASM + RN well (STRATEGY §5.3).
- **Ship empty crate skeletons now:** rejected — scaffolding with no core to
  bind; violates ponytail.

## References

- STRATEGY §5.1–§5.3 (the core + bridge architecture).
- Depends on: ADR-0016 (the client core itself).
