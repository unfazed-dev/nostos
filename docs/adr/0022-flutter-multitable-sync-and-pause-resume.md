# ADR-0022: Flutter multi-table sync per handle and real pause/resume

- **Status:** Accepted
- **Date:** 2026-07-15
- **Supersedes / refines:** ADR-0009 (per-session ack → one global client checkpoint),
  ADR-0013 (collapsed-write outbox), ADR-0019 (typed payload mapping),
  ADR-0021 (client schema discovery over REST)
- **Tracks:** WS5 (multi-table), P2–P4 of the Provider Dashboard launch

## Context

The Flutter SDK (`sdk/nostos_flutter`) shipped single-table-per-`Nostos` first. The
Provider Dashboard launch needs **N tables on one connection** (providers, clients,
availabilities, appointments, invoices) plus the ability to **pause and resume
syncing** while the app stays fully usable offline — the headline offline-first
contract this class of app needs.

Three constraints shaped the design:

1. **One global checkpoint (ADR-0009).** Ack is per-session-sink, but the client
   maintains ONE resume LSN + ONE checkpoint across the whole WAL feed. A
   per-table LSN does not structurally exist, so N tables MUST share one ack
   stream — they cannot be N independent sessions each with their own LSN.
2. **Snapshot LSNs are synthetic.** `PgSnapshotter` stamps snapshot rows with
   `base_lsn + 1 + i` *per table*. On a shared sink this collides across tables
   and the router's dedup ring drops the second table's snapshot as duplicates.
3. **The client must stay usable across a pause.** `disconnect()` is only useful
   if reads, writes (→ durable outbox), and the UI keep working while the `/sync`
   loop is stopped. That requires the client + storage to survive task abort.

The dashboard plan (`docs/plans/nostos-provider-dashboard-multitable.md`) proposed
a sharper "D5" API surface (`watchOf` / `insert` / `update` / a `writes` event
stream / a unified `Nostos` class / `connect()`). That surface is **proposed and
gated on a separate sign-off** (the connection-redesign); it was not authorized
for this launch. This ADR records what was actually built and ratified.

## Decision

### D1 — Multi-table per handle over ONE `/sync` socket (drop the wire-version gate)

One socket → ONE shared `Arc<TokioEventSink>` (one mpsc channel, one
`acked_lsn`, one writer task — ADR-0009's single global checkpoint) → **N
single-predicate `SyncSession`s** registered against it.

- `candidates_for` is table-indexed, so there is no cross-talk between tables.
- `min_acked_lsn` folds the shared sink's one checkpoint across the N sessions.
- The client surface is additive: `SyncClientConfig.extra_tables: Vec<TableSub>`
  (primary `table` + extras), sent as one `Subscribe` for the primary then one
  `Subscribe` per extra, all sharing the same `resume_lsn`.

**Additive over clean-replace.** The plan called for a clean `tables: Vec<>`
config field. We kept the additive `extra_tables` instead: only `crates/*` are
workspace members (`make ci`), but six out-of-workspace SDKs (tauri/kotlin/dotnet/
node/swift + the Flutter example) call the existing single-table shape at ~17
sites. A clean replace would force an unmeasured cascade across SDKs this launch
does not touch; the additive field is zero-cascade and behaviorally identical.

### Synthetic-LSN cursor (the load-bearing correctness fix)

`PgSnapshotter` stamps per-table snapshot LSNs `base_lsn + 1 + i`
(`snapshot_source.rs`). On a shared sink, table B's snapshot collides with table
A's and the dedup ring (`router.rs`) drops it as a duplicate — so the second
subscribed table's pre-existing rows never arrive. Fix: a per-socket **monotonic
`synthetic_cursor`** seeded from the first subscribe's `resume_lsn`, passed as
`base_lsn` to each snapshot call, and advanced by the delivered row count. No two
events on the socket share an LSN. This is the bug the "locked" multi-table
design missed; found by primary-source reading of the snapshot + router code.

### Cap-exceed: reject-and-continue (recorded deviation)

A per-socket `MAX_TABLES_PER_SOCKET` (32) bounds snapshot cost. When a
mid-session `Subscribe` would exceed it, the server **rejects that one subscribe
and keeps the socket alive** (reject-and-continue) rather than closing the whole
connection. This is safe because `register_subscribe` returns *before* calling
`manager.connect`, so a rejected subscribe leaks no session; and the reader half
of the socket cannot cleanly force-close the writer half mid-stream. The
alternative (close-on-cap) would tear down a working multi-table session for one
bad subscribe.

### D2 — `disconnect()` / `resume()` (real pause/resume)

`disconnect()` aborts **only** the connect/apply/reconnect loop
(`Session.run_task`); it keeps the `SyncClient`, its `SqliteStorage`, and every
`watch()` pump alive — so reads, writes (→ durable outbox, ADR-0013), and the UI
keep working offline. `resume()` respawns the loop on the **same** client.

This rests on three verified properties of `nostos-client`:

- `run_once(&self)` takes `&self` (never `&mut`); all per-session state is local
  to the call and dropped on abort. The reusable fields are `Arc`/channel
  primitives (`engine: Arc<Mutex<ApplyEngine>>`, `changes: broadcast::Sender`,
  `write_notify: Notify`).
- `tokio::sync::Mutex` carries **no poison semantics**; abort at an `.await`
  releases any held guard. An in-flight `spawn_blocking` finishes independently.
- The outbox lives in `SqliteStorage` (the same SQLite file), flushed at session
  startup (`client.rs`) — so it drains on `resume()`.

So the SAME `SyncClient` is reusable across an abort → respawn; no rebuild, no
storage reopen, and the live `watch` pumps (which hold the client's
`subscribe_changes()`) keep firing.

`Session` stashes `config: SyncClientConfig` (the spawn moves the original) so
`resume()` can rebuild the loop without reconstructing the client. `run_task` is
`Option<JoinHandle>` (present while running, `None` while paused).

**Named `resume`, not `connect`.** `connect` clashes in Rust (E0592 vs the
`NostosHandle::connect` constructor) **and** Dart (`static Nostos.connect` /
`NostosDatabase.connect` / `factory RustNostosEngine.connect` all forbid an
instance member of the same name). `resume` matches WS5's "Pause/Resume" name and
is clash-free at every layer.

### Dashboard against the ratified (shipped) API

The Provider Dashboard (`archive/sdk/nostos_flutter/example` — archived
2026-07-30, reference only; `archive/` removed in cleanup, see git history;
the live copy is `sdk/nostos_flutter/example`) is built against the
**shipped** surface — `NostosDatabase.connect` → `subscribeTables` →
`watchMapped<T>('SELECT * FROM <table>', fromRow)` → `write` →
`disconnect`/`resume`. The plan's D5 names belong to the unratified
connection-redesign. The app's structure (5 tables, `NavigationRail`, CRUD,
connection panel) is API-agnostic, so a future D5 port is a mechanical rename of
the call sites, not a rebuild.

## Consequences

- **Positive:** N tables share one checkpoint (correct under ADR-0009), one
  socket, one ack stream — minimal wire + state. Pause/resume is real
  (transport-level, not a dead-URL hack): offline writes queue in the durable
  outbox and flush on resume; the UI never blocks on connectivity.
- **Positive:** The synthetic-LSN cursor makes multi-table snapshots correct;
  without it the second+ table's rows silently never arrive.
- **Negative:** `extra_tables` is an additive wart (primary + extras) rather than
  a clean `tables: Vec<>`. Acceptable until a coordinated SDK rename; flagged
  here so it is a deliberate debt, not an accident.
- **Negative:** `disconnect()` cannot observe outbox-flush completion (no
  `writes`/ack event stream in the shipped API), so the dashboard's
  "queued writes" counter is approximate (cleared when `connectionState` returns
  to `connected`). A real ack/conflict stream is part of the proposed D5 surface.
- **Risk:** `resume` (not `connect`) diverges from the dashboard plan's literal
  API names. Documented so a D5 ratification can alias `connect` → `resume` if
  parity with the common `connect`/`disconnect` naming convention is later
  required.

## Alternatives considered

- **N independent sessions, each its own LSN.** Rejected: contradicts ADR-0009
  (one global checkpoint); a per-table LSN does not exist in the WAL model.
- **Multi-predicate per session (one `Subscribe` with `[(table, where_sql), …]`).**
  Rejected: requires a wire-version gate the snapshot + transport changes would
  have to honor; the single-predicate-per-session + shared-sink shape reuses the
  existing `SyncSession` machinery with no protocol bump.
- **`connect()` as the resume verb.** Rejected: name clash at every layer (above).
- **Rebuild the `SyncClient` on `resume()` (don't reuse).** Rejected: would break
  the live `watch` pumps (they hold the *old* client's `subscribe_changes()`),
  fighting the "keep UI working" contract, and would require reopening storage.
- **Clean-replace `tables: Vec<>`.** Rejected for this launch (cascade across 6
  out-of-workspace SDKs); revisitable when those SDKs are in scope.

## Verification

- `make ci` green (fmt + clippy `-D warnings` + workspace tests) after fixing two
  `doc_lazy_continuation` lints (`+`-as-list-marker in `TableSub` docs) that
  `cargo check` does not surface.
- `nostos-infra` WS-contract tests: 13 pass, including
  `multi_table_one_socket_receives_both_tables` (proves the 2nd subscribe
  registers on the shared sink, not ignored).
- `flutter test` + `flutter analyze` clean on the SDK and the example.
- **Deferred to P5 (live smoke):** runtime multi-table snapshot delivery, the
  synthetic-LSN-cursor behavior under real Postgres, and the
  disconnect→offline-write→resume→flush cycle.

## Related

- ADR-0009 (one global client checkpoint), ADR-0013 (collapsed-write outbox),
  ADR-0015 (FFI bridge strategy), ADR-0019 (typed payload mapping / WS2 views),
- `docs/plans/nostos-provider-dashboard-multitable.md` (proposed D5 surface —
  unratified),
- `crates/nostos-infra/src/transport.rs` (D1 + synthetic cursor + cap handling),
- `crates/nostos-client/src/client.rs` (`extra_tables`, `TableSub`,
  cancellation-safe `run_once`),
- `sdk/nostos_flutter/rust/src/api/nostos.rs` (`disconnect`/`resume`, stashed
  config, `Option<JoinHandle>`),
- `archive/sdk/nostos_flutter/example/` (Provider Dashboard — archived 2026-07-30,
  reference only; `archive/` removed in cleanup, see git history).
