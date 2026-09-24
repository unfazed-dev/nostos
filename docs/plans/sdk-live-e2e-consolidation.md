> **SUPERSEDED (2026-07-30).** Its bar was **7/7** platforms. There are now **ten**, and all
> ten prove a live PUSH+ECHO round-trip in one `SDK_E2E_STRICT=1 make sdk-e2e` run (exit 0,
> zero skips) — see the A9 section of
> [`nostos-completion-assessment-2026-07-29.md`](nostos-completion-assessment-2026-07-29.md).
> Superseded by [`sdk-parity-final-three.md`](sdk-parity-final-three.md) for the 10-platform
> scope. Kept because its per-slice harness design is still what `scripts/sdk-e2e.sh` runs.

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

## Sequencing — COMPLETE (7/7, 2026-07-12)

1. ✅ **Spine binary** (`d82e565`) — `[spine] PUSH_OK`/`ECHO_OK`, both directions.
2. ✅ **Rust SDK live-E2E** (`792a6c8`) — reference template, green in 0.98s.
3. ✅ **Flutter** — already live (`nostos_live_test.dart`, docker PG, W5 proof).
4. ✅ **Node SDK** (`eb3913d`) — `subscribe()` was already wired; `smoke_live.cjs`
   PUSH_OK/ECHO_OK.
5. ✅ **Tauri SDK** (`6bb1897`) — returned the owned runtime + `subscribe`;
   `Option<Runtime>` + custom `Drop` dodges the tokio drop-panic; PUSH_OK/ECHO_OK.
6. ✅ **Swift SDK** (`121a8d8`) — UniFFI poll pattern (`subscribe` + `query()` poll)
   on the iPhone sim; `build.sh` spawns the spine + injects the port via
   `SIMCTL_CHILD_NOSTOS_E2E_PORT`. PUSH_OK/ECHO_OK/SUCCESS.
7. ✅ **Kotlin SDK** (`f4f38ad`) — mirrors Swift's poll on the API-34 emulator
   (`emulator-5556`, host via `10.0.2.2`, port via `-PnostosPort` instrumentation
   arg). **Re-push poll fix:** the cold-start subscribe race flaked on
   independent re-run; re-pushing (idempotent by pk) until the row lands made it
   deterministic. PUSH_OK/ECHO_OK, `tests=2 failures=0`.
8. ✅ **Web SDK** (`609cf05`) — `web-sys` WS into the apply path + Playwright
   headless-browser E2E. **Found + fixed a latent flush bug:** the WASM onmessage
   pump never flushed standalone frames (the U3 risk); an unconditional
   `engine.flush()` mirrors the native client's per-batch commit. PUSH_OK/ECHO_OK.
   (Note: the WASM apply engine is an in-memory KV, not SQL — the test reads
   `rowsFor("tasks")`, not `cairn_data` SQL.)
9. ✅ **`make sdk-e2e` runner** (`scripts/sdk-e2e.sh` + Makefile target) — runs
   all 7 slices; host slices (rust/node/tauri/web) always run, device slices
   (flutter/swift/kotlin) SKIP-with-reason when their runtime is absent. Optional
   slice-name args for focused runs (`scripts/sdk-e2e.sh rust web`).

   > **Count grew after this was written (note added 2026-07-30).** "7 slices" was
   > accurate at this commit; `ALL_SLICES` is now **10** — capacitor, dotnet, and
   > reactnative landed later. The step above is left as-is because it records what
   > this piece shipped; `scripts/sdk-e2e.sh` is the authority on the current set.
   > The same stale "7" had propagated into the script header and the Makefile help
   > text, where it was a live claim rather than a record, and was corrected there.

**Run it:** `make sdk-e2e` (or `scripts/sdk-e2e.sh [slices…]`).

**Every slice was independently re-verified** before commit (re-ran the agent's
test command, confirmed PUSH_OK/ECHO_OK + clippy clean + `forbid(unsafe)` intact).
The Kotlin cold-start flake and the Web flush bug were both caught by that
independent re-run — not by the agents' own green reports.

## Verification discipline (process lesson, reaffirmed)

Independently compile + test **every** delegated SDK slice — agents that don't
finish their build leave compile errors (Tauri/Swift lesson from the parity
push). Re-run each SDK's E2E myself before marking it 🟢. Treat suspiciously
clean agent reports as unverified until reproduced.
