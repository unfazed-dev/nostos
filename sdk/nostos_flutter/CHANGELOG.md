## Unreleased

Direct mode (`NostosDatabase.direct`), newest first:

- **ADR-0049 — `keepLocalOnSignOut`** on `NostosDatabase.direct`,
  `Nostos.direct`, `DirectNostosEngine.connect` and `createDirectNostosEngine`
  (default `false`, ADR-0029 wipe unchanged). `true` keeps the rows across
  `signOut()` for the same JWT `sub`; a different user's token wipes before its
  first pull. `NostosDatabase.keepLocalOnSignOut` is public; the T6 blob-store
  sign-out hook skips its wipe when it is `true`.
- **`Attachments.bytes(id)`** — read-through fetch for shared attachments
  (local `BlobStore`, else adapter download, cached). Never touches the
  metadata row, so a public catalog's images need no write RLS and no
  per-device state fan-out; `queueDownload` stays the durable per-user path.
- **Fix: `NostosDatabase.isOnline` self-wires** the status pump. An app that
  never read `status` saw `false` forever, so the T6 driver never moved a
  byte; `bytes()` no longer gates on it at all (a miss at first paint was
  never retried).
- **`watch` withholds the pre-snapshot empty read.** The first emission is the
  first real read, so UIs no longer flash an empty state for the ~2 s
  bootstrap. Streams that expected an immediate `[]` on a fresh store now wait.
- **Bounded PostgREST requests** (10 s connect, 30 s read) and a failed first
  sync retries on the 0.5–30 s backoff instead of waiting for the 60 s floor.
- **gzip on the pull** (7.4× fewer snapshot bytes).
- **Doorbell TLS**: `wss://` Realtime works on iOS (rustls `ring` provider
  compiled in); the reconnect backoff resets after a healthy session.
- **WAL + `busy_timeout=5000`** on the SQLite file so two engines (app + push
  wake isolate) share it.
- `nostos link --mode direct` policy: the Realtime join check slices the topic
  by prefix length, so `nostos:` channels pass RLS after the rename.

## 0.2.0 (2026-09-01)

The first flutter-carrying tag. `0.1.0` below was never published to pub.dev,
and the repo's `v0.1.0` git tag (2026-07-05) predates the entire `sdk/` tree —
**no earlier tag has ever carried this package**. `0.2.0-dev.1` was the
moving-head pin for path-dependency consumers (apps/atlet, arxa clients);
this tag is the cut the dev head pointed at. Pushing it runs the release
pipeline for real (`.github/workflows/release.yml`): CLI/server per-platform
builds, the seven flutter-glue native artifacts, and the
`release-prebuilt-manifest.json` PR that fills `hook/prebuilt.json` — the
zero-Rust-toolchain consumer path (kit plan D3 0c). Publishing to pub.dev
stays an explicit operator step after that PR merges — see the README's
Versioning and Releases sections.

Since the 0.1.0 entry was written (40+ commits), the surface grew to a
superset — highlights, newest first:

- **ADR-0041 D7 — iroh transport, off-default.** `nostos_flutter_rust` gains
  an `iroh` cargo feature (default OFF; prebuilt binaries never carry it).
  `connect(url, …)` was already scheme-agnostic; Dart `NostosConfig` accepts
  the `iroh://` scheme, and without the feature an `iroh://` URL fails loudly
  (`reject_iroh_scheme`). Opt-in at build time via
  `NOSTOS_FLUTTER_CARGO_FEATURES=iroh` (source-build path only).
- **Proxied sync works.** The REST base keeps the sync URL's path prefix, so
  `/schema` + `/push-tokens` stay reachable when sync rides a reverse-proxy
  prefix (the arxa studio tunnel's `/__nostos` leg).
- `NostosDatabase.supabase()` opens sessionless — sync starts at sign-in
  instead of requiring a live session at construction.
- Rejected subscribes surface as fatal errors; `connected` now means PROVEN
  (first frame or write ack), not socket-up.

- `NostosDatabase.local({sqliteDir, schema, …})` — the no-server entry point:
  declared schema + on-device SQLite + durable outbox with the sync loop
  paused before it can dial, so every feature works identically with
  local-only storage. Upgrade to sync by reopening the SAME SQLite file with
  a real `/sync` URL — zero migration. Server-only calls fail loudly:
  `resumeSync` and the push-token REST verbs throw `StateError`, and
  `waitForFirstSync` resolves immediately (there is no first sync).
  `Nostos.withEngine` also grew optional `orSetTables`/`counterTables` so
  fake-engine tests can pin the CRDT tier declarations.
- `syncStream(name, params).subscribe()` — parameterized
  streams on the live session (P5 slice 8); web engine throws
  `UnimplementedError` (native-only v1).
- Flutter-web engine (`WebNostosEngine`) over the shared `nostos-ffi-wasm`
  Worker — conditional-import platform switch (ADR-0036), including
  CRDT verbs + atomic `writeBatch` over `NostosSocket` delegates.
- Two-plane attachment blob sync (ADR-0034) and the PN-Counter CRDT tier
  mirroring the OR-set (ADR-0030 addendum).
- Push-token registration over REST (`registerPushToken` /
  `deregisterPushToken`, ADR-0037) with stale-session self-heal (401 → one
  refresh + retry) and connection-level retry through the iOS local-network
  permission window.
- Unified Wave-1 API: structured predicates, typed reads, atomic
  `writeBatch`, `deadLetters`, CRDT-table config exposure on Flutter.

## 0.1.0

Initial release. Plug-and-play local-first sync for Flutter, backed by
[Nostos](https://github.com/unfazed-dev/nostos): `Nostos.connect`/`subscribe`/`watch`/`write` over
a Rust-owned SQLite + WebSocket sync loop (flutter_rust_bridge native-assets
backend — no codegen, no Xcode/Gradle wiring). Supabase auth pass-through via
`NostosSupabase.connect`. Platforms: macOS verified; iOS/Android build config
present, verified by `.github/workflows/release.yml`'s cross-compile matrix.
Windows/Linux/Web are fast-follow (see README's Platforms table).
