# Atlet Appwrite Flutter Web Implementation Plan

> **For agentic workers:** Execute these tasks in order in the `atlet-appwrite-web` worktree. Keep PR #60 draft and base this branch on its head until that PR merges.

**Goal:** Run the same full visual Atlet Flutter UI in Chrome against the hosted ADS Appwrite project, with durable OPFS offline sync and the existing Rust smoke launcher.

**Architecture:** A Rust WASM Appwrite bridge owns principal, cursor, and outbox invariants. A SharedWorker broker authenticates private tab ports; the authenticated host tab starts one dedicated OPFS Worker and transfers its private engine port to the broker. The Flutter UI stays shared with macOS.

**Tech Stack:** Rust `nostos-core` and `nostos-ffi-wasm`, SQLite-WASM OPFS, JavaScript module Worker, Dart Flutter, Chrome, Appwrite Cloud, Rust Atlet harness.

**Spec:** `docs/plans/atlet-appwrite-web-design-2026-09-26.md`; parent contract `docs/plans/atlet-cross-sdk-cloud-reference-2026-09-26.md`.

## Global constraints

- The ADS project is `6ab741900038c74d1086`, region `fra`; test accounts come only from ignored `apps/atlet/.env.cloud`.
- Direct is the default; Appwrite JWTs stay in memory and the Function retains cloud write authority.
- `nostos-core` stays WASM-clean; no new async, Tokio, or SQLite dependency there.
- Browser tests must assert `durable` OPFS, reload survival, the admin and two customers, and one hosted order.
- Shell wrappers may call toolchains, but persistent acceptance launchers are Rust binaries.
- Run `make ci`, `scripts/check.sh flutter`, the browser cloud runner, and the arxa PR checks before reporting a pass.

## Task 1 — Durable Appwrite state in WASM

**Files:** `crates/nostos-ffi-wasm/src/lib.rs`, `crates/nostos-ffi-wasm/src/sqlite_wasm.rs`, `crates/nostos-ffi-wasm/tests/`.

**Interface:** Add Appwrite methods to the existing `NostosEngine`: `newAppwrite(dbHandle,deviceId)`, `bindPrincipal(endpoint,project,function,user)`, `applyAppwritePage(body)`, `appwritePending()`, `appwriteAck(id)`, `appwriteReject(id,error,permanent)`, and read-only `appwriteAfter`. Reuse its existing `writeBatch`, `rowsFor`, `query`, `applySchema`, `clear`, and write-status accessors. Page validation delegates to `AppwriteCursor::apply`; Rust does not own HTTP.

- [ ] Write failing Rust host tests: a principal change clears private rows and pending writes; a hidden-only page advances the horizon; a bad page leaves it unchanged; an acknowledged write is removed; a transient failure stays queued; inactive wipe rejects subsequent writes.
- [ ] Implement `Storage::principal/save_principal` for `SqliteWasmStorage` using `nostos_meta`, and delegate them through `WebStorage`. Preserve `clearAll()` removal of principal and horizon. Initialize a durable `device_id` once in `sqlite_wasm_glue.js` using `crypto.randomUUID()`; retain it across sign-out, as native SQLite does.
- [ ] Implement the exported bridge methods with `ApplyEngine<WebStorage>`, `AppwriteCursor`, and `Outbox`. Convert JS objects to `PendingWrite` once at the Rust boundary. Derive each 32-hex mutation ID as native does: SHA-256 of the durable device ID bytes followed by the big-endian outbox ID, taking the first 16 digest bytes. The Function rejects other ID shapes. Return outbox IDs as numbers only below JS's safe-integer ceiling.
- [ ] Run `cargo test -p nostos-ffi-wasm`, `cargo clippy -p nostos-ffi-wasm --all-targets -- -D warnings`, and the existing WASM/browser smoke; commit `feat: expose Appwrite state through WASM`.

## Task 2 — Browser HTTP transport and Worker protocol

**Files:** `sdk/nostos_flutter/web/nostos/nostos_worker.js`, new `sdk/nostos_flutter/web/nostos/appwrite_transport.js`, `sdk/nostos_flutter/lib/src/engine_web.dart`, `sdk/nostos_flutter/test/engine_web_test.dart`.

**Interface:** `connect` adds `{provider:'appwrite', endpoint,projectId,functionId,userId,token}`. Worker responses retain `{id,ok|error}` and pushes retain `status`, `snapshot`, `writeStatus`, `storage`; `status` also carries `accessRevoked` on the exact inactive-account 403.

- [ ] Add failing fake-port Dart tests for Appwrite connect fields, token refresh, disconnect/resume, and the distinct revoked state.
- [ ] Add a Worker transport class that uses `fetch` against `/functions/atlet_sync/executions`, checking both outer HTTP and `responseStatusCode`; sends mutation IDs from WASM; pushes before pulling bounded pages; handles transient/permanent errors like the native client; never persists JWTs.
- [ ] Reuse the existing dedicated Worker command dispatcher for reads/writes/watches. Route tabs through a SharedWorker broker with private ports and no BroadcastChannel payloads; validate Appwrite JWTs before cached rows open. The Appwrite bridge supplies the same local methods that `NostosSocket` supplies. Push initial snapshots after schema apply and later ticks after local writes or remote apply.
- [ ] Run `fvm flutter test test/engine_web_test.dart` from `sdk/nostos_flutter` and the SDK Playwright Worker smoke; commit `feat: sync Appwrite from Flutter web Worker`.

## Task 3 — Packaged browser storage and Flutter entrypoint

**Files:** `sdk/nostos_flutter/lib/src/engine_selector_web.dart`, `sdk/nostos_flutter/lib/src/nostos_database.dart`, `apps/atlet/flutter/web/nostos/`, `apps/atlet/flutter/lib/adapters/nostos_adapter.dart`, `apps/atlet/flutter/lib/main.dart`.

- [ ] Make `createAppwriteNostosEngine` connect through the broker and construct `WebNostosEngine` with the Appwrite connect payload. Keep server-mode WebSocket behavior and route its tabs through the same private broker.
- [ ] Vendor the pinned `@sqlite.org/sqlite-wasm` distribution under the app's served `web/nostos/` assets, preserve its license, and resolve the import locally. Rebuild and copy the `nostos-ffi-wasm` JS/WASM artifact after Task 1.
- [ ] Build `fvm flutter build web` with `ATLET_PROVIDER=appwrite`. Serve the build on `localhost`, confirm the Worker reports `durable` rather than silent memory fallback, and confirm sign-in reaches the hosted Function.
- [ ] Add a browser reload test that queues a session write offline, reloads, sees the pending row, resumes, and waits for the cloud echo; commit `feat: run Atlet Appwrite in Chrome`.

## Task 4 — Rust browser acceptance, arxa CI, docs

**Files:** `apps/atlet/harness/src/bin/appwrite_flutter_web_smoke.rs`, `apps/atlet/flutter/web/e2e/appwrite_cloud.cjs`, `scripts/check.sh`, `.github/workflows/ci.yml`, `apps/atlet/README.md`, `sdk/nostos_flutter/README.md`, ADR 0051.

- [ ] Use a dedicated Rust browser launcher with evidence including OPFS mode, reload recovery, provider, app/SDK versions, source ref, fixture ID, timing, and structured errors. Fail for missing Chrome, credentials, Function, or durable storage.
- [ ] Run the visual admin, customer A, customer B, and order scenarios against the same hosted project from Chrome. Confirm customer B isolation and a second native Nostos client sees the Chrome order after sync.
- [ ] Add a serialized arxa `atlet-web-cloud` PR job and a matching `scripts/check.sh atlet-web-cloud` area. Upload exact credential-free JSON evidence and fail on a missing artifact.
- [ ] Run local gates and CI. Review source, logs, and uploaded evidence; fix all findings before marking the PR ready. Keep physical remote push as its separate acceptance gate.

## Review checkpoints

After each task, inspect the diff against the spec, run the named tests, and
commit only that task's files. The final review checks stale-account reads,
partial page apply, lost outbox acknowledgements, OPFS memory fallback,
browser token persistence, and cloud fixture cleanup.
