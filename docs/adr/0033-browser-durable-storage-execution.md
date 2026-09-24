# ADR-0033: Browser-durable storage execution (ADR-0017 follow-up)

- **Status:** Implemented — Worker + `SqliteWasmStorage` impl of `Storage`+`Outbox`,
  Playwright harness (durable.spec.cjs green), degrade path, sign-out wipe. Verified:
  row survives reload, checkpoint survives reload, signOut wipes OPFS to empty.
  Implements ADR-0017's follow-up scope.
- **Date:** 2026-08-08
- **Implements:** ADR-0017 (Decision L57-60; Follow-up scope L133-147; addendum L159-284)
- **Does NOT supersede:** ADR-0017's backend choice (opfs-sahpool, option 1). This ADR
  records the *execution* architecture for that decision.

## Context

ADR-0017 decided: when browser durability ships, the backend is **official SQLite-WASM
with the `opfs-sahpool` VFS** (option 1), explicitly rejecting wa-sqlite's
`OPFSCoopSyncVFS` (option 2) for its COOP/COEP deployment tax. The follow-up scope
(steps 1-5 + addendum step 6) requires a Worker hosting `SqliteWasmStorage` (impl of
`Storage` AND `Outbox`), a `postMessage` protocol, a Playwright browser harness, a
degrade path, and sign-out wiping OPFS + localStorage checkpoint.

The addendum (2026-07-30) corrected the ceiling from "non-durable" to "**live-only**" —
the browser has no outbox at all (writes ship live-only; nothing survives a reload).
This ADR closes that gap: both `Storage` and `Outbox` land together (addendum step 6).

## Decision

### Worker architecture

The existing `sdk/nostos_web/worker/nostos.worker.js` (WS1) already owns the sole wasm
instance: `NostosSocket` + apply engine + `InMemoryStorage` run inside the Worker; the
main thread (`index.js` / `app.html`) is a pure `postMessage` proxy importing no wasm.
This ADR extends that Worker to also own the SQLite-WASM database:

1. **On boot**, the Worker async-initializes sqlite-wasm with `opfs-sahpool`. On
   success → durable mode. On failure (Safari Private Browsing, old browsers, OPFS
   disallowed) → degrade to `InMemoryStorage` (today's behavior), surfaced on
   `SyncStatus`.
2. **The Rust `SqliteWasmStorage`** (in `nostos-ffi-wasm`, NOT `nostos-core` — core stays
   WASM-clean) holds a `js_sys::Object` handle to a JS wrapper around the sqlite-wasm
   `db`. Each `Storage`/`Outbox` method delegates to the JS wrapper's sync methods via
   `js_sys::Reflect`/`Function::call`. The `opfs-sahpool` VFS provides synchronous
   `FileSystemSyncAccessHandle` writes, so NO `SharedArrayBuffer`/`Atomics` is needed
   — the calls are plain synchronous function invocations.
3. **A `WebStorage` enum** (`Memory(InMemoryStorage)` | `SqliteWasm(SqliteWasmStorage)`)
   unifies both backends so `NostosEngine` (`ApplyEngine<WebStorage>`) and `NostosSocket`
   work with either. The Memory variant is the default (node smoke, standalone);
   SqliteWasm is the Worker's durable path. Both implement `Storage` + `Outbox` by
   delegation; the enum also surfaces `row_count()` / `rows_for()` (diagnostics, not
   on the trait).

### `postMessage` protocol (main ↔ Worker)

Unchanged from WS1 (request/response with monotonic `id`; fire-and-forget `write`).
New messages:
- `{type:"storage", mode:"durable"|"memory"}` — the Worker pushes the backend mode
  after init so the main thread can surface it on `SyncStatus`.
- The existing `signOut` command now wipes OPFS (via `db.clear()` / file removal) +
  `localStorage["nostos:checkpoint:<table>"]` + the token.

### serde-JSON vs transferable `ArrayBuffer` — justification

**serde-JSON over the postMessage boundary; `Uint8Array` over the wasm↔JS boundary.**

- The main↔Worker boundary already uses structured-clone JSON objects (the WS1
  protocol). `RowOp`/`PendingWrite` cross this boundary as field-named JSON, not as
  binary frames — the Worker is the sole wasm host, so the row data never crosses
  main↔Worker as raw bytes.
- The wasm↔JS boundary (Rust `SqliteWasmStorage` ↔ JS sqlite-wasm db) uses
  `Uint8Array` for BLOB payloads (the opaque tuple image). wasm-bindgen maps
  `Vec<u8>` ↔ `Uint8Array` natively, so payload bytes transfer without base64/hex
  encoding. For `apply_batch`, Rust builds a `js_sys::Array` of op objects (each with
  a `Uint8Array` payload field) and calls one JS method that runs the transaction.
  This is ONE boundary crossing per batch (not per row), keeping the hot path lean.

### Schema + transaction shape

`SqliteWasmStorage` mirrors `SqliteStorage`'s schema verbatim
(`nostos_data`, `nostos_meta`, `nostos_outbox` — including `applied_lsn` per-row LSN
gating and `attempts`/`dlq` dead-letter columns). The `apply_batch` transaction
(BEGIN → per-row gated upsert/delete → checkpoint UPDATE → COMMIT) runs in JS as one
atomic unit, exactly as `SqliteStorage::apply_batch` does in rusqlite. The per-row LSN
gate (`WHERE applied_lsn <= ?` on the `ON CONFLICT DO UPDATE` / `DELETE`) is in the SQL
itself, mirroring the reference impl.

### Durable checkpoint

Resume reads the checkpoint from SQLite (`nostos_meta` key `checkpoint`), NOT from
`localStorage`. The `localStorage["nostos:checkpoint:<table>"]` key is retained ONLY as
a sign-out wipe target (clearing it prevents a stale-LSN resume after OPFS is wiped).
The transport's `connect` reads `engine.checkpoint()` (which delegates to
`Storage::checkpoint()`) instead of `read_checkpoint_ls`.

### Degrade path

OPFS unavailable → `InMemoryStorage` + localStorage checkpoint (today's behavior,
surfaced on `SyncStatus` as `mode: "memory"`). The Worker catches the sqlite-wasm init
failure, sets `storageMode = "memory"`, and proceeds with the existing in-memory path.
NOT a crash. The Playwright harness exercises this by testing with OPFS blocked.

### Sign-out (ADR-0029)

`signOut()` wipes:
1. OPFS DB file: `Storage::clear()` on `SqliteWasmStorage` (`DELETE FROM nostos_data;
   DELETE FROM nostos_outbox; UPDATE nostos_meta SET value='0' WHERE key='checkpoint'`).
2. `localStorage["nostos:checkpoint:<table>"]` — removed so the next principal does not
   resume from a stale LSN (closes the e2 stale-LSN gap).
3. The cached token — dropped so the next `connect` requires re-auth.

### No COOP/COEP

`opfs-sahpool` uses synchronous `FileSystemSyncAccessHandle` writes, not
`SharedArrayBuffer`/`Atomics`. Cross-origin isolation is NOT required.
`web/vite.config.ts` stays header-free. This is a *feature* (no deployment tax) —
documented in `docs/api/web.md`.

### Playwright harness

`FileSystemSyncAccessHandle` is Worker+browser-only — it does not exist in Node. The
browser test (Playwright/headless Chrome) is the HEADLINE proof:
1. Write offline → kill page → reload → reconnect → server receives the write.
2. Exercise the in-memory degrade path.

The existing `e2e/browser_live.spec.cjs` pattern (spawn spine + static HTTP server +
headless chromium) is extended with a durable-storage round-trip spec.

## Consequences

**Positive:**
- Browser is now offline-capable AND reload-survivable — rows + outbox + checkpoint
  all persist in the OPFS-backed SQLite file. The "live-only" ceiling (ADR-0017
  addendum) is closed.
- The `Storage`/`Outbox` trait seam pays rent: only the storage backend changes; the
  apply engine, WS transport, and frame pump are untouched.
- Header-free deployment preserved — no COOP/COEP tax on nostos users.

**Negative:**
- The Rust↔JS boundary adds one function-call crossing per `apply_batch` (and per
  outbox op). The wire is already JSON, so serialization cost is comparable; the ops
  cross as a pre-built `js_sys::Array` (one call, not per-row).
- `SqliteWasmStorage` is browser-only (the JS methods don't exist in Node). Host cargo
  tests exercise only the `Memory` variant; the `SqliteWasm` path is proven by the
  Playwright browser test.

## References

- ADR-0017 — the decision this implements (opfs-sahpool, option 1; wa-sqlite rejected)
- ADR-0013 — outbox + dead-letter policy
- ADR-0025 — snapshot-reconcile (`pks_for_table`/`delete_pks`); per-row LSN gating
- ADR-0027 — dead-letter quarantine
- ADR-0029 — sign-out wipe
- `crates/nostos-client/src/sqlite.rs` — `SqliteStorage`, the reference impl mirrored
