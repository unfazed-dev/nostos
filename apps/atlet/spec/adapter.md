# SyncAdapter contract v1.1 (frozen 2026-08-06 at pilot sign-off/retro,
# corrected 2026-08-06 in fix round 1 of the same task's review — see
# `conformance-flutter.md` for the results, retro, and fix-round history that
# produced this version. No further amendments without a new version number
# and a new dated entry there.)

One adapter per (SDK, engine). The app and bench runner speak ONLY this surface.

## Operations
- `init(config)` — open local DB + connect engine using the signed-in Supabase
  session's access token. Idempotent.
- `signOut()` — disconnect + FULL local wipe (engine DB files deleted).
- `addSession(session) -> id` / `deleteSession` (**v1.1 correction** — v1
  also listed `updateSession` here. It was never implemented in `SyncAdapter`,
  `NostosAdapter`, or `PowerSyncAdapter` — the pilot's UI has no session-edit
  flow, so nothing ever called for it. Dropped rather than retained as
  "reserved": an unimplemented operation on a frozen contract is exactly the
  defect class this spec exists to prevent, and re-adding it later needs its
  own version bump and its own reason, not a standing placeholder.)
- `watchSessions() -> stream of ordered session lists` (normal read path)
- `watchProducts() -> stream` (read-only bulk table)
- `connected -> Stream<bool>` (engine-level connectivity signal; **v1
  correction** — v0 specified a three-state `syncStatus()` stream
  (connected/syncing/offline). The interface Task 7 actually froze is a
  boolean `connected` stream; every adapter, the engine toggle, and the
  bench harness are built against the boolean. This line now matches the
  shipped code instead of the other way around.)
- `setConnected(bool)` — engine-level offline toggle for queue-drain runs.

## Instrumentation marks (externally observable ONLY)
A mark is legal iff it is derived from data visible through the adapter's normal
read path. Engine-internal callbacks/private state are forbidden (fairness rule).
Mark derivation itself (`MarkDeriver`, `lib/bench/marks.dart`) is **one shared,
audited implementation used unmodified by every adapter** — adapter-specific
responsibility is limited to correct read-path sequencing (populate the
in-flight id bookkeeping before the underlying write resolves). Fairness
between engines depends on this: no adapter reimplements or is trusted to
reimplement the ordering logic below.
- `localVisible(rowId, tMono)` — row first appears in `watchSessions` output.
- `serverAcked(rowId, tMono)` — the row's `server_committed_at` becomes non-null
  in `watchSessions` output (client inserts it as null; the server default fills
  it; the value syncs back — full round trip, engine-neutral).
- `remoteVisible(rowId, tMono, serverCommittedAt)` — a row created by the
  harness (via PostgREST) appears in `watchSessions` output.

## Conformance checklist (run per implementation)
**Prerequisites (v1 addition):** items 1–4 require a live, operator-provisioned
environment — a real Supabase project, `apps/atlet/services/.env` filled from
`.env.example`, the `docker-compose.atlet.yml` stack up, and a signed-in test
session. This is explicitly **not** in scope for any implementation task by
default; six tasks across the pilot (T6, T9, T10, T12, T14, T15) independently
lacked it, and the pilot's own conformance sign-off (`conformance-flutter.md`)
still lacked it. A task assigning live conformance work must say so and must
name which of the 5 items it covers.

"Run per implementation" means **every item, for every adapter**, tracked
against one running scorecard (`conformance-flutter.md`) — not whichever
subset an individual task's brief happened to request. (v0's silence on this
point let item 2 go unassigned to NostosAdapter and item 3 go unassigned to
both adapters, undetected until sign-off — see the retro.)

1. init → signIn → addSession → serverAcked mark fires < 60s.
2. Row inserted via PostgREST appears (remoteVisible) < 60s.
3. setConnected(false) → 25 writes → setConnected(true) → all 25 serverAcked.
4. signOut wipes: local DB files absent; re-init cold-syncs from zero.
5. No adapter API leaks engine types into the app/bench layer.
