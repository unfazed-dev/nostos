# ADR-0034: WASM typed-verb surface (ADR-0032 Wave 4a)

- **Status:** Implemented — host tests green (56/0), `make ci` green (647/0). Browser Playwright verification pending (no headless browser in this env).
- **Date:** 2026-08-08
- **Implements:** ADR-0032 (the typed T1–T5 verb contract) for the WASM bridge — the port that lets `@nostos-sync/web` and (later) Flutter-web reach the typed verbs.
- **Does NOT supersede:** ADR-0015 (wasm-clean scope), ADR-0017/0033 (browser-durable storage), ADR-0030 (CRDT merge tier). This ADR records the *extension* of the wasm bridge from apply-engine-only to the full typed surface.

## Context

ADR-0015 scoped `nostos-ffi-wasm` to the **apply engine only** — feed frames, flush, read checkpoint. The typed verbs (`write` returning an id, `writeBatch`, `orSetAdd/Remove`, `counterIncrement/Decrement`, structured reads) all live on `nostos_client::SyncClient`, which is **tokio-based and unreachable from wasm** (tokio + rusqlite are both `!wasm32`). ADR-0032 ratified the unified typed contract and shipped it for the Flutter native SDK (Wave 1). The web SDK still exposed only the raw apply-engine surface, so `@nostos-sync/web` apps could not use the typed verbs even though the in-engine state to back them already existed.

The gap is *plumbing*, not new logic: the engine holds the rows, the outbox, and the CRDT-merge reachability — the typed verbs are thin orchestration (read → merge → enqueue → apply_local). The native `SyncClient` orchestrates them through tokio RPC; the wasm bridge can orchestrate the same in-process engine synchronously, reusing the pure-Rust CRDT invariants from `nostos-domain`.

## Decision

Extend `nostos-ffi-wasm` from apply-engine-only to the **full Tier-1 typed surface**, as a **port** (not a wiring of `SyncClient`). The native `SyncClient` stays untouched.

### 1. Port, do not wire, `SyncClient`

`nostos_client::SyncClient` is tokio-based and depends on rusqlite + the nostos-infra WS transport — all three are `!wasm32`. Wiring it would require a tokio-on-wasm shim (rejected — ADR-0015 keeps the bridge WASM-clean) or forking the client. Instead, port only the **thin orchestration** of each typed verb (read payload → merge CRDT → enqueue + apply_local) onto the wasm bridge's existing in-process `ApplyEngine<WebStorage>`. No new state machine, no new invariants — the engine already enforces them.

### 2. Reuse `nostos-domain` CRDT types; do not re-implement

The wasm crate already depends transitively on `nostos-domain` (via `nostos-core`). Made the dep explicit so the typed-verb orchestration can name `Hlc`, `OrSetElement`, `counter_apply_delta`, `merge_or_set_or_lww`, `merge_counter_or_lww` directly. `nostos-domain` is pure (zero I/O, zero async) — adding it to the wasm seam does not violate the hexagonal rule (dependencies still point inward: ffi-wasm → core → domain).

### 3. Three `SqliteWasmStorage` overrides — mirror native `SqliteStorage` exactly

The browser-durable backend (`SqliteWasmStorage`, ADR-0033) gained three overrides matching the native `nostos_client::SqliteStorage` semantics:

1. **`enqueue_batch`** — transactional: `BEGIN` → loop `INSERT` → `COMMIT`, with `ROLLBACK` on any mid-batch error. A mid-batch failure leaves the outbox untouched (atomicity), matching the native `rusqlite::Transaction` path. The `Memory` variant's `enqueue_batch` (already present in `InMemoryStorage`) delegates through `WebStorage`.
2. **Dead-letter columns** — `last_error TEXT` and `dead_lettered_at INTEGER` added to the `cairn_outbox` schema, plus an idempotent `migrate_outbox_dlq()` ALTER TABLE for pre-existing DBs. `mark_dead_letter_with_error` writes both columns with a fallback to the old `dlq=1`-only path if the columns are absent (pre-migration DB). Used by `watchWriteStatus` (ADR-0027).
3. **Counter merge in `apply_local` + `read_payload` override** — for tables tagged via `set_counter_tables`, `apply_local` reads the existing row and `merge_counter_or_lww`s instead of clobbering; the same path runs for OR-set tables via `merge_or_set_or_lww`. `read_payload` is overridden so the merge has the prior bytes (the trait default returns `Ok(None)`, which would silently disable the merge).

The `Memory` backend needed **no new storage code** — `InMemoryStorage` already overrides `enqueue_batch`, `read_payload`, and the CRDT-merge arms of `apply_local` (Wave 1). The only missing piece was `WebStorage` delegation, which was added (without it, the trait defaults shadowed both real impls).

### 4. Typed verbs on `NostosEngine`

Each typed verb is a thin orchestration on the in-process engine:

| Verb | Orchestration |
|---|---|
| `write` | `enqueue` + `apply_local` → returns outbox id (`f64` at the JS boundary) |
| `writeBatch` | `enqueue_batch` (atomic) + per-op `apply_local` → returns ids in order |
| `orSetAdd` | mint HLC, build `OrSetElement` JSON, enqueue + apply_local (merge) |
| `orSetRemove` | mint HLC, build tombstone element, enqueue + apply_local (merge) |
| `counterIncrement` / `counterDecrement` | mint HLC, `counter_apply_delta`, enqueue + apply_local (merge) |
| `applySchema` | delegate to `WebStorage::apply_schema` (materializes views, Memory = no-op) |
| `query` | delegate to `WebStorage::query_json` (Memory returns `[]`) |
| `pendingCount` / `deadLetteredCount` / `lastError` | outbox diagnostics for `watchWriteStatus` |

HLC minting uses a per-engine `Cell<Option<Hlc>>` state + a `derive_replica_id()` (atomic counter + wall clock). The CRDT invariants (`Hlc::mint`, `counter_apply_delta`, the merge functions) come from `nostos-domain` — **not** re-implemented in the wasm crate.

### 5. Multi-table `subscribe` + `resume`

`NostosSocket::subscribe(tables, whereSql?)` sends a subscribe frame carrying the full table list (the native client subscribes per-table; the wasm bridge ports the orchestration to a single multi-table frame for the Worker's batched push model). `resume()` re-sends the subscribe frame with the persisted checkpoint as a heartbeat if the socket is open, or signals the caller to reconnect via `connect()` (engine state is preserved in the `dbHandle`). `ponytail:`: a full in-place reconnect requires making the `ws` field interior-mutable; deferred to avoid transport churn in Wave 4a.

## Testing boundary

- **Host tests (56, run in `make ci`):** the typed-verb orchestration on the `Memory` backend, the CRDT-merge correctness, HLC monotonicity, replica-id uniqueness, enqueue_batch atomicity, dead-letter counters, multi-table subscribe frame shape, and resume idempotency. These are the load-bearing invariants — they run on every CI build.
- **Browser tests (PENDING):** the `SqliteWasmStorage` overrides (enqueue_batch transactionality, dead-letter column migration, counter merge over OPFS) require a headless browser + sqlite-wasm. A Playwright harness is the right home; it could not be run in this env. The host tests cover the *orchestration*; the browser tests must cover the *JS↔sqlite-wasm delegation* for the three overrides.

## Consequences

- **`@nostos-sync/web` and Flutter-web can use the typed verbs** without a second bridge. The JS wrapper translates typed-verb calls into the wasm methods documented here.
- **`nostos-domain` is now an explicit dep of `nostos-ffi-wasm`.** It was already transitive; making it explicit lets the wasm seam name the CRDT types. Hexagonal rule holds (ffi-wasm → core → domain, all inward).
- **`make ci` test count: 647** (was 631 pre-Wave-4a). The 16 new tests are host unit tests of the typed-verb orchestration + CRDT merge + subscribe/resume frame shapes.
- **Browser Playwright verification of the three `SqliteWasmStorage` overrides is the open follow-up.** The host tests prove the orchestration; the browser tests must prove the sqlite-wasm delegation.

## Open follow-ups

- **Playwright browser harness** for `SqliteWasmStorage::enqueue_batch` atomicity, `migrate_outbox_dlq`, and counter-merge-over-OPFS — the three overrides are browser-only code paths.
- **`subscribe` over the live wire** — the frame shape is host-tested; the end-to-end multi-table push through the Worker is a manual E3-style check.
- **Flutter-web compile** — the typed surface is now present on the wasm bridge, but Flutter-web still compiles against `frb_generated.web.dart`; wiring the typed verbs through that surface is a separate slice.
