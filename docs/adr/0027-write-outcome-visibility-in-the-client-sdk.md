# ADR-0027: Write-outcome visibility — dead-letter-only error surfacing

**Status:** Accepted · **Date:** 2026-07-29

Extends ADR-0024 (client reactive facade), which introduced `SyncStatus` with a
deliberately minimal `{conn, lastSyncedAt}` surface and deferred richer fields to
"P1 with engine-side signals." This is that follow-up. Write semantics are
ADR-0013 (direct write-back + durable outbox, v2 dead-lettering).

## Context

`SyncClient::write` returns as soon as the write is durable in the local outbox —
that is the whole point of the outbox (a user action is captured the instant it
happens, network or not). The Dart `db.write` correspondingly returns "the local
outbox id (NOT a server ack)."

What happened *after* that was invisible. The server rejects unwritable tables
loudly and actionably (`nostos-infra/src/transport.rs:786` names the exact
`NOSTOS_WRITE_TABLES` value to set), and the client retried, counted attempts, and
eventually quarantined the write — but every one of those outcomes terminated in a
`tracing::warn!`. `nostos-client/src/client.rs:32` recorded the gap in its own
module docs: *"the user-facing surface is a Phase-2 concern."*

The consequence is not a missing convenience. **No Nostos app could tell its user
that a write was lost.** The data was still on disk and inspectable, but nothing
above the Rust client could observe it, so no UI could react.

This also made Flutter's *own* documented optimistic-state pattern
unimplementable. That pattern sets state optimistically and reverts inside a
`catch`:

```dart
subscribed = true;
try { await repository.subscribe(); }
catch (e) { subscribed = false; error = true; }
```

On Nostos there is no `catch` to revert in — the write already succeeded locally
and the failure arrives later, out of band. The platform vendor's blessed pattern
had no expression in our SDK. That is a stronger argument than any competitive
comparison, because it is not a matter of taste.

## Decision

Publish outbox state from the engine and fold it into `SyncStatus`.

### 1. `WriteQueueStatus` on a `watch` channel (`crates/nostos-client`)

`SyncClient` gains `tokio::sync::watch::Sender<WriteQueueStatus>` carrying
`{pending, dead_lettered, last_error}`, exposed via `subscribe_write_status()` /
`write_status()`.

`watch`, not `broadcast` (which `changes` uses): a status readout must show the
*current* value to a subscriber that arrives late — a status widget built after
connect still has to render "3 pending" — and coalescing intermediate values is
correct for a status readout in a way it is not for change ticks.

`pending` is seeded from `Outbox::pending()` at construction so writes made in a
previous process are counted after a restart. Enqueue increments by one (always a
new row, so `+1` is exact); **ack and dead-letter re-count from storage** rather
than decrementing, because both operations are idempotent and the flush loop
documents ack redelivery after a partial flush as a real path. A naive `-= 1`
double-counts a redelivered ack and reports "0 unsynced" while writes are still
queued — a silent undercount, which is worse than showing nothing. The re-count
runs inside the existing `spawn_blocking` under the same lock, so it costs one
query per server response, never one per keystroke.

### 2. `last_error` is set ONLY on dead-letter — the load-bearing choice

A `WriteResult{ok:false}` is *not* an error worth showing. `client.rs:172`
already documents these as routinely transient — a constraint violation racing a
concurrent write — and the flush loop retries them to
`dead_letter_max_attempts`. Surfacing every rejection would make apps display
scary, self-resolving errors, which trains users to dismiss write errors and
makes the signal worth less than silence.

The user-actionable event is the **dead-letter**: the write has permanently
failed and left the send queue. That is the moment a human must be told, and it
is the only moment `last_error` is set.

Equally, `pending > 0` is deliberately **not** an error. It is the offline-first
promise working, and the SDK docs say to render it as "N unsynced changes."

### 3. `SyncStatus` gains five fields and four derived getters

`pendingWrites`, `deadLetteredWrites`, `lastWriteError` (state) plus
`hasWriteError`, `hasPendingWrites`, `uploading`, `hasSynced` (derived — no new
state). `NostosDatabase` folds two independent streams (connection + outbox) into
the one `ValueListenable<SyncStatus>`.

We stopped well short of a twelve-member `SyncStatus` some sync engines ship. Superhuman's
offline design collapses *all* network failure into a single "offline" state on
the grounds of "fewer states, fewer code paths," and that restraint is worth
respecting: each extra state is a branch every consuming app must handle.

## Consequences

- **Positive:** an app can now render "2 unsynced changes" and "Change not saved:
  `<server reason>`" — and the server's reason arrives verbatim, so an allowlist
  rejection reaches the developer naming the env var to set.
- **Positive:** the engine already held every fact involved. Nothing new is
  computed or persisted; this is purely a publishing change.
- **Positive — guard tests.** `only_dead_letter_surfaces_a_write_error`
  (`client.rs`) asserts the *intermediate* state: rejections 1–2 leave
  `last_error` `None`. Asserting only the final state would pass even if every
  rejection set the error, i.e. exactly the inversion this ADR exists to
  prevent. Note it must use `SqliteStorage`, not `InMemoryStorage` —
  `Outbox::bump_attempts` has a default impl returning `Ok(0)`, so on
  `InMemoryStorage` the dead-letter branch is unreachable and the test passes
  vacuously (it did, until it was caught). Three Dart tests cover the fold,
  including that a connection change does not wipe the write fields.
- **Negative — `dead_lettered` is session-scoped.** It counts this process's
  dead-letters and resets on restart, because `dead_letter_entries()` is on
  `SqliteStorage` rather than the `Outbox` trait and is not reachable
  generically. `pending` does survive restarts. Seed it from storage if this
  matters; the count is a UI hint, and the entries themselves are never deleted.
- **Negative — the outbox pump attaches at the LATER of first `status` access
  and first `subscribe`, never eagerly.** Two reasons. (1) Precondition:
  `watchWriteStatus()` errors without an active subscription, while
  `connectionState` never did — reading `status` before subscribing is
  legitimate (you get the honest `disconnected` default), so attaching at
  either point alone turns a valid order of calls into a stream error.
  (2) Cost: apps that never read `status` never pay for the FFI stream. The
  attach cancels any prior pump (re-subscribe doesn't orphan) and swallows
  stream errors (a dead pump can't take the connection half of `SyncStatus`
  down with it).
- **Finding (pre-existing, surfaced while verifying this): the zero-setup
  fake-replicator environment saturates any session that outlives a few
  seconds.** `nostos-server`'s fake branch emits `u64::MAX` events with no
  pacing (`main.rs` `FakeReplicatorConfig::small(u64::MAX)`; `fake.rs` has no
  rate knob), and each `watch` pump re-queries the FULL, monotonically growing
  table per change tick — quadratic. Native stack sampling of a wedged run
  (1,717 samples) showed ~80% of a worker in `emit_snapshot` closures plus
  hex/JSON/SSE encoding and a Dart GC storm, and **zero** frames in this ADR's
  pump — the write-status feature contributes one message and then parks. The
  SDK's integration test survives the environment by finishing in ~3s; during
  verification, eagerly attaching the pump in `subscribe` added just enough
  startup latency to lose that race reproducibly, which is what motivated the
  later-of rule above. **FIXED (A10, 2026-07-30):** `FakeReplicatorConfig` gained
  `paced(events_per_sec)` and `recycling_keys(n)` (both `0` = today's unbounded
  behaviour, so `nostos-bench` and the moat numbers are untouched), and
  `nostos-server`'s fake branches now default to 20 events/sec over 50 distinct
  keys (`NOSTOS_FAKE_EPS` / `NOSTOS_FAKE_KEYS`). Key recycling is the load-bearing
  half: client apply is an upsert on `(table, pk)`, so a bounded key space bounds
  the *table*, which makes the full-table watch snapshot O(1) in session length.
  Pacing alone would only slow the quadratic growth. Measured — **server-side
  generator load only, no client attached** — 10 s: old default **100% CPU** (a
  full core spinning on fan-out with zero consumers) → new default **0.0%**. That
  is a distinct defect from the client-side saturation diagnosed above (per-tick
  `emit_snapshot` + hex/JSON/SSE + Dart GC), which was **not** re-measured in this
  pass; the O(1)-snapshot claim follows from the upsert + bounded key space, not
  from a device run. See the A10 section of
  `docs/plans/nostos-completion-assessment-2026-07-29.md`.
- **Negative — regenerating the bridge invalidates cached native libs.** After
  `flutter_rust_bridge_codegen generate`, a stale `.dylib` fails at runtime with
  a `_sanityCheckContentHash` mismatch, not at compile time. `flutter clean &&
  flutter pub get` in `example/` clears it; noted at the flutter slice in
  `scripts/sdk-e2e.sh`.
- **Negative — added surface on `NostosEngine`.** `watchWriteStatus()` is a new
  abstract member, so every fake implements it (four in-tree did). Deliberate:
  the engine seam is what lets the Dart tests run without FFI.
- **Neutral — `lastSyncedAt` is still a proxy** stamped on each `connected`
  transition. The download path still has no "sync completed" signal; only the
  write half stopped guessing here.
