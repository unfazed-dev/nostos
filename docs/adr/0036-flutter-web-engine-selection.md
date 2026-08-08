# ADR-0036: Flutter-web engine selection (shared nostos-ffi-wasm over frb.web)

- **Status:** Implemented (Wave 4b) + **Wave 4c CRDT/writeBatch gap CLOSED** — the
  four CRDT verbs + atomic `writeBatch` now ship on Flutter-web via thin
  `NostosSocket` delegates. `flutter test` green (9/9 engine-web VM tests),
  `cargo test -p nostos-ffi-wasm` green (56/0), `flutter build web` green for
  `nostos_flutter` (example), Playwright browser smoke green **2/2** (connect +
  write + reactive snapshot; AND the new Wave 4c CRDT + writeBatch test, both in
  a real headless Chromium against a live `nostos-server` with durable OPFS
  storage).
- **Date:** 2026-08-08
- **Supersedes:** none. **Relates:** ADR-0017 (web live-only), ADR-0033 (browser-durable
  storage), ADR-0035 (wasm typed-verb surface — the shared backend), ADR-0029 (sign-out).

## Context

The Flutter SDK (`sdk/nostos_flutter`) targets iOS/Android/desktop via
`flutter_rust_bridge` (frb): the native adapter `RustNostosEngine` wraps the
generated `NostosHandle` and drives `nostos-client` (rusqlite + tokio sync loop).
frb *can* compile that same generated binding to web
(`frb_generated.web.dart` exists and loads), so `flutter build web` was already
technically possible. The question this ADR settles is **which backend a
Flutter-web build should drive**, because the frb-web path is architecturally
wrong for nostos:

1. **Durability regression.** frb-web compiles the `nostos_flutter/rust` crate
   (the frb-facing Rust) and any rusqlite-bearing client code to wasm. The
   browser-durable backend nostos actually ships — opfs-sahpool SQLite-WASM
   (ADR-0033), the same one `@nostos-sync/web` uses — lives in `nostos-ffi-wasm`
   (`--target web`), a *different* artifact than frb's web output. Driving frb's
   web binding strands Flutter-web on whatever in-process storage that crate
   compiles to, NOT the OPFS-backed backend. A reload in Safari Non-Private
   Browsing would lose data that `@nostos-sync/web` survives — an incoherent story.
2. **Two wasm artifacts.** Letting frb-web stand would mean nostos ships two
   browser wasm backends (frb-web's and `nostos-ffi-wasm`'s), diverging on
   storage, CRDT, and the typed-verb surface (ADR-0035). That is exactly the
   duplication the hexagonal boundary forbids.

The unified-API plan (ADR-0032) therefore scoped Wave 4b as "Flutter-web uses
the *shared* `nostos-ffi-wasm` backend via a `WebNostosEngine`", explicitly
**rejecting** `frb_generated.web.dart` as the web path.

## Decision

**Compile-time platform selection via a Dart conditional import**, with the
native implementation isolated from the web compile.

### 1. Conditional-import barrel

`engine_selector.dart` exports the native factory, swapped for the web factory
when `dart.library.js_interop` is available:

```dart
export 'engine_selector_io.dart'
    if (dart.library.js_interop) 'engine_selector_web.dart';
```

`Nostos.connect` calls `createNostosEngine(...)`. On native this is
`RustNostosEngine.connect` (frb + `path_provider`); on web it spawns a Worker and
returns a `WebNostosEngine`. Selection is compile-time, so the native path's
imports (`RustLib.init`, `path_provider`, the frb dylib loader) are never even
*imported* on web — `flutter build web` never touches frb's web codegen for
execution (the `frb_generated.web.dart` file still compiles as a transitive
import of `rust/api/nostos.dart`, but it is dead: nothing calls it).

### 2. RustNostosEngine split (forced by PlatformInt64)

The native adapter `RustNostosEngine` was originally defined in `engine.dart`
alongside the abstract `NostosEngine` seam. It was **moved** to
`engine_io.dart` (reached only via `engine_selector_io.dart`). The class body is
unchanged — only its file location moved. The split is not optional:

frb's `PlatformInt64` type alias resolves to `int` on io but `BigInt` on web.
`RustNostosEngine.counterIncrement` passes `delta` (an `int`) to the generated
handle, which type-checks on io but **not** on web (where the alias is
`BigInt`). No single expression satisfies both platforms, so the class body
cannot be compiled against both targets. Isolating it in a native-only file
keeps `flutter build web` off frb's divergent web signatures entirely while the
native path (`Nostos.connect` → `engine_selector_io` → `engine_io.dart`) keeps
the exact logic that shipped in Wave 4a. Native `flutter test` stays green.

`engine.dart` now holds only the platform-agnostic seam: the abstract
`NostosEngine`, the `NostosConnectionState` enum, `NostosTableSub`, and the
`ClientTableFfi` re-export (a plain Dart class that compiles on both targets).

### 3. WebNostosEngine over a Worker (shared nostos-ffi-wasm)

`WebNostosEngine` (`engine_web.dart`) implements `NostosEngine` by driving the
**shared** `nostos-ffi-wasm` backend over a Worker (`web/nostos/nostos_worker.js`).
This is the same `nostos_ffi_wasm.js` `--target web` artifact `@nostos-sync/web`'s
worker consumes — one Rust backend, two JS-layer hosts. Flutter-web therefore
inherits Wave-2 opfs-sahpool durability (ADR-0033), NOT the rejected frb-web
rusqlite-to-wasm path.

The wiring is split into a **pure-Dart** half and a **web-only** half so the
protocol logic is VM-testable:
- `worker_port.dart` — abstract `NostosWorkerPort` + a `FakeNostosWorkerPort`
  (records sends, pushes replies).
- `engine_web.dart` — pure Dart (no `dart:js_interop`): request/response id
  correlation, multi-table watch fan-out, connection-state synthesis from Worker
  pushes, writeStatus polling. Unit-tested with the fake (`engine_web_test.dart`,
  8 tests).
- `web_worker_port.dart` — web-only `dart:js_interop` + `package:web` adapter
  spawning the real browser Worker (`Worker`, `onmessage`, `postMessage`).

### 4. Worker protocol + bootstrap

The Worker (`web/nostos/nostos_worker.js`) is the sole wasm host: it loads
`nostos_ffi_wasm.js`, async-inits sqlite-wasm (opfs-sahpool, or memory degrade),
owns the live `NostosSocket`, and speaks `WebNostosEngine`'s boundary protocol
(connect with a tables list, write→writeId, query→json, applySchema,
watch/unwatch per table, setToken/disconnect/resume/close/signOut; pushes:
status, per-table json snapshots on each onChange tick, writeStatus, storage
mode). `sqlite_wasm_glue.js` is copied verbatim from `sdk/nostos_web/worker/`
(identical backend). Apps place `nostos_worker.js` + `sqlite_wasm_glue.js` + the
`nostos_ffi_wasm.{js,wasm}` artifact in `web/nostos/`; `Nostos.connect(workerUrl:)`
overrides the default `nostos/nostos_worker.js`.

## CRDT + atomic writeBatch gap — CLOSED (Wave 4c)

The wasm surface splits transport (`NostosSocket` — connected, ships) from the
full typed-verb engine (`NostosEngine` — in-process, no transport). Wave 4b left
the OR-set / PN-counter verbs + atomic `writeBatch` as a gap: the four CRDT
verbs threw `UnsupportedError`, and `writeBatch` was a non-atomic loop of single
writes. **Wave 4c closes it** by adding thin delegates on `NostosSocket`:

- `orSetAdd`/`orSetRemove`/`counterIncrement`/`counterDecrement`/`writeBatch`
  now exist on `NostosSocket`. Each delegates to the **same `NostosEngine`** the
  socket already holds (`SocketInner.engine: Rc<RefCell<NostosEngine>>` — the
  engine IS reachable), so the CRDT logic (HLC mint via `nostos-domain`,
  `OrSetElement`/`counter_apply_delta`, `enqueue_batch` atomicity) is reused
  verbatim — **no CRDT algebra is re-implemented in the wasm crate**, and
  `NostosEngine`/native are untouched (no HLC/replica-id duplication on the
  socket, since the engine owns that state).
- The only addition over a bare delegate is `ship_if_open`: the engine path
  enqueues + `apply_local`s but never sends over the wire, so the socket adds
  the "ship now if OPEN + mark_done + reactive tick" half (mirroring
  `NostosSocket::write`'s WS1 contract). A connected client's CRDT op ships
  immediately instead of waiting for the next `onopen` flush.
- The Worker (`nostos_worker.js`) gained `orSetAdd`/`orSetRemove`/
  `counterIncrement`/`counterDecrement`/`writeBatch`/`setCrdtTables` commands;
  `WebNostosEngine` wired the four CRDT verbs (no more `UnsupportedError`) and
  `writeBatch` now sends the single atomic command (no looped ponytail).

## Consequences

- **+** Flutter-web and `@nostos-sync/web` share one Rust backend; durability story
  is consistent (OPFS where available, memory degrade elsewhere).
- **+** Native path unchanged in behavior; the `RustNostosEngine` body is
  verbatim-relocated, native tests stay green.
- **+** (Wave 4c) The full typed Tier-1 surface — including CRDT verbs + atomic
  `writeBatch` — now works on Flutter-web, parity with native. No CRDT
  re-implementation; the delegates reuse `NostosEngine` + `nostos-domain`.
- **−** `frb_generated.web.dart` is still transitively compiled (dead) on web;
  a future cleanup could conditionally import the `rust/api/nostos.dart` barrel
  itself, but `ClientTableFfi` is needed on both targets, so the dead import
  stays until that type is split too.

## Verification

- `flutter test` (SDK package, native VM): 9/9 on `engine_web_test.dart`
  (includes the 3 new Wave 4c tests: atomic writeBatch, orSetAdd,
  counterIncrement/Decrement).
- `cargo test -p nostos-ffi-wasm` (host tests): 56/0 (the CRDT delegate logic is
  covered by the existing `NostosEngine` typed-verb tests the delegates reuse).
- `flutter build web`: green for `sdk/nostos_flutter/example` (nostos_flutter).
- **Playwright browser smoke (real headless Chromium, live `nostos-server`,
  durable OPFS storage): 2/2 green** — the original connect + write + reactive
  snapshot test, AND the new Wave 4c test exercising counterIncrement +
  orSetAdd + writeBatch through the real Worker/wasm/socket path. This is the
  first time the Flutter-web browser round-trip has been run (4b deferred it).
