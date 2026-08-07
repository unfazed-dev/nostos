# Nostos Unified API — Implementation Plan (for executor agents)

**Status:** ratified 2026-08-08. Contract: `docs/plans/nostos-unified-api-contract.md`
(v1 + v1.1 + v1.2 tier-1 amendment). Written by the tech-lead session; executed
by other agents; **reviewed by the tech lead after each wave**. Do not reorder
waves. Wave 4 (Flutter-web) trails and is gated on wave 2.

## Read first (in this order)

1. `CLAUDE.md` (root) — crate map, verbs, conventions. Hexagonal deps are
   review-failing constraints. `unsafe` is forbidden workspace-wide.
2. `docs/plans/nostos-unified-api-contract.md` — the contract you are implementing.
   The verb matrix and semantics there are **ratified**; do not redesign them.
3. ADRs 0024 (Collection<T>), 0027 (write-outcome/dead-letter), 0028 (views over
   opaque payload — typed tables REJECTED), 0017 + addendum (web live-only,
   IndexedDB rejected), 0029 (sign-out), 0013 (write-back).
4. `docs/api/flutter.md` and `docs/api/web.md` — current documented surfaces.
   `scripts/check-doc-signatures.py` must exit 0 after your changes.

## Ground rules (all waves)

- **Scope:** nothing outside the nostos tree. Plans/docs only in `docs/plans/`,
  ADRs in `docs/adr/` (next free number; 0032 is reserved for the contract ADR).
- **Gate:** `make ci` green before claiming done. For server/replication work:
  real-PG e2e via `docker compose -f docker/docker-compose.yml up -d` then
  `NOSTOS_E2E_PG=1 NOSTOS_PG_URL=postgres://cairn:cairn@localhost:5433/cairn cargo test -p nostos-infra --features pg`.
  **Without `NOSTOS_E2E_PG=1` those tests self-skip and report a false-positive
  pass** — never cite a skipped run as evidence.
- **Commits:** single line, conventional prefix, no author mentions.
- Deliberate shortcuts carry a `ponytail:` comment naming the ceiling and
  upgrade path.
- Never `git add -A` (a deleted-but-uncommitted `fixtures/` dir exists by
  design). `sdk/nostos_swift/swift-sources/` is a gitignored regen artifact.
- Perf-relevant changes ship with before/after numbers (`make bench`) or get
  reverted. The wire protocol stays human-debuggable JSON.
- Every claim in your completion report needs a command + observed output.
  Defects hide where no test runs — new surface ⇒ new test.

---

## Wave 1 — Flutter core-contract port + pause/resume + atlet de-SQL

**Serves:** Flutter native. **Depends on:** nothing. Start here.

### 1.1 Typed verb surface in `sdk/nostos_flutter`

Files: `lib/src/nostos_database.dart`, `lib/src/schema.dart`, new
`lib/src/collection.dart` (+ exports in `lib/nostos_flutter.dart`).

- Implement the contract's `Collection<T>` verbs on top of the existing
  `NostosDatabase` (ADR-0028: **SQLite views over `cairn_data`, never
  materialized typed tables**):
  - Reads: `get(pk)`, `getAll()`, `fetchById`, `watch(...)`, `watchOne(...)`,
    `waitForFirstSync()`.
  - Predicates as **data**, not strings: `where:` eq/lt/gt/in + and/or,
    `orderBy:` field+direction, `limit`/`offset` — per the contract's
    structured-predicate section. Compile to SQL against the view internally.
  - Writes: `upsert`, `update` (per-field collapsed-write, ADR-0014 tiers),
    `delete`, `writeBatch`, `transaction` — collapsed-write semantics through
    the existing outbox; **no new write path**.
  - Write-outcome surface per ADR-0027: `pendingWrites`, `deadLetteredWrites`,
    `deadLetters` list, `retryDeadLetter(id)`, `discardDeadLetter(id)`,
    `lastWriteError` — DEAD-LETTER-only surfacing; do not invent per-write acks.
  - Keep `execute()` / `watchSql()` as the documented escape hatch. Demote in
    docs; do not remove.
- `pauseSync()` / `resumeSync()` on the top-level client: disconnect/connect
  that retains token, schema, and watch subscriptions; watches re-emit on
  resume without caller re-wiring. No wire-protocol change.

### 1.2 atlet de-SQL

`apps/atlet/flutter/` — replace raw `watch('SELECT ...')` usage in
`nostos_adapter.dart` (and friends) with the typed verbs. atlet is the
acceptance fixture: if a needed verb is missing, that's a contract gap —
**stop and report it**, don't ad-hoc extend the surface.

### 1.3 Docs + ADR

- Update `docs/api/flutter.md`; `scripts/check-doc-signatures.py` exits 0.
- Write `docs/adr/0032-unified-api-contract.md` citing the contract doc.

### Wave-1 acceptance

- `make ci` green; `flutter test` green in `sdk/nostos_flutter` **and**
  `apps/atlet/flutter` (the cargo-fallback hook makes `flutter test` work for
  the SDK pkg).
- `make sdk-e2e` still green (Flutter is wired into it).
- Grep proof: no `SELECT` strings left in atlet app code outside the adapter's
  escape-hatch boundary.

---

## Wave 2 — Browser-durable Storage in nostos-core (reopens ADR-0017)

**Serves:** JS web now, Flutter-web later. **Depends on:** nothing in wave 1.

### 2.1 ADR first

ADR-0017 already DECIDED the backend: **official SQLite-WASM with the
`opfs-sahpool` VFS** (option 1), explicitly rejecting wa-sqlite/`OPFSCoopSyncVFS`
(option 2) for its COOP/COEP deployment tax. (The prior text here named wa-sqlite
— a misread of the ADR; corrected 2026-08-08 after operator review.) Do NOT
re-litigate the backend choice and do NOT add COOP/COEP headers — header-free
deployment is a *requirement* of the chosen path (ADR-0017).

Write ADR-0033 as the **execution** ADR for ADR-0017's follow-up scope (NOT a
supersession of its decision): the concrete Worker architecture —
`SqliteWasmStorage` impl of `Storage`+`Outbox` mirroring `SqliteStorage`'s
schema, the `postMessage` protocol (decide serde-JSON vs transferable
`ArrayBuffer`), the Playwright browser harness, the degrade path, and sign-out
wiping OPFS + the localStorage checkpoint. Update ADR-0017's Status line to
point at ADR-0033 as the in-progress follow-up. Must cover:
- **No** SharedArrayBuffer / cross-origin-isolation / COOP+COOP requirement —
  `opfs-sahpool` uses synchronous `FileSystemSyncAccessHandle` writes. Document
  header-free deployment as a feature, not a tax.
- Safari Private Browsing (OPFS disallowed): **graceful degrade to the
  in-memory backend + localStorage checkpoint** (today's behavior), surfaced on
  `SyncStatus`, not a crash.

### 2.2 Implementation shape

- `nostos-core` stays WASM-clean (no tokio, no SQLite deps). The durable
  backend implements the existing `Storage` **trait boundary**; the
  OPFS/wa-sqlite binding lives on the wasm side (`nostos-ffi-wasm` /
  `sdk/nostos_web`), not in core.
- Bring web up to the core contract: durable local rows, **browser outbox**
  (T3 writes offline → replay on reconnect), dead-letter surface (ADR-0027),
  resume from durable checkpoint instead of localStorage-lsn-only.
- Sign-out (ADR-0029): wipe must now clear OPFS state too.

### Wave-2 acceptance

- Browser-run test (Playwright/headless Chrome — NOT Node: `FileSystemSyncAccessHandle` is Worker+browser-only): write offline → kill
  page → reload → reconnect → server receives the write; and the in-memory degrade
  path exercised.
- `make ci` green; real-PG e2e green (with `NOSTOS_E2E_PG=1`, see ground rules).
- `docs/api/web.md` updated; ADR-0017 status line updated to point at ADR-0033
  (the execution follow-up — NOT a supersession of ADR-0017's opfs-sahpool decision).

---

## Wave 3 — T6 Attachments (nostos-core queue + BYO adapter)

**Serves:** Flutter native immediately; web once wave 2 lands.
**Depends on:** wave 1 (Flutter surface); web half depends on wave 2.

### 3.1 Core queue (`nostos-core`, WASM-clean, once)

- Attachments metadata table synced as a normal table (server needs it in
  `NOSTOS_WRITE_TABLES` — empty-default gate, ADR-0013; document this loudly,
  it's the #1 foot-gun).
- State machine `QUEUED_UPLOAD | QUEUED_DOWNLOAD | QUEUED_DELETE | SYNCED |
  ARCHIVED`; transitions ride the existing outbox/apply engine; retry with
  backoff; ordering guarantee: blob upload completes before the referencing
  row's write is released (or document the chosen weaker ordering in the ADR).
- **No server changes for blob bytes. Blobs never transit the Nostos server.**

### 3.2 Adapters + surfaces

- `AttachmentStorageAdapter` interface in Dart and TS:
  `upload(path, bytes, mediaType)` / `download(path)` / `delete(path)`.
- First-class `SupabaseStorageAdapter` in both languages.
- Local blob store: filesystem (path_provider) on Flutter; OPFS dir on web.
- Sign-out wipes local blobs (ADR-0029 consistency).

### Wave-3 acceptance

- e2e in atlet (or a fixture app): pick image offline → queued → reconnect →
  uploaded to Supabase Storage → second client downloads. Dead-letter path
  tested (adapter throws → surfaced, retryable).
- `make ci` + `flutter test` green; docs for both SDKs updated + signature
  check exit 0.

---

## Wave 4 (trailing) — Flutter-web binding

**Gated on wave 2. Do not start before wave 2 review passes.**

- Same Dart API, second engine path: nostos-core WASM (the wave-2 backend)
  behind the existing `Engine` abstraction in `lib/src/engine.dart`, selected
  by platform (conditional import). flutter_rust_bridge native path unchanged.
- Acceptance: atlet compiled to web runs the wave-1 typed surface + wave-3
  attachments in a browser; degrade path on Safari Private Browsing.

---

## Known traps (read before debugging)

- Real-PG suite leaks `e2e_%` replication slots → `ControlPlane "db error"`
  at ensure_connected. Prune inactive slots via
  `docker exec nostos-postgres psql ...` (PG max bumped to 20).
- `cmd | grep -q` under pipefail returns 141 on successful match with large
  output — use `[[ ]]` or a here-string in harness scripts.
- Android sdk-e2e: boot emulator on port **5556** (5554 deadlocks the kotlin
  harness).
- Supabase direct DB is IPv6-only (AAAA); dev VPNs drop IPv6 — use
  `scripts/warp-ipv6-egress.sh` (127.0.0.1:15433, sslmode=disable).
- `NOSTOS_REPLICATOR` must be `pg` or the snapshotter is None and "nothing
  shows" — a config bug, not a sync bug.

## Reporting format (per wave, for tech-lead review)

1. Commits (hashes + one-line messages).
2. Verification evidence: exact commands + pass/fail counts
   (`make ci`, `flutter test`, real-PG e2e **with** `NOSTOS_E2E_PG=1` shown,
   `check-doc-signatures.py` exit code, browser test output for wave 2/4).
3. Contract gaps hit (verbs missing/ambiguous) — reported, not self-resolved.
4. Deviations from this plan, each with rationale and a `ponytail:` marker if
   it's a shortcut.
5. Anything you did NOT test, stated explicitly.
