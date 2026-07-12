# SDK Live-E2E Consolidation — 7/7 platforms, real replication round-trips

**Started:** 2026-07-12. **Owner:** Claude (tech lead). **Bar (operator-approved):**
every one of the 7 shipped SDKs proves a **LIVE server→client replication
round-trip** through its real public API — including Web via a headless browser.
Zero offline-FFI asterisks. One reproducible `make sdk-e2e` runner reports 7/7.

## Why

The parity push landed 7/10 platform SDKs, but verification is uneven: only
**Flutter** (`nostos_live_test.dart`, docker PG) and **Rust** (`reactive_scroll`,
in-process) currently drive a *live* server→client round-trip. The other five
(Node, Web, Tauri, Swift, Kotlin) prove only offline FFI + SQLite — their
`subscribe()`/run-loop is a documented `ponytail:` deferral. Without that loop
no live-replication E2E is possible: the apply engine's write path is
server-driven, so offline you cannot even demonstrate write→read. Consolidating
means closing those gaps, not relabeling the offline smokes.

## Done = all true

1. A shared **no-docker E2E server** binary exists: in-process axum `/sync`
   (real handler) + `InMemorySessionStore` + echo `WriteBack` + `FanOutService`
   + a triggerable `FakeReplicator`. Binds `127.0.0.1:0`, prints
   `NOSTOS_E2E_PORT=<port>`. No PG, no docker.
2. Each SDK's **public API**: connect → subscribe → a server-pushed row arrives
   on-device → `query()` sees it → SDK `write()`s a row → server echoes it back
   → `query()` sees the write. Captured proof per SDK.
3. `make sdk-e2e` runs all 7 and reports 7/7 (Web via headless browser).
4. Parity table + memory updated; committed per verified increment.

## The spine (blocking prerequisite): shared E2E server

Extract `reactive_scroll`'s server half into a connectable binary
(`nostos-e2e-server` — an example in `nostos-infra`, the crate that already owns
`FakeReplicator`, `sync_handler`, `SyncRouterState`).

- axum `/sync` (real handler) + `InMemorySessionStore`.
- Echo `WriteBack`: accepted client writes re-emitted as `ReplicationEvent`s
  through `FanOutService` (reactive_scroll already does exactly this —
  "the writer sees its own write arrive via the same replication path").
- `FanOutService` over the shared store.
- `FakeReplicator` + a **control channel** (HTTP `POST /push` or a stdin line)
  to inject a test row on demand.
- Binds `127.0.0.1:0`, prints the port; clean shutdown on Ctrl-C / SIGTERM.

**Verify the spine independently:** a ~30-line WS client subscribes to `tasks`,
server pushes a row, client receives it; client writes a row, server echoes it,
client receives its own write. Exit 0 iff both directions proven.

## Per-SDK slices (parallelizable after the spine)

Each slice: spawn the spine binary → SDK connects via its real public API →
subscribe → assert pushed row arrives + `query()` sees it → `write()` → assert
round-trip → capture proof.

| SDK | Subscribe wiring needed | Test harness | Effort |
|---|---|---|---|
| Rust (`nostos-client`) | none — `reactive_scroll` has it | `tests/e2e_live_replication.rs` vs spawned spine | S |
| Flutter | none — `nostos_live_test` (docker PG) | fold into runner; optional no-docker spine variant | S |
| Tauri | wire `subscribe` cmd + `tauri::ipc::Channel`; `rt.spawn(run_with_reconnect)` | `NostosState` integration test vs spine (command wrapper is thin) | M |
| Node (napi) | wire `subscribe(table, cb)` via napi threadsafe fn; `rt.spawn(run_with_reconnect)` | `smoke_live.cjs` vs spine | M |
| Swift (UniFFI) | wire `subscribe` + **poll** `poll_new_rows()` | `ios-test/main.swift` vs spine (sim → host `localhost`) | M-L |
| Kotlin (UniFFI) | mirror Swift (poll) | `NostosClientTest` live variant on API-34 emu (host via `10.0.2.2`) | M-L |
| Web (`@nostos-sync/web`) | wire `web-sys` `WebSocket` into apply path + `subscribe` | headless-browser (Playwright) test vs spine | L |

## Key design decisions

- **Poll, not push, for the UniFFI mobile SDKs.** The run loop writes received
  rows to an internal queue; Swift/Kotlin call `poll_new_rows()` from their
  event loop. Sidesteps UniFFI 0.28 async-callback complexity and matches the
  existing `block_on`-per-call shape. Upgrade path: a UniFFI callback interface
  for push when a use-case needs sub-50ms ticks.
- **One shared server binary, not per-language harnesses.** Language-agnostic;
  no dep-graph pollution of the separate-workspace SDK crates; every SDK
  connects via the real WS contract — which is exactly the surface under test.
- **Write-back echo reuses existing machinery.** `reactive_scroll`'s echo
  `WriteBack` already pumps accepted writes back through `FanOutService`; the
  spine binary inherits it verbatim.

## Risks (load-bearing unknowns)

- **U1 — spine echo-back without PG.** `reactive_scroll` proves it works. LOW.
- **U2 — sim/emu → host WS reachability.** iOS sim = `localhost`; Android emu
  = `10.0.2.2`. Known-good. LOW.
- **U3 — web-sys WS wiring depth.** Current `@nostos-sync/web` is apply-engine-only;
  browser WS + headless test is the bulk of Web's effort. **Web is the long
  pole.** MEDIUM-HIGH.
- **U4 — UniFFI event delivery.** Mitigated by the poll design. LOW-MEDIUM.
- **U5 — napi threadsafe callback.** Well-trodden pattern. LOW.

## Sequencing

1. ✅ **Spine binary** — built + independently verified (`[spine] PUSH_OK`/`ECHO_OK`,
   both replication directions). Commit `a03e992`.
2. ✅ **Rust SDK live-E2E** (reference template) —
   `crates/nostos-client/tests/e2e_live_replication.rs` green vs the spine in
   0.98s; PUSH + ECHO both directions proven via the SDK's real public API.
   Commit `dc19595`. **This is the shape the 5 FFI SDKs copy.**
3. ✅ **Flutter** — already live (`sdk/nostos_flutter/.../nostos_live_test.dart`:
   two clients × real nostos-server + docker Postgres + HS256 JWTs, the W5
   acceptance proof). Fold-into-runner is mechanical, part of step 7.
4. ⏳ **Wave 1** (needs fresh window) — Node + Tauri (both Rust-native; copy the
   reference template; wire `subscribe`).
5. ⏳ **Wave 2** — Swift + Kotlin (mobile; poll-based `subscribe`; slower
   iteration on sim/emu).
6. ⏳ **Wave 3** — Web (longest — `web-sys` WS into the apply path + Playwright).
7. ⏳ `make sdk-e2e` runner + consolidated parity table; verify every slice
   independently; commit per verified increment.

**Window note (2026-07-12):** paused the fan-out at 83% of the 5h window after a
spine-build agent 429'd; the spine + Rust slice were finished in-context
(main-loop, no agent) instead. The 5 FFI slices resume as a parallel fan-out in
the next window. 3/7 SDKs done; 5 remain.

## Verification discipline (process lesson, reaffirmed)

Independently compile + test **every** delegated SDK slice — agents that don't
finish their build leave compile errors (Tauri/Swift lesson from the parity
push). Re-run each SDK's E2E myself before marking it 🟢. Treat suspiciously
clean agent reports as unverified until reproduced.
