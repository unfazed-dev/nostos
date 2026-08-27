## Unreleased (0.2.0-dev.1)

Moving-head pre-release. `0.1.0` below was never published to pub.dev (the
repo has no GitHub remote; the release pipeline has never run), and the repo's
`v0.1.0` git tag (2026-07-05) predates the entire `sdk/` tree — **no tag has
ever carried this package**. This version number exists so path-dependency
consumers (apps/atlet, upcoming arxa clients) can pin an unambiguous head.
The first flutter-carrying tag (`v0.2.0`) is cut when the release pipeline
runs for real — see the README's Versioning and Releases sections.

Since the 0.1.0 entry was written (40+ commits), the surface grew to a
superset — highlights, newest first:

- `NostosDatabase.local({sqliteDir, schema, …})` — the no-server entry point:
  declared schema + on-device SQLite + durable outbox with the sync loop
  paused before it can dial, so every feature works identically with
  local-only storage. Upgrade to sync by reopening the SAME SQLite file with
  a real `/sync` URL — zero migration. Server-only calls fail loudly:
  `resumeSync` and the push-token REST verbs throw `StateError`, and
  `waitForFirstSync` resolves immediately (there is no first sync).
  `Nostos.withEngine` also grew optional `orSetTables`/`counterTables` so
  fake-engine tests can pin the CRDT tier declarations.
- `syncStream(name, params).subscribe()` — PowerSync-shaped parameterized
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
