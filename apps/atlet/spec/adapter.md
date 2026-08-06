# SyncAdapter contract v0 (frozen at pilot retro)

One adapter per (SDK, engine). The app and bench runner speak ONLY this surface.

## Operations
- `init(config)` — open local DB + connect engine using the signed-in Supabase
  session's access token. Idempotent.
- `signOut()` — disconnect + FULL local wipe (engine DB files deleted).
- `addSession(session) -> id` / `updateSession` / `deleteSession`
- `watchSessions() -> stream of ordered session lists` (normal read path)
- `watchProducts() -> stream` (read-only bulk table)
- `syncStatus() -> stream` (connected / syncing / offline, engine's own notion)
- `setConnected(bool)` — engine-level offline toggle for queue-drain runs.

## Instrumentation marks (externally observable ONLY)
A mark is legal iff it is derived from data visible through the adapter's normal
read path. Engine-internal callbacks/private state are forbidden (fairness rule).
- `localVisible(rowId, tMono)` — row first appears in `watchSessions` output.
- `serverAcked(rowId, tMono)` — the row's `server_committed_at` becomes non-null
  in `watchSessions` output (client inserts it as null; the server default fills
  it; the value syncs back — full round trip, engine-neutral).
- `remoteVisible(rowId, tMono, serverCommittedAt)` — a row created by the
  harness (via PostgREST) appears in `watchSessions` output.

## Conformance checklist (run per implementation)
1. init → signIn → addSession → serverAcked mark fires < 60s.
2. Row inserted via PostgREST appears (remoteVisible) < 60s.
3. setConnected(false) → 25 writes → setConnected(true) → all 25 serverAcked.
4. signOut wipes: local DB files absent; re-init cold-syncs from zero.
5. No adapter API leaks engine types into the app/bench layer.
