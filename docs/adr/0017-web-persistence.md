# ADR-0017: Web persistence (Front 5 — browser-durable storage)

- **Status:** Deferred past v0.1 — decision recorded, follow-up scoped
- **Date:** 2026-07-04

## Context

ADR-0015 shipped the WASM bridge with an in-memory apply engine and a
deliberate deferral: browser-durable row storage (OPFS or otherwise) was left
for a verified follow-up. Task E1 (commit `559b311`) has now shipped the WASM
WebSocket transport, which closes the *transport* gap but leaves the *durability*
gap: on a page reload, the in-memory rows are lost and the client replays from
the `resume_lsn` persisted in `localStorage` (`cairn:checkpoint:<table>`).

This ADR owns the decision the plan (`docs/plans/complete-nostos-fully-wired-operational.md`
Task E2) explicitly punted: **which browser-durable mechanism does nostos adopt,
and does v0.1 ship one at all?** The plan text anticipated "defer" as a
legitimate outcome — this ADR records the evidence for that verdict and scopes
the follow-up.

The spike surveyed three candidate mechanisms in July 2026 and the prior art
among named sync-client projects. The findings are summarized below; the
decision follows from them.

## The candidates (July 2026)

| # | Mechanism | VFS / backend | COOP/COEP? | Bundle add | Atomicity |
|---|-----------|---------------|------------|------------|-----------|
| 1 | **Official SQLite WASM** (`@sqlite.org/sqlite-wasm` v3.53) | `opfs-sahpool` | **No** | ~10–30× pkg (≈1.3 MB uncompressed) | SQLite txn ✓ |
| 2 | **wa-sqlite** (`rhashimoto/wa-sqlite`) | `OPFSCoopSyncVFS` | **Yes** (SAB) | comparable | SQLite txn ✓ |
| 3 | **Raw OPFS keyed rows** | `navigator.storage.getDirectory()` | No | small | **No txn** ✗ |

Browser support for OPFS sync handles (`createSyncAccessHandle`) is universal
in the July 2026 install base: Chrome/Edge 102+, Firefox 111+, Safari 17+
(Sep 2023). Safari Private Browsing disallows OPFS — a durable backend must
fall back to today's in-memory behavior there.

### Prior art (what the named sync clients actually use)

| Project | Language | Durable browser storage | VFS |
|---|---|---|---|
| PowerSync Web SDK | TS in Worker | wa-sqlite (fork) | `OPFSCoopSyncVFS` / `IDBBatchAtomicVFS` |
| RxDB | TS | IndexedDB (OPFS is premium-only) | pluggable |
| Dexie.js | TS | IndexedDB | — |
| ElectricSQL | TS | PGlite (WASM Postgres) | IndexedDB |
| Triplit | TS | IndexedDB | — |

**The decisive prior-art observation:** *none* of these is a Rust→wasm sync
client with a `Storage` trait on the main thread. Every one is TypeScript
already running in a Worker. Nostos's specific shape — Rust core on main thread,
add durability — has no direct precedent.

## Decision

**Defer browser-durable storage past v0.1.** Ship the `localStorage`
checkpoint + replay-from-`resume_lsn` story as the v0.1 ceiling (already
implemented in E1). Commit to **option (1): SQLite-WASM with the `opfs-sahpool`
VFS** for the post-launch slice. Explicitly reject options (2) and (3).

### Why defer for v0.1

1. **The Worker re-architecture is the dominant cost, not the VFS choice.**
   `createSyncAccessHandle` is Worker-only by spec; there is no main-thread
   path. nostos-ffi-wasm runs on the main thread today (`NostosEngine`,
   `NostosSocket`). Going durable means: spawn a dedicated Worker, define a
   `postMessage` command/response protocol, marshal `RowOp`/`PendingWrite`
   across the boundary, and move the WS transport too (it can't call sync
   storage from the main thread). This is a multi-day slice with no
   Node-verifiable test path — the exact ponytail ADR-0015 warned against.
2. **Write-back v1 raised the trait surface from 2 to 5 methods.** A durable
   WASM backend must now implement `Storage` (`checkpoint`, `apply_batch`) AND
   `Outbox` (`enqueue`, `pending`, `mark_done`) — each crossing the Worker
   boundary. The "small seam" assumption that made the deferral look tight in
   ADR-0015 is stale; the cost is larger than when the original ponytail was
   written.
3. **The v0.1 ceiling is honest, not a data-loss bug.** The server holds
   canonical state; the snapshot is re-delivered on reconnect (commit
   `f55c491`); correctness is unaffected. The cost of deferral is one
   cold-reload re-fetch, not data loss. The Show HN / Phase-3 demo audience
   judges the replication-throughput moat, not whether rows survive a refresh.
4. **No prior art in Rust→wasm for this shape** de-risks the plumbing. Every
   cited sync client is TypeScript-in-Worker; nostos would be first, and
   "first" is not the v0.1 gate.

### Why SQLite-WASM + `opfs-sahpool` when durability ships

1. **No COOP/COEP deployment tax — decisive.** `opfs-sahpool` uses synchronous
   `FileSystemSyncAccessHandle` writes, not `SharedArrayBuffer`/`Atomics`, so
   cross-origin isolation is NOT required. wa-sqlite's `OPFSCoopSyncVFS` (option 2)
   forces COOP/COEP onto every nostos user's deployment, which breaks OAuth
   popups, analytics iframes, and any non-CORS-clean embed. `web/vite.config.ts`
   ships zero such headers today and should stay that way.
2. **The atomicity contract is satisfied structurally.** `nostos-core`'s
   `Storage` trait requires that the row writes and the LSN checkpoint land in
   the same atomic transaction (`crates/nostos-core/src/storage.rs`). SQLite
   transactions give this for free. Raw OPFS (option 3) has no multi-region
   transaction primitive — you'd hand-roll a WAL, which is rebuilding SQLite
   badly.
3. **The reference impl already exists.** `SqliteStorage` in
   `crates/nostos-client/src/sqlite.rs` is the exact schema (`cairn_data`,
   `cairn_meta`) and transaction shape a SQLite-WASM backend mirrors. The port
   is mechanical once the Worker plumbing exists.

### Why options (2) and (3) are rejected

- **Option (2) wa-sqlite** — COOP/COEP tax with no compensating advantage over
  `opfs-sahpool` for nostos's single-writer model. PowerSync uses it because
  their VFS layer is custom-JS and predates sahpool's maturity; nostos has
  neither constraint.
- **Option (3) raw OPFS** — fails the atomicity contract structurally; the
  performant variant (chunked containers + offset index) is a hand-rolled
  SQLite. No prior art in any named sync client.

## Consequences

**Positive:** v0.1 ships on the existing in-memory engine without a
multi-day Worker re-architecture on the critical path. The decision is
reversible — when durability ships, the `Storage`/`Outbox` trait seam means
*only* the storage backend changes; E1's transport and apply pump are
untouched (the seam paying rent).

**Negative:** rows are lost on a page reload until the follow-up lands. The
`localStorage` checkpoint means reload replays from `resume_lsn` (one
re-fetch, no duplication — ADR-0009's exactly-once holds). The web demo (E3)
must document this ceiling verbatim rather than imply durability.

**Deployment:** no COOP/COEP headers added. `web/vite.config.ts` stays
header-free; this is now a *requirement* of the chosen future path (option 1),
not merely a current-state observation.

## Follow-up scope (when the slice opens)

1. Add a Worker entry to `nostos-ffi-wasm` (separate `--target web` build)
   hosting a `SqliteWasmStorage` impl of `Storage` + `Outbox`, mirroring
   `SqliteStorage`'s schema.
2. Define the `postMessage` protocol (request/response with monotonic ids;
   `RowOp`/`PendingWrite` as serde-JSON or transferable `ArrayBuffer`).
3. Build a browser test harness (Playwright/headless Chrome) — the ponytail
   blocker ADR-0015 names. `FileSystemSyncAccessHandle` does not exist in
   Node, so the Worker path must be verified in a real browser, not assumed.
4. Keep `opfs-sahpool` as the only VFS. Do not adopt `OPFSCoopSyncVFS`; do
   not add COOP/COEP headers.
5. Fall back to `InMemoryStorage` + `localStorage` checkpoint when OPFS is
   unavailable (Safari Private Browsing, old browsers) — today's behavior,
   made explicit.

## References

- ADR-0015 (FFI bridge strategy; the original deferral ponytail)
- ADR-0009 (LSN resume — the contract that makes the deferral safe)
- ADR-0013 addendum (write-back v1; raised the trait surface to 5 methods)
- `crates/nostos-core/src/storage.rs` (Storage trait + atomicity contract)
- `crates/nostos-core/src/outbox.rs` (Outbox trait)
- `crates/nostos-client/src/sqlite.rs` (reference impl for the follow-up)
- `crates/nostos-ffi-wasm/src/transport.rs` (the transport that must move to the Worker)
