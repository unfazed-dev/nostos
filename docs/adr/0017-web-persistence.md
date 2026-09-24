# ADR-0017: Web persistence (Front 5 — browser-durable storage)

- **Status:** Follow-up SHIPPED (ADR-0033) — the opfs-sahpool durable backend
  (Storage + Outbox in a Worker) is implemented and verified (durable.spec.cjs green:
  write survives reload, checkpoint survives reload, signOut wipes OPFS).
  Decision stands: option 1 (SQLite-WASM + `opfs-sahpool`), options 2 and 3 rejected.
  Amended 2026-07-30 (IndexedDB alternative rejected; scope corrected — see the
  addendum at the end).
- **Date:** 2026-07-04

## Context

ADR-0015 shipped the WASM bridge with an in-memory apply engine and a
deliberate deferral: browser-durable row storage (OPFS or otherwise) was left
for a verified follow-up. Task E1 (commit `a5260a2`) has now shipped the WASM
WebSocket transport, which closes the *transport* gap but leaves the *durability*
gap: on a page reload, the in-memory rows are lost and the client replays from
the `resume_lsn` persisted in `localStorage` (`nostos:checkpoint:<table>`).

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
   `7d631c5`); correctness is unaffected. The cost of deferral is one
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
   `crates/nostos-client/src/sqlite.rs` is the exact schema (`nostos_data`,
   `nostos_meta`) and transaction shape a SQLite-WASM backend mirrors. The port
   is mechanical once the Worker plumbing exists.

### Why options (2) and (3) are rejected

- **Option (2) wa-sqlite** — COOP/COEP tax with no compensating advantage over
  `opfs-sahpool` for nostos's single-writer model. It's the right choice for a
  custom-JS VFS layer built before sahpool matured; nostos has neither
  constraint.
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

## Addendum: IndexedDB rejected; the browser is *live-only*, not merely non-durable (2026-07-30)

> **⚠️ CORRECTION (2026-08-05): the "live-only" headline and §1 below are
> SUPERSEDED by `f338188` (2026-07-31, "WS1 slice 2").** That commit shipped an
> in-memory `Outbox` + optimistic local row on the browser write path
> (`crates/nostos-ffi-wasm/src/lib.rs:496-566`: `enqueue` → `apply_local` →
> send-if-open → `mark_done`), so a write while disconnected **queues and flushes
> on reconnect — it no longer throws**. The browser is therefore **offline-capable
> within a session**, matching the native client's in-session behavior. This
> addendum's IndexedDB argument STILL correctly establishes only the
> *reload-durability* ceiling: nothing survives a browser reload (no IndexedDB/
> OPFS), because `Storage`/`Outbox` are sync traits and IndexedDB is async. So the
> accurate one-liner is the inverse of this addendum's title: **"offline-capable
> within a session; nothing survives a reload."** §1's "no outbox / throws /
> live-only" text is retained below as the 2026-07-30 historical record — do not
> act on it.

**Status:** Accepted. The deferral above stands. Its *scope* was wrong, and the
cheaper IndexedDB alternative floated in
`docs/plans/adr-and-docs-completion-audit-2026-07-30.md` is **rejected**.

**Who decided:** the rejection is a tech-lead call made while writing this
addendum, not an operator ratification — the operator asked for the amendment, and
the audit had put the IndexedDB option forward as *open*. Overturnable; the
argument to attack is point 3 below.

Three facts checked against code rather than against this ADR's own prose.

### 1. The browser has no outbox at all — writes are live-only

`NostosSocket::write` (`crates/nostos-ffi-wasm/src/lib.rs:496`) builds a frame and
calls `ws.send_with_str` directly. It never touches `Outbox`. With the socket not
OPEN its `Err` — `"nostos write: WebSocket send failed (socket not OPEN)"` — is
**thrown** at the JS boundary (wasm-bindgen maps `Result<(), JsValue>` to a
thrown exception, not a returned error value; Capacitor's `async write` surfaces
it as a rejected promise).

This is `NostosSocket.write` — the live browser transport — specifically. The
`NostosClient.write` on the `index.js` facade is a different surface: it feeds the
apply engine directly and never opens a socket at all (its `connect()` only sets a
flag), which is the separately-documented Node ceiling below.

Contrast the native path (`crates/nostos-client/src/client.rs:418`): `enqueue()`
first — durable before any network round-trip — then `apply_local()` for the
instant local row.

So the browser is **not a local-first client that forgets its rows on reload**. It
is a **live-only client**: no offline write capture, no optimistic local row, and
rows that vanish on reload. This is *not* silent data loss — the caller gets an
`Err` — but it means row durability alone would not make the browser
offline-capable.

This ADR predates the browser write surface (ADR-0017: 2026-07-04;
`NostosSocket::write`: `609cf05`, 2026-07-12), which is why its Consequences
section reasons only about rows and concludes the cost is "one cold-reload
re-fetch, not data loss". True of rows; silent about writes.

### 2. The required trait surface is 7 methods, not 5 — and 13 for undegraded behaviour

Point 2 of the deferral above counted 5: `checkpoint`, `apply_batch`, `enqueue`,
`pending`, `mark_done`. All five are still required. ADR-0025 added two more
required ones (`pks_for_table`, `delete_pks` for snapshot-reconcile), so
**required-vs-required is 7 vs 5 — 1.4×, not the 2.6× first written here.**

The full surface is 13 (`Storage` 6, `Outbox` 7); the other 6 have defaults. But
those defaults *degrade* rather than fail, and each degradation is a real feature
switched off: the `bump_attempts`/`mark_dead_letter` default disables dead-letter
quarantine (ADR-0027), `apply_local`'s default drops the instant-local row so
writes only appear on the server echo, and `epoch`/`save_epoch` skip the oplog
epoch check. A Worker backend that ships only the 7 required methods is correct
but visibly worse than the native client.

So: 1.4× on the floor, up to 2.6× for parity. Either way the follow-up got more
expensive while sitting still — every ADR that widens a client trait silently
re-prices this work.

### 3. Why IndexedDB is rejected — and what the real blocker is

**Correction to the audit that proposed it:** the objection is *not* that
IndexedDB lacks transactions. It has them, they span multiple object stores, and
rows + checkpoint can therefore commit atomically — `Storage`'s central contract
would survive.

The actual blocker: IndexedDB's API is **asynchronous** and both `Storage` and
`Outbox` are **synchronous** (deliberately — `nostos-core` is WASM-clean, no
tokio). No sync trait method can await an IDB request, so IndexedDB cannot
implement either trait on the main thread. It can only be a *write-behind mirror*
alongside the in-memory store.

Rejected, because a mirror fixes the visible half and leaves the half that
matters:

1. Rows would survive a reload; a write with the socket closed would still throw.
   The result **looks** offline-capable and is not — worse than an honestly
   live-only client, because the failure moves from "obviously missing" to
   "discovered in production".
2. The mirror must write the checkpoint in the same IDB transaction as the rows.
   The existing `localStorage` checkpoint would then run **ahead** of the mirrored
   rows, and resuming from it skips every row between the two positions —
   permanently, since the server never re-sends them. Fixing that means demoting
   `localStorage`, i.e. modifying the one durable thing the browser has today.
3. SQLite-WASM deletes it. Two persistence mechanisms where the second erases the
   first is work that pays for itself only if the first ships for months.

### Re-scoped follow-up

Steps 1–5 stand, plus:

6. The Worker must land **`Storage` and `Outbox` together.** Rows-only repeats the
   half-feature rejected above.
7. Until then the documented ceiling is "**live-only**", not "non-durable".
   `sdk/nostos_web/README.md` claimed "the remaining gap is Node-only" — wrong, and
   wrong in the direction that flatters us. Corrected in this commit.

If a durable read cache is later wanted on its own merits (instant paint, no
snapshot re-fetch), that is a **performance** argument requiring its own
before/after measurement — not this ADR's durability argument, and not a reason to
revisit point 3.

### References (addendum)

- ADR-0013 addendum v2 (outbox dead-letter policy — two of the 13 methods)
- ADR-0025 (snapshot-reconcile — the other two)
- `docs/plans/adr-and-docs-completion-audit-2026-07-30.md` (where the rejected
  IndexedDB option was raised)
