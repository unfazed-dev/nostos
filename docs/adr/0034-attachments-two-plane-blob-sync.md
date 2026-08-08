# ADR-0034: Attachments — two-plane blob sync (T6)

- **Status:** Accepted (Wave 3 shipped 2026-08-08).
- **Date:** 2026-08-08
- **Implements:** T6 of the unified-API contract (`docs/plans/nostos-unified-api-contract.md`
  §Amendment v1.2); execution plan `docs/plans/nostos-unified-api-implementation.md` Wave 3.
- **References:** ADR-0013 (write-back / `NOSTOS_WRITE_TABLES`), ADR-0027 (dead-letter),
  ADR-0028 (read-views over `cairn_data`), ADR-0029 (sign-out wipe), ADR-0033 (browser-durable
  storage — the OPFS blob store builds on this).

## Context

The contract's Tier-1 extension (v1.2, ratified 2026-08-08) adds T6 — attachments — as a
**two-plane** design:

- a **metadata plane** that Nostos syncs (filename, size, media type, lifecycle state), and
- a **blob plane** the developer supplies (their own storage bucket).

The driving constraint is a moat constraint, not a convenience: **blobs must never transit the
Nostos server.** Proxying blob bytes would pollute the fan-out throughput that is Nostos's headline
advantage (833k ops/sec aggregate, 0% drops — `benches/results/RESULTS.md`) and make the server
stateful (it is deliberately stateless beyond the replication slot). PowerSync takes the same
posture with its `AbstractRemoteStorageAdapter`.

The open design questions this ADR resolves:
1. Where does the "queue" live, given `nostos-core` is WASM-clean (no async, no I/O) and the
   blob adapter is inherently async + platform-specific?
2. How strong is the ordering guarantee between a blob transfer and a referencing business row?
3. How are adapter failures surfaced (dead-letter parity with ADR-0027)?

## Decision

### 1. Pure state machine in `nostos-core`; per-SDK driver loops

The **one** shared artifact in `nostos-core` is a pure state machine
(`crates/nostos-core/src/attachments.rs`): the `AttachmentState` enum
(`QueuedUpload | QueuedDownload | QueuedDelete | Synced | Archived`), its wire strings, the
`on_success` transition, a backoff schedule, and the `attachments` table's column-name constants.
It is pure Rust — no tokio, no async, no trait that the SDKs implement. It compiles to WASM
unchanged, like the rest of `nostos-core`.

The **driver loops** that actually call `upload`/`download`/`delete` on the adapter live
**per-SDK**: in Dart (`sdk/nostos_flutter/lib/src/attachments.dart`) and in TS
(`sdk/nostos_web`). This is forced by two facts the contract itself encodes:
- the adapter is async + platform-specific (`supabase_flutter` / `@supabase/supabase-js`), and
- the contract places `AttachmentStorageAdapter` "in Dart AND TS" — not Rust.

An adapter trait in `nostos-core` would either pull `async` into a WASM-clean crate or force a
synchronous trait that misrepresents blob I/O. So "the queue lives once in nostos-core" is
satisfied by the state machine + the wire-string contract; the drivers are the ports. The
duplication across Dart/TS is shallow (five string constants + a transition table over pure
data), and the `nostos-core` unit tests guard the round-trip that both drivers duplicate.

This was ratified by an architecture advisor consult (GLM-5.2, HIGH confidence) before
implementation: "the state machine IS the queue contract; adapters are drivers, not
implementations of a core interface."

### 2. Metadata plane is a NORMAL synced table

The `attachments` table (`id, filename, size, media_type, state, timestamp`) is synced through
the ordinary replication/outbox path — no new wire frame, no new write op, no special-case in
the apply engine. State transitions are ordinary `patch` writes on the `state` column through
the collapsed outbox (ADR-0013).

**`NOSTOS_WRITE_TABLES` — the #1 foot-gun (loudly documented).** Because the metadata table is
writable through the same collapsed outbox as any business table, the server's empty-default
allowlist (`NOSTOS_WRITE_TABLES`, ADR-0013) **MUST include `attachments`**. A forgotten entry
surfaces as the transport's loud
_"table not writable: 'attachments' — add it to NOSTOS_WRITE_TABLES"_ error
(`crates/nostos-infra/src/transport.rs`). The SDK READMEs and `docs/api/{flutter,web}.md`
document this in the setup steps; it is the single most likely misconfiguration.

### 3. Weaker ordering — the metadata state IS the synchronization primitive

v1 ships **weaker ordering**, not strong cross-row ordering:

- An attachment's metadata row reaches `state = synced` **only after** the blob upload confirms
  on the remote bucket. This is a within-row guarantee and it is structural (the driver patches
  `state` to `synced` in the success branch, never speculatively).
- Cross-row ordering — "a `tasks` row referencing `attachment_id` must not reach the server
  until the blob is uploaded" — is **NOT** enforced by Nostos. It is the app's responsibility,
  expressed by reading the attachment's `state` reactively (`watch`) and gating the referencing
  UI on `synced`.

**Rationale:** the outbox is a flat FIFO per row; a cross-row dependency (hold the `tasks`
write until the attachment is `synced`) would require an outbox dependency mechanism — a real
design change that trades the simple, crash-safe flat queue for a benefit that the common case
does not need. The metadata `state` is already a sufficient coordination primitive for an app
that wants the gate (it reads `state` and decides). This matches PowerSync's
`AbstractRemoteStorageAdapter`, where the upload is async and the app coordinates.

**Upgrade path (not v1):** if a measurement shows user-visible races from concurrent
upload/download of related attachments (the advisor's MEDIUM risk), a per-row "dependencies"
field on the outbox `PendingWrite` could hold the referencing write until its target row
reaches `synced`. That change is local to the outbox + flush loop; the state machine and wire
protocol are unaffected.

### 4. State machine + dead-letter

```
  QueuedUpload   ──adapter ok──►  Synced
  QueuedDownload ──adapter ok──►  Synced
  QueuedDelete   ──adapter ok──►  Archived   (blob gone; metadata tombstone)
  any Queued*    ──retries exhausted──►  Archived  (dead-letter; local error)
  Synced | Archived  ──►  terminal (idle)
```

- The driver retries failed adapter calls on an exponential backoff
  (`nostos_core::attachments::retry_after_ms`: 1s, 2s, 4s, … capped 60s).
- After `max_attempts` (default 5) the row is flipped to `Archived` and the last adapter error
  is surfaced locally (`Attachments.lastErrorFor(id)` in Dart; the analogous field in TS).
  This is dead-letter parity with ADR-0027: the row leaves the active queue (so it doesn't
  block the head) but is NOT deleted — it stays for inspection/replay. Re-queueing an
  `Archived` row is an explicit app action (patch `state` back to a `Queued*` value); the
  driver never auto-revives a dead-letter.
- The attempt count + last error are **driver-owned LOCAL state, NOT a synced column.** Retry
  timing and conditions are per-device; a process restart resets attempts, which is acceptable
  because the metadata row stays `Queued*` across a restart (its state is synced) and the driver
  simply retries fresh. `ponytail:` in `attachments.rs`/`attachments.dart` marks this ceiling.

### 5. Blob plane — `AttachmentStorageAdapter` + local blob store

- `AttachmentStorageAdapter` (`upload(path, bytes, mediaType)`, `download(path)`,
  `delete(path)`) is defined in Dart AND TS. Methods MUST be idempotent under retry: the driver
  may re-dispatch after a network blip, and `delete` on a path that is already gone MUST
  succeed so a `queued_delete` converges.
- A first-class `SupabaseStorageAdapter` ships in both languages (Supabase Storage, `upsert:
  true` for upload idempotency).
- A **local blob cache** holds bytes the app picked (pending upload) or fetched (after
  download): the filesystem via `path_provider` on Flutter (`LocalFileBlobStore`); an OPFS dir
  on web (building on Wave 2's durable storage, ADR-0033). `nostos_flutter` deliberately does
  NOT depend on `path_provider` — the app passes the directory in.

### 6. Sign-out wipes local blobs (ADR-0029 consistency)

`NostosDatabase.signOut()` now awaits registered `_signOutHooks` AFTER the core engine + outbox
wipe. The `Attachments` driver registers its `BlobStore.wipe` there, so a sign-out / principal
switch leaves no blob bytes for the next principal — consistent with the SQLite + outbox wipe.
Hooks are best-effort (a failing wipe is swallowed) so one cannot block the core sign-out.

## Consequences

- **Positive — moat preserved.** Zero server changes for blob bytes; the fan-out path is
  untouched. The headline throughput claim is unaffected by T6.
- **Positive — metadata plane is free.** No new wire frame, no new apply path; the existing
  replication + outbox + read-views carry the metadata. Offline-first works automatically
  (state writes queue in the durable outbox).
- **Positive — PowerSync-parity feature.** `AttachmentStorageAdapter` mirrors
  `AbstractRemoteStorageAdapter`; `SupabaseStorageAdapter` is the zero-config default for the
  taught Supabase path.
- **Negative — weaker cross-row ordering.** Documented above with the upgrade path. Apps that
  need the gate read `state` reactively.
- **Negative — driver logic duplicated across Dart and TS.** Mitigated by the shared
  `nostos-core` state machine + a shared test matrix (below). The duplication is shallow.
- **Negative — `NOSTOS_WRITE_TABLES` configuration step.** A new allowlist entry the operator
  must not forget; mitigated by the loud transport error + docs.

## The test that matters

Two clients share a bucket. Client A picks a blob **offline** → the metadata row is
`queued_upload` and the bytes are cached locally → A reconnects → the driver uploads →
`state = synced` → client B (which received the synced metadata row via replication) queues a
download → the driver fetches the bytes into B's local cache. Separately, an adapter that
throws drives a row through retry into `Archived` with the error surfaced. And a sign-out
mid-session wipes the local blob cache so the next principal sees nothing.

The Wave-3 Flutter test (`sdk/nostos_flutter/test/attachments_test.dart`) exercises this
end-to-end against a shared in-memory fake adapter (standing in for Supabase Storage), so the
full queue→reconnect→upload→second-client-download→dead-letter→sign-out-wipe path is covered
without a live server or bucket. The **real Supabase-Storage round-trip is the headline proof
and is called out as untested-environment** in the wave report: no Supabase project is
configured in this worktree, so the live bucket path is exercised only through the
`SupabaseStorageAdapter` code path compiling against `supabase_flutter`, not through a live
upload/download. An operator with a project configured should run that round-trip before
shipping.

## Advisor followups (codified)

- **Shared test matrix** (HIGH risk: driver divergence): both SDK drivers MUST pass the same
  scenarios — `queued → synced` (upload + download), `queued_delete → archived`, retry-then-
  dead-letter, sign-out-mid-session wipes blobs. The Flutter suite covers all five; the web
  suite covers the same set against its own fake adapter.
- **Weaker-ordering scope** codified in §3 above (the explicit boundary + upgrade path).
- **signOut wipe race** (MEDIUM): hooks run AFTER the engine quiesces + wipes, so a hook never
  sees in-flight apply frames; an in-flight adapter call at sign-out time completes or fails
  against the (now-wiped) local state and its state patch is a no-op on the wiped store. This
  is acceptable for v1; a cancel-the-in-flight-transfer signal is a future enhancement.
