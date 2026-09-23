# Web SDK best practices Nostos can apply — 2026-09-21

Scope: the two web paths ratified in the 2026-09-21 grill (see
`docs/plans/nostos-integration-tauri-flutter-push.md` §Grilled decisions):
(A) Flutter-web via the ADR-0036 engine, (B) `@nostos-sync/web` for JS/TS clients.
Both share one Worker design (`nostos.worker.js` + `sqlite_wasm_glue.js`, duplicated
in `sdk/nostos_web/worker/` and `sdk/nostos_flutter/web/nostos/`), so every item below
applies to both unless marked.

Sources read (official only): sqlite.org/wasm persistence doc; docs.flutter.dev Wasm page; vite.dev Features; MDN Storage
quotas & eviction; web.dev OPFS guide; dart.dev/tools/hooks; pub.dev frb versions.

## What Nostos already gets right (keep)

- **`opfs-sahpool` VFS.** sqlite.org: the right pick for clients that "value
  performance more than concurrency, or are unable to set COOP/COEP". No
  cross-origin-isolation headers needed, works on Safari ≥ 16.4 (the plain `opfs`
  VFS needs Safari ≥ 17). Atomic, unlike raw OPFS.
- **Engine + socket + storage in one Worker**, main thread is a postMessage proxy.
  Required: `createSyncAccessHandle` is Worker-only.
- **Memory fallback with a surfaced `storage: "memory"` mode** for Safari Private
  Browsing / old browsers.

## Gaps, ranked by blast radius

### 1. Second tab = silent data divergence (P0 for any real consumer)

sqlite.org: sahpool "does not directly support concurrency … initializing it twice,
e.g. via two tabs to the same origin, will fail for the second". Today the second
tab's `installOpfsSAHPoolVfs()` throws, the glue catches it, and the tab runs
**in-memory with its own live socket**. Two tabs now hold different local state and
tab 2's writes are non-durable, with no signal beyond `storage: "memory"`.

Prior-art answer: a `SharedWorker` named per DB file owns the
DB + sync on behalf of all tabs; credentials come from the most recently opened tab;
an opt-in multi-tab flag; without shared workers only one tab syncs and state is
mirrored over `BroadcastChannel`.

Recommended, in ponytail order:

1. **Now (≈30 lines):** Web Locks leader election in the Worker —
   `navigator.locks.request("nostos:" + dbName, { ifAvailable: true }, …)`. Loser
   reports `storage: "memory-secondary-tab"` and the facade throws unless the caller
   passed `allowSecondaryTab: true`. Failing loudly beats diverging silently.
2. **Next — DONE 2026-09-21, but NOT as a SharedWorker.** Tried: Chromium exposes
   no `Worker` constructor in `SharedWorkerGlobalScope`, and opfs-sahpool needs a
   dedicated worker's `FileSystemSyncAccessHandle`, so a SharedWorker can neither
   host the engine nor spawn the host. Shipped instead: the losing tab's dedicated
   Worker proxies every command to the leader's Worker over a `BroadcastChannel`
   (responses by id, pushes mirrored, `reason:"follower"`), and queues on the Web
   Lock so it is promoted (opens OPFS, replays its own `connect`) when the leader
   closes. `allowSecondaryTab: true` keeps the standalone memory engine.
3. **Escape hatch:** document the recipe — a per-tab unique DB name when
   multi-tab sharing is not wanted.

### 2. Storage is best-effort until the app asks otherwise (P1)

MDN: all origin storage is best-effort by default and evictable under pressure;
`navigator.storage.persist()` upgrades the origin to persistent (Firefox prompts,
Chromium/Safari decide by heuristics). sqlite.org's "Mysterious Disappearance of
Databases" sidebar is exactly this.

Recommended: call `navigator.storage.persist()` from the **main thread** once at
facade init (Workers cannot prompt), surface the boolean on `SyncStatus` next to
`storage`, and expose `navigator.storage.estimate()` as `storageEstimate()` for the
consumer's quota UI.

### 3. Pending writes on tab close (P1, memory mode P0)

Prior-art recipe: watch the CRUD queue and on `beforeunload` call `preventDefault()`
when local mutations are outstanding. Nostos already exposes `pending` via
`deadLetters()`. Recommended: opt-in `guardUnload: true` on the facade that
installs the handler when `pending > 0`, default on when `storage === "memory"`.

### 4. `@nostos-sync/web` is not consumable from npm as packaged (P1 for path B)

`package.json` `files` ships only `index.js`, `index.d.ts`, `README.md`. The Worker,
glue, and `pkg-web/` wasm are missing, so even a git install needs `wasm-pack` on the
consumer machine. Recommended:

- Build `pkg-web/` in CI and include `worker/`, `sw/`, `pkg-web/` in `files`.
- Construct the Worker the way Vite (and webpack 5) statically detect:
  `new Worker(new URL("./worker/nostos.worker.js", import.meta.url), { type: "module" })`.
  Vite docs call this the recommended form; string paths break under bundling.
- Let the consumer override wasm/sqlite locations (`locateFile`-style option) for
  CDN deploys.
- Bump `@sqlite.org/sqlite-wasm` `3.53.0-build1` → `3.53.4-build1` (npm latest).

### 5. Flutter-web (A) specifics

- Flutter `--wasm` builds need `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: credentialless|require-corp` for multithreading;
  the JS build does not. sahpool needs neither, so Nostos imposes no header
  requirement of its own — say so in the README, because ADR-0036 users will ask.
- Flutter Wasm does not run on iOS browsers at all (WebKit WasmGC bug), and
  Firefox/Safari desktop are blocked by renderer bugs per docs.flutter.dev. Ship
  kit apps as the JS build until that clears; the Nostos Worker is plain JS and
  unaffected either way.
- `sdk/nostos_flutter/README.md` platform table still says "Web: punted" while
  ADR-0036 says shipped and Playwright-green. Fix the row.
- `kit/nostos` has no web-engine wiring; that is the actual A deliverable.

### 6. Dedupe the Worker (P2, hygiene)

`nostos.worker.js` and `sqlite_wasm_glue.js` exist twice with near-identical
comments. One source under `sdk/nostos_web/worker/`, copied into
`sdk/nostos_flutter/web/nostos/` by a script or symlink, before items 1–3 land twice.

## Non-issues confirmed

- **First-table-only checkpoint** (`crates/nostos-ffi-wasm/src/lib.rs:1320`): LSN is
  stream-global, so a single resume point is correct for all tables on one socket.
  Document; do not rewire.
- **WebSocket vs HTTP streaming:** Nostos's JSON-over-WS is fine; revisit only if a consumer sits behind a WS-hostile proxy.
- **COOP/COEP:** not required by sahpool. Do not add them for Nostos's sake.

## Version drift noticed on the way

| pin | current | where |
|---|---|---|
| `flutter_rust_bridge =2.13.0-beta.5` | `2.13.0` stable | `sdk/nostos_flutter/pubspec.yaml`, `rust/Cargo.toml` |
| `@sqlite.org/sqlite-wasm 3.53.0-build1` | `3.53.4-build1` | `sdk/nostos_web/package.json` |
| "native assets are an opt-in flag" | build hooks are first-class in Dart 3.13 docs | `sdk/nostos_flutter/README.md:283` |
